//! Validation: turns the raw YAML types into the resolved [`Config`].

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use crate::cidr::{Acl, Cidr, GeoAcl};
use crate::keys::base64_decode_32;
use crate::matcher::Matcher;
use crate::parse::*;
use crate::resolved::*;
use crate::schema::*;
use crate::ConfigError;

/// Internal: a `backend_sources[]` entry after validation — either folded to a
/// fixed list (`static`) or a runtime source spec (dynamic kinds).
enum ResolvedSource {
    Static(Vec<SocketAddr>),
    Dynamic(SourceConfig),
}

pub(crate) fn validate(raw: RawConfig) -> Result<Config, ConfigError> {
    use ConfigError::Invalid;

    if raw.listeners.is_empty() {
        return Err(Invalid("at least one listener is required".into()));
    }

    let admin_listen = raw.settings.admin.listen.parse().map_err(|_| {
        Invalid(format!(
            "settings.admin.listen is not a valid socket address: {}",
            raw.settings.admin.listen
        ))
    })?;
    let admin_auth_token = raw.settings.admin.auth_token.clone();
    let admin_tls = raw.settings.admin.tls.clone();

    // Resolve `backend_sources`. A `static` source becomes a fixed address list;
    // the dynamic kinds become a `SourceConfig` for the runtime refresh task.
    let mut source_names = BTreeSet::new();
    let mut sources: std::collections::HashMap<String, ResolvedSource> =
        std::collections::HashMap::with_capacity(raw.backend_sources.len());
    for s in raw.backend_sources {
        if !source_names.insert(s.name.clone()) {
            return Err(Invalid(format!(
                "duplicate backend_sources name: {}",
                s.name
            )));
        }
        if s.refresh_interval_sec == 0 {
            return Err(Invalid(format!(
                "backend_sources {}: refresh_interval_sec must be > 0",
                s.name
            )));
        }
        let refresh_interval = Duration::from_secs(s.refresh_interval_sec);
        let resolved = match s.kind.as_str() {
            "static" => {
                if s.targets.is_empty() {
                    return Err(Invalid(format!(
                        "backend_sources {}: type static needs a non-empty targets list",
                        s.name
                    )));
                }
                let mut addrs = Vec::with_capacity(s.targets.len());
                for t in &s.targets {
                    addrs.push(t.parse().map_err(|_| {
                        Invalid(format!(
                            "backend_sources {}: target is not a valid socket address: {t}",
                            s.name
                        ))
                    })?);
                }
                ResolvedSource::Static(addrs)
            }
            "dns_srv" => {
                let record = s.record.clone().ok_or_else(|| {
                    Invalid(format!(
                        "backend_sources {}: type dns_srv needs `record`",
                        s.name
                    ))
                })?;
                if record.is_empty() {
                    return Err(Invalid(format!(
                        "backend_sources {}: `record` must not be empty",
                        s.name
                    )));
                }
                ResolvedSource::Dynamic(SourceConfig {
                    name: s.name.clone(),
                    kind: SourceKind::DnsSrv { record },
                    refresh_interval,
                })
            }
            "consul" => {
                let service = s.service.clone().ok_or_else(|| {
                    Invalid(format!(
                        "backend_sources {}: type consul needs `service`",
                        s.name
                    ))
                })?;
                ResolvedSource::Dynamic(SourceConfig {
                    name: s.name.clone(),
                    kind: SourceKind::Consul {
                        service,
                        addr: s
                            .consul_addr
                            .clone()
                            .unwrap_or_else(|| "http://127.0.0.1:8500".to_string()),
                        tag: s.tag.clone(),
                    },
                    refresh_interval,
                })
            }
            "kubernetes" => {
                let service = s.service.clone().ok_or_else(|| {
                    Invalid(format!(
                        "backend_sources {}: type kubernetes needs `service`",
                        s.name
                    ))
                })?;
                ResolvedSource::Dynamic(SourceConfig {
                    name: s.name.clone(),
                    kind: SourceKind::Kubernetes {
                        namespace: s.namespace.clone().unwrap_or_else(|| "default".to_string()),
                        service,
                        port_name: s.port_name.clone(),
                        api: s
                            .api
                            .clone()
                            .unwrap_or_else(|| "https://kubernetes.default.svc".to_string()),
                    },
                    refresh_interval,
                })
            }
            "tunnel" => {
                let pubkey = s.pubkey.clone().ok_or_else(|| {
                    Invalid(format!(
                        "backend_sources {}: type tunnel needs `pubkey`",
                        s.name
                    ))
                })?;
                if base64_decode_32(&pubkey).is_none() {
                    return Err(Invalid(format!(
                        "backend_sources {}: `pubkey` must be a base64-encoded \
                         32-byte WireGuard key",
                        s.name
                    )));
                }
                ResolvedSource::Dynamic(SourceConfig {
                    name: s.name.clone(),
                    kind: SourceKind::Tunnel { pubkey },
                    refresh_interval,
                })
            }
            other => {
                return Err(Invalid(format!(
                    "backend_sources {}: unknown type {other:?} \
                     (static | dns_srv | consul | kubernetes | tunnel)",
                    s.name
                )))
            }
        };
        sources.insert(s.name, resolved);
    }

    let mut pool_names = BTreeSet::new();
    let mut pools = Vec::with_capacity(raw.pools.len());
    for p in raw.pools {
        if !pool_names.insert(p.name.clone()) {
            return Err(Invalid(format!("duplicate pool name: {}", p.name)));
        }
        // Backends come from either a static `targets` list or a named `source`.
        let (targets, source) = match &p.source {
            Some(_) if !p.targets.is_empty() => {
                return Err(Invalid(format!(
                    "pool {}: `targets` and `source` are mutually exclusive",
                    p.name
                )))
            }
            Some(src_name) => match sources.get(src_name) {
                None => {
                    return Err(Invalid(format!(
                        "pool {}: unknown backend_sources name: {src_name}",
                        p.name
                    )))
                }
                Some(ResolvedSource::Static(addrs)) => (addrs.clone(), None),
                Some(ResolvedSource::Dynamic(sc)) => (Vec::new(), Some(sc.clone())),
            },
            None => {
                if p.targets.is_empty() {
                    return Err(Invalid(format!(
                        "pool {}: needs `targets` or `source`",
                        p.name
                    )));
                }
                let mut targets = Vec::with_capacity(p.targets.len());
                for t in &p.targets {
                    let addr = t.parse().map_err(|_| {
                        Invalid(format!(
                            "pool {}: target is not a valid socket address: {t}",
                            p.name
                        ))
                    })?;
                    targets.push(addr);
                }
                (targets, None)
            }
        };
        if p.connect_timeout_ms == 0 {
            return Err(Invalid(format!(
                "pool {}: connect_timeout_ms must be > 0",
                p.name
            )));
        }

        let hc = &p.health_check;
        let hc_kind = match hc.kind.as_str() {
            "tcp_connect" => {
                if hc.send_hex.is_some() || hc.expect_hex_prefix.is_some() {
                    return Err(Invalid(format!(
                        "pool {}: send_hex / expect_hex_prefix only apply to health_check.type \
                         udp_probe",
                        p.name
                    )));
                }
                HealthCheckKind::TcpConnect
            }
            "udp_probe" => {
                let send = match &hc.send_hex {
                    Some(s) => parse_hex(s).map_err(|e| {
                        Invalid(format!("pool {}: health_check.send_hex: {e}", p.name))
                    })?,
                    None => {
                        return Err(Invalid(format!(
                            "pool {}: health_check.type udp_probe requires send_hex",
                            p.name
                        )))
                    }
                };
                if send.is_empty() {
                    return Err(Invalid(format!(
                        "pool {}: health_check.send_hex must not be empty",
                        p.name
                    )));
                }
                let expect_prefix = match &hc.expect_hex_prefix {
                    Some(s) => parse_hex(s).map_err(|e| {
                        Invalid(format!(
                            "pool {}: health_check.expect_hex_prefix: {e}",
                            p.name
                        ))
                    })?,
                    None => Vec::new(),
                };
                HealthCheckKind::UdpProbe {
                    send,
                    expect_prefix,
                }
            }
            other => {
                return Err(Invalid(format!(
                    "pool {}: health_check.type {other:?} is not supported \
                     (tcp_connect | udp_probe)",
                    p.name
                )))
            }
        };
        if hc.interval_sec == 0 || hc.timeout_ms == 0 {
            return Err(Invalid(format!(
                "pool {}: health_check interval_sec and timeout_ms must be > 0",
                p.name
            )));
        }
        if hc.rise == 0 || hc.fall == 0 {
            return Err(Invalid(format!(
                "pool {}: health_check rise and fall must be >= 1",
                p.name
            )));
        }
        if let Some(0) = p.per_backend.max_sessions {
            return Err(Invalid(format!(
                "pool {}: per_backend.max_sessions must be > 0 when set",
                p.name
            )));
        }

        let hash_on = match (p.balancer, p.hash_on) {
            (Balancer::ConsistentHash, on) => Some(on.unwrap_or(HashOn::SrcIp)),
            (_, Some(_)) => {
                return Err(Invalid(format!(
                    "pool {}: hash_on applies only to balancer consistent_hash",
                    p.name
                )))
            }
            (_, None) => None,
        };

        if !p.weights.is_empty() && p.balancer != Balancer::Weighted {
            return Err(Invalid(format!(
                "pool {}: weights applies only to balancer weighted",
                p.name
            )));
        }
        let mut weights = std::collections::HashMap::with_capacity(p.weights.len());
        for (addr, w) in &p.weights {
            if *w == 0 {
                return Err(Invalid(format!(
                    "pool {}: weight for {addr} must be >= 1",
                    p.name
                )));
            }
            let sa: SocketAddr = addr.parse().map_err(|_| {
                Invalid(format!(
                    "pool {}: weights key {addr:?} is not an ip:port",
                    p.name
                ))
            })?;
            weights.insert(sa, *w);
        }

        pools.push(PoolConfig {
            name: p.name,
            targets,
            source,
            balancer: p.balancer,
            hash_on,
            weights,
            connect_timeout: Duration::from_millis(p.connect_timeout_ms),
            idle_timeout: Duration::from_secs(p.idle_timeout_sec),
            health_check: HealthCheck {
                kind: hc_kind,
                interval: Duration::from_secs(hc.interval_sec),
                timeout: Duration::from_millis(hc.timeout_ms),
                rise: hc.rise,
                fall: hc.fall,
            },
            max_sessions: p.per_backend.max_sessions,
            proxy_protocol: p.proxy_protocol,
        });
    }

    let mut resolver_names = BTreeSet::new();
    let mut resolvers = Vec::with_capacity(raw.resolvers.len());
    for r in raw.resolvers {
        if !resolver_names.insert(r.name.clone()) {
            return Err(Invalid(format!("duplicate resolver name: {}", r.name)));
        }
        let kind = match r.kind.as_str() {
            "http" => ResolverKind::Http,
            "grpc" => ResolverKind::Grpc,
            other => {
                return Err(Invalid(format!(
                    "resolver {}: type {other:?} is not supported (http | grpc)",
                    r.name
                )))
            }
        };
        if r.endpoint.trim().is_empty() {
            return Err(Invalid(format!("resolver {}: endpoint is empty", r.name)));
        }
        if r.timeout_ms == 0 {
            return Err(Invalid(format!(
                "resolver {}: timeout_ms must be > 0",
                r.name
            )));
        }
        if r.target_connect_timeout_ms == 0 || r.target_idle_timeout_sec == 0 {
            return Err(Invalid(format!(
                "resolver {}: target_connect_timeout_ms and target_idle_timeout_sec must be > 0",
                r.name
            )));
        }
        let cache = match r.cache {
            Some(c) if !c.key.is_empty() => {
                let mut parts = Vec::with_capacity(c.key.len());
                for k in &c.key {
                    parts.push(parse_cache_key_part(&r.name, k)?);
                }
                if c.max_entries == 0 {
                    return Err(Invalid(format!(
                        "resolver {}: cache.max_entries must be > 0",
                        r.name
                    )));
                }
                Some(CacheConfig {
                    key: parts,
                    positive_ttl: Duration::from_secs(c.positive_ttl_sec),
                    negative_ttl: Duration::from_secs(c.negative_ttl_sec),
                    max_entries: c.max_entries,
                })
            }
            Some(_) => {
                return Err(Invalid(format!(
                    "resolver {}: cache.key must be non-empty",
                    r.name
                )))
            }
            None => None,
        };
        resolvers.push(ResolverConfig {
            name: r.name,
            kind,
            endpoint: r.endpoint,
            timeout: Duration::from_millis(r.timeout_ms),
            on_error: r.on_error,
            cache,
            proxy_protocol: r.proxy_protocol,
            target_connect_timeout: Duration::from_millis(r.target_connect_timeout_ms),
            target_idle_timeout: Duration::from_secs(r.target_idle_timeout_sec),
        });
    }

    let mut listener_names = BTreeSet::new();
    let mut binds = BTreeSet::new();
    let mut listeners = Vec::with_capacity(raw.listeners.len());
    for l in raw.listeners {
        if !listener_names.insert(l.name.clone()) {
            return Err(Invalid(format!("duplicate listener name: {}", l.name)));
        }
        let (bind, extra_binds) = parse_bind_spec(&l.name, &l.bind)?;
        for addr in std::iter::once(bind).chain(extra_binds.iter().copied()) {
            if !binds.insert((addr, l.protocol)) {
                return Err(Invalid(format!(
                    "listener {}: bind {addr} is already used by another listener",
                    l.name
                )));
            }
        }
        let routes = if !l.routes.is_empty() {
            if l.pool.is_some() {
                return Err(Invalid(format!(
                    "listener {}: set either `pool` or `routes`, not both",
                    l.name
                )));
            }
            let mut rs = Vec::with_capacity(l.routes.len());
            for (i, r) in l.routes.iter().enumerate() {
                let matcher = parse_matcher(&l.name, i, &r.r#match)?;
                let action = match (&r.action.pool, &r.action.resolver) {
                    (Some(p), None) => {
                        if !pool_names.contains(p) {
                            return Err(Invalid(format!(
                                "listener {}: route {i}: unknown pool {p}",
                                l.name
                            )));
                        }
                        Action::Pool(p.clone())
                    }
                    (None, Some(rn)) => {
                        if !resolver_names.contains(rn) {
                            return Err(Invalid(format!(
                                "listener {}: route {i}: unknown resolver {rn}",
                                l.name
                            )));
                        }
                        Action::Resolver(rn.clone())
                    }
                    _ => {
                        return Err(Invalid(format!(
                        "listener {}: route {i}: action needs exactly one of `pool` / `resolver`",
                        l.name
                    )))
                    }
                };
                rs.push(Route { matcher, action });
            }
            rs
        } else {
            let pool = l.pool.clone().ok_or_else(|| {
                Invalid(format!(
                    "listener {}: needs a `pool` or a `routes` list",
                    l.name
                ))
            })?;
            if !pool_names.contains(&pool) {
                return Err(Invalid(format!("listener {}: unknown pool {pool}", l.name)));
            }
            vec![Route {
                matcher: Matcher::Always,
                action: Action::Pool(pool),
            }]
        };
        if l.protocol == Protocol::Udp
            && routes.iter().any(|r| matches!(r.matcher, Matcher::Sni(_)))
        {
            return Err(Invalid(format!(
                "listener {}: the `sni` match requires a tcp listener",
                l.name
            )));
        }
        let affinity = match (l.protocol, l.affinity) {
            (Protocol::Tcp, Some(_)) => {
                return Err(Invalid(format!(
                    "listener {}: affinity applies only to udp listeners",
                    l.name
                )))
            }
            (Protocol::Tcp, None) => None,
            (Protocol::Udp, None) => Some(HashOn::default()),
            (Protocol::Udp, Some(a)) => Some(a.hash_on),
        };

        let prefix = match &l.prefix {
            Some(p) => {
                if l.protocol != Protocol::Udp {
                    return Err(Invalid(format!(
                        "listener {}: `prefix` mode requires a udp listener",
                        l.name
                    )));
                }
                if !extra_binds.is_empty() {
                    return Err(Invalid(format!(
                        "listener {}: `prefix` mode needs exactly one wildcard socket, not a \
                         bind port range",
                        l.name
                    )));
                }
                if !bind.ip().is_unspecified() {
                    return Err(Invalid(format!(
                        "listener {}: `prefix` mode needs a wildcard `bind` (e.g. \"[::]:{}\")",
                        l.name,
                        bind.port()
                    )));
                }
                let cidr = Cidr::parse(p)
                    .map_err(|e| Invalid(format!("listener {}: prefix: {e}", l.name)))?;
                Some(cidr)
            }
            None => None,
        };
        if l.freebind && l.protocol != Protocol::Tcp {
            return Err(Invalid(format!(
                "listener {}: `freebind` applies only to tcp listeners (udp uses `prefix`)",
                l.name
            )));
        }
        if l.transparent && l.prefix.is_some() {
            return Err(Invalid(format!(
                "listener {}: `transparent` and `prefix` are mutually exclusive (both derive \
                 the per-datagram destination, by different mechanisms)",
                l.name
            )));
        }
        if l.transparent {
            // A tunnel backend is reached over the WireGuard interface, whose address
            // family is the tunnel network's, not the client's; the transparent bind
            // would silently fall back to a plain connect on a family mismatch.
            // (A resolver route can still hand back a tunnel pool at runtime.)
            for r in &routes {
                let Action::Pool(name) = &r.action else {
                    continue;
                };
                let is_tunnel = pools.iter().any(|p| {
                    &p.name == name
                        && matches!(
                            p.source.as_ref().map(|s| &s.kind),
                            Some(SourceKind::Tunnel { .. })
                        )
                });
                if is_tunnel {
                    return Err(Invalid(format!(
                        "listener {}: `transparent` is unsupported with pool {name}, which \
                         uses a `tunnel` source (the client and tunnel address families can \
                         differ)",
                        l.name
                    )));
                }
            }
        }
        if l.first_packet_gate {
            if l.protocol != Protocol::Udp {
                return Err(Invalid(format!(
                    "listener {}: `first_packet_gate` applies only to udp listeners",
                    l.name
                )));
            }
            let has_gateable = routes.iter().any(|r| {
                matches!(
                    r.matcher,
                    Matcher::FirstBytes { .. } | Matcher::Sniffer { .. }
                )
            });
            if !has_gateable {
                return Err(Invalid(format!(
                    "listener {}: `first_packet_gate` needs at least one `first_bytes` route \
                     or a `sniffer` (otherwise it drops every datagram)",
                    l.name
                )));
            }
        }

        let parse_cidrs = |field: &str, raw: &[String]| -> Result<Vec<Cidr>, ConfigError> {
            raw.iter()
                .map(|s| {
                    Cidr::parse(s)
                        .map_err(|e| Invalid(format!("listener {}: {field}: {e}", l.name)))
                })
                .collect()
        };
        let acl = Acl::new(
            parse_cidrs("allow", &l.allow)?,
            parse_cidrs("deny", &l.deny)?,
        );

        let geo = match &l.geo {
            None => None,
            Some(g) => {
                if raw.settings.geo_db.is_none() {
                    return Err(Invalid(format!(
                        "listener {}: `geo` needs `settings.geo_db` to be set",
                        l.name
                    )));
                }
                if g.allow.is_empty() && g.deny.is_empty() {
                    return Err(Invalid(format!(
                        "listener {}: `geo` needs a non-empty `allow` or `deny`",
                        l.name
                    )));
                }
                let parse_ccs = |field: &str,
                                 raw: &[String]|
                 -> Result<Vec<[u8; 2]>, ConfigError> {
                    raw.iter()
                        .map(|s| {
                            let b = s.as_bytes();
                            if b.len() == 2 && b.iter().all(u8::is_ascii_alphabetic) {
                                Ok([b[0].to_ascii_uppercase(), b[1].to_ascii_uppercase()])
                            } else {
                                Err(Invalid(format!(
                                    "listener {}: geo.{field}: {s:?} is not a 2-letter country code",
                                    l.name
                                )))
                            }
                        })
                        .collect()
                };
                Some(GeoAcl {
                    allow: parse_ccs("allow", &g.allow)?,
                    deny: parse_ccs("deny", &g.deny)?,
                })
            }
        };

        let per_source = match &l.per_source {
            None => None,
            Some(ps) => {
                for (field, v) in [
                    ("max_per_ip", ps.max_per_ip),
                    ("max_per_net", ps.max_per_net),
                ] {
                    if v == Some(0) {
                        return Err(Invalid(format!(
                            "listener {}: per_source.{field} must be >= 1 (omit for no cap)",
                            l.name
                        )));
                    }
                }
                if ps.max_per_ip.is_none() && ps.max_per_net.is_none() {
                    return Err(Invalid(format!(
                        "listener {}: `per_source` needs `max_per_ip` and/or `max_per_net`",
                        l.name
                    )));
                }
                Some(PerSourceLimit {
                    max_per_ip: ps.max_per_ip,
                    max_per_net: ps.max_per_net,
                })
            }
        };

        let rate_limit = match l.rate_limit {
            None => None,
            Some(rl) => {
                let to_bucket = |b: &RawBucket, which: &str| -> Result<TokenBucket, ConfigError> {
                    if b.rate == 0 {
                        return Err(Invalid(format!(
                            "listener {}: rate_limit.{which}.rate must be >= 1",
                            l.name
                        )));
                    }
                    let burst = b.burst.unwrap_or(b.rate).max(1);
                    Ok(TokenBucket {
                        rate: b.rate,
                        burst,
                    })
                };
                let per_ip = rl
                    .per_ip
                    .as_ref()
                    .map(|b| to_bucket(b, "per_ip"))
                    .transpose()?;
                let per_net = rl
                    .per_net
                    .as_ref()
                    .map(|b| to_bucket(b, "per_net"))
                    .transpose()?;
                if per_ip.is_none() && per_net.is_none() {
                    return Err(Invalid(format!(
                        "listener {}: rate_limit needs at least one of `per_ip` / `per_net`",
                        l.name
                    )));
                }
                Some(RateLimit { per_ip, per_net })
            }
        };

        // At most one sniffer plugin per listener (gsp-core runs one per conn).
        let mut sniffer: Option<String> = None;
        for r in &routes {
            if let Matcher::Sniffer { name, .. } = &r.matcher {
                match &sniffer {
                    Some(prev) if prev != name => {
                        return Err(Invalid(format!(
                            "listener {}: routes use two different sniffers ({prev}, {name}); \
                             only one per listener is supported",
                            l.name
                        )));
                    }
                    _ => sniffer = Some(name.clone()),
                }
            }
        }

        listeners.push(ListenerConfig {
            name: l.name,
            bind,
            extra_binds,
            protocol: l.protocol,
            routes,
            affinity,
            prefix,
            freebind: l.freebind,
            transparent: l.transparent,
            sniffer,
            route_hint: l.route_hint,
            first_packet_gate: l.first_packet_gate,
            acl,
            geo,
            per_source,
            rate_limit,
        });
    }

    // `proxy_protocol` form must match the transport of the listeners that use
    // the pool: v1/v2 are TCP-only, v2-udp is UDP-only. A pool statically routed
    // from both (or from the wrong transport) is rejected. Pools reached only
    // through a resolver `action` are not checked here (the target pool is not
    // known until runtime); the data path falls back to sending no header on a
    // transport mismatch.
    for p in &pools {
        if p.proxy_protocol == ProxyProtocol::None {
            continue;
        }
        let (mut on_tcp, mut on_udp) = (false, false);
        for l in &listeners {
            let uses = l
                .routes
                .iter()
                .any(|r| matches!(&r.action, Action::Pool(n) if n == &p.name));
            if uses {
                match l.protocol {
                    Protocol::Tcp => on_tcp = true,
                    Protocol::Udp => on_udp = true,
                }
            }
        }
        let want_udp = p.proxy_protocol == ProxyProtocol::V2Udp;
        if want_udp && on_tcp {
            return Err(Invalid(format!(
                "pool {}: proxy_protocol v2-udp is used by a TCP listener",
                p.name
            )));
        }
        if !want_udp && on_udp {
            return Err(Invalid(format!(
                "pool {}: proxy_protocol {} is used by a UDP listener (use v2-udp)",
                p.name,
                p.proxy_protocol.label()
            )));
        }
    }

    // Same transport check for a resolver's `target` PROXY protocol form:
    // v1/v2 are TCP-only, v2-udp is UDP-only. A resolver reached from listeners
    // of both transports (or the wrong one) is rejected.
    for r in &resolvers {
        if r.proxy_protocol == ProxyProtocol::None {
            continue;
        }
        let (mut on_tcp, mut on_udp) = (false, false);
        for l in &listeners {
            let uses = l
                .routes
                .iter()
                .any(|rt| matches!(&rt.action, Action::Resolver(n) if n == &r.name));
            if uses {
                match l.protocol {
                    Protocol::Tcp => on_tcp = true,
                    Protocol::Udp => on_udp = true,
                }
            }
        }
        let want_udp = r.proxy_protocol == ProxyProtocol::V2Udp;
        if want_udp && on_tcp {
            return Err(Invalid(format!(
                "resolver {}: proxy_protocol v2-udp is used by a TCP listener",
                r.name
            )));
        }
        if !want_udp && on_udp {
            return Err(Invalid(format!(
                "resolver {}: proxy_protocol {} is used by a UDP listener (use v2-udp)",
                r.name,
                r.proxy_protocol.label()
            )));
        }
    }

    let rl = &raw.settings.limits;
    for (name, zero) in [
        ("max_connections", rl.max_connections == Some(0)),
        ("max_udp_sessions", rl.max_udp_sessions == Some(0)),
        (
            "max_new_sessions_per_sec",
            rl.max_new_sessions_per_sec == Some(0),
        ),
    ] {
        if zero {
            return Err(Invalid(format!(
                "settings.limits.{name}: 0 blocks all traffic; omit the key for no cap"
            )));
        }
    }
    let limits = GlobalLimits {
        max_connections: rl.max_connections,
        max_udp_sessions: rl.max_udp_sessions,
        max_new_sessions_per_sec: rl.max_new_sessions_per_sec,
    };

    let sniffers = match raw.settings.sniffers {
        Some(rs) => Some(validate_sniffers(rs)?),
        None => None,
    };

    let gossip = match (raw.settings.failure_domain.clone(), raw.settings.gossip) {
        (None, None) => None,
        (Some(_), None) => {
            return Err(Invalid(
                "settings.failure_domain is set but settings.gossip is missing".into(),
            ))
        }
        (None, Some(_)) => {
            return Err(Invalid(
                "settings.gossip is set but settings.failure_domain is missing".into(),
            ))
        }
        (Some(_), Some(rg)) => Some(validate_gossip(rg)?),
    };

    let group = match raw.settings.group {
        Some(g) => Some(validate_group(g)?),
        None => None,
    };

    Ok(Config {
        workers: raw.settings.workers,
        shutdown_grace: Duration::from_secs(raw.settings.shutdown_grace_sec),
        admin_listen,
        admin_auth_token,
        admin_tls,
        pools,
        resolvers,
        listeners,
        limits,
        geo_db: raw.settings.geo_db,
        sniffers,
        failure_domain: raw.settings.failure_domain,
        gossip,
        group,
    })
}

fn validate_group(g: String) -> Result<String, ConfigError> {
    use ConfigError::Invalid;
    if g.is_empty() || g.starts_with('/') || g.ends_with('/') {
        return Err(Invalid(
            "settings.group must not be empty or start/end with '/'".into(),
        ));
    }
    if g.split('/').any(str::is_empty) {
        return Err(Invalid(
            "settings.group must not contain empty segments (e.g. \"a//b\")".into(),
        ));
    }
    Ok(g)
}

fn validate_gossip(rg: RawGossip) -> Result<GossipConfig, ConfigError> {
    use ConfigError::Invalid;
    let bind = rg.bind.parse().map_err(|_| {
        Invalid(format!(
            "settings.gossip.bind is not a valid socket address: {}",
            rg.bind
        ))
    })?;
    let mut seeds = Vec::with_capacity(rg.seeds.len());
    for s in &rg.seeds {
        seeds.push(s.parse().map_err(|_| {
            Invalid(format!(
                "settings.gossip.seeds: not a valid socket address: {s}"
            ))
        })?);
    }
    if !(rg.quorum_fraction > 0.5 && rg.quorum_fraction <= 1.0) {
        return Err(Invalid(
            "settings.gossip.quorum_fraction must be > 0.5 and <= 1.0".into(),
        ));
    }
    if rg.psk.is_empty() {
        return Err(Invalid("settings.gossip.psk must not be empty".into()));
    }
    Ok(GossipConfig {
        bind,
        seeds,
        quorum_fraction: rg.quorum_fraction,
        psk: rg.psk,
    })
}

fn validate_sniffers(rs: RawSniffers) -> Result<SniffersConfig, ConfigError> {
    use ConfigError::Invalid;
    if rs.dir.trim().is_empty() {
        return Err(Invalid("settings.sniffers.dir must not be empty".into()));
    }
    if rs.call_timeout_ms == 0 {
        return Err(Invalid(
            "settings.sniffers.call_timeout_ms must be > 0".into(),
        ));
    }
    if rs.max_memory_bytes == 0 {
        return Err(Invalid(
            "settings.sniffers.max_memory_bytes must be > 0".into(),
        ));
    }
    let mut modules = Vec::with_capacity(rs.modules.len());
    for m in rs.modules {
        if m.name.trim().is_empty() {
            return Err(Invalid(
                "settings.sniffers.modules[].name must not be empty".into(),
            ));
        }
        let hex_ok = m.sha256.len() == 64 && m.sha256.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex_ok {
            return Err(Invalid(format!(
                "settings.sniffers.modules[{}].sha256 must be a 64-char hex digest",
                m.name
            )));
        }
        if matches!(&m.config, Some(c) if c.is_empty()) {
            return Err(Invalid(format!(
                "settings.sniffers.modules[{}].config must not be empty when set",
                m.name
            )));
        }
        modules.push(SnifferModulePin {
            name: m.name,
            sha256: m.sha256.to_ascii_lowercase(),
            config: m.config,
        });
    }
    Ok(SniffersConfig {
        dir: rs.dir,
        call_timeout: Duration::from_millis(rs.call_timeout_ms),
        max_memory_bytes: rs.max_memory_bytes,
        modules,
    })
}

#[cfg(test)]
mod tests;
