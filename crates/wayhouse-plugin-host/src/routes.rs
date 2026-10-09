//! The `routes` capability: which hostnames a plugin may claim and where they may point.
//!
//! A plugin hands the host its whole route set in one `routes_set` call; the host checks
//! every entry against what the operator approved and the controller materialises the set
//! (docs/plugins.md "Routes"). Operator routes always win: plugin routes are only ever
//! appended after them.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};

/// Most hostname patterns a `routes` declaration may list.
pub const MAX_ROUTE_PATTERNS: usize = 16;
/// Most backend networks a `routes` declaration may list.
pub const MAX_ROUTE_NETWORKS: usize = 16;
/// Host ceiling on `routes.max_entries`.
pub const MAX_ROUTE_ENTRIES: usize = 256;

/// What a plugin may route.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCap {
    /// Lowercase hostname patterns the plugin may claim: an exact name or `*.suffix`.
    pub hosts: Vec<String>,
    /// Networks (`ip` or `ip/prefix`) its backends must be in.
    pub backends: Vec<String>,
    /// Most entries one route set may hold.
    pub max_entries: usize,
}

/// One route: a hostname (or `*.suffix` pattern) and the backend it goes to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteEntry {
    pub host: String,
    pub backend: String,
}

/// A parsed `ip/prefix` network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Net {
    base: IpAddr,
    prefix: u8,
}

impl Net {
    /// `10.0.0.0/8`, `2001:db8::/32`, or a bare address (a host network).
    pub fn parse(s: &str) -> Result<Self, String> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let base: IpAddr = addr
            .parse()
            .map_err(|_| format!("{s:?} is not an IP address or network"))?;
        let max = if base.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("{s:?} has an invalid prefix length"))?,
            None => max,
        };
        Ok(Self { base, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.base, ip) {
            (IpAddr::V4(b), IpAddr::V4(x)) => bits(&b.octets(), &x.octets(), self.prefix),
            (IpAddr::V6(b), IpAddr::V6(x)) => bits(&b.octets(), &x.octets(), self.prefix),
            _ => false,
        }
    }

    /// Whether every address of `other` is in `self`.
    pub fn covers(&self, other: &Net) -> bool {
        self.prefix <= other.prefix && self.contains(other.base)
    }
}

fn bits(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let full = (prefix / 8) as usize;
    if a[..full] != b[..full] {
        return false;
    }
    let rem = prefix % 8;
    rem == 0 || {
        let mask = 0xffu8 << (8 - rem);
        (a[full] & mask) == (b[full] & mask)
    }
}

/// A valid route hostname: lowercase DNS labels, or `*.` followed by such a name.
pub fn valid_pattern(p: &str) -> bool {
    let name = p.strip_prefix("*.").unwrap_or(p);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Whether pattern `granted` covers everything `want` matches. `*.x` covers `a.x`,
/// `a.b.x` and `*.y.x`, not `x` itself.
pub fn pattern_covers(granted: &str, want: &str) -> bool {
    if granted == want {
        return true;
    }
    match granted.strip_prefix("*.") {
        Some(suffix) => {
            let want = want.strip_prefix("*.").unwrap_or(want);
            want.len() > suffix.len() && want.ends_with(&format!(".{suffix}"))
        }
        None => false,
    }
}

impl RouteCap {
    /// Shape checks on a declaration.
    pub fn validate(&self) -> Result<(), String> {
        if self.hosts.is_empty() || self.hosts.len() > MAX_ROUTE_PATTERNS {
            return Err(format!(
                "routes.hosts needs 1 to {MAX_ROUTE_PATTERNS} entries"
            ));
        }
        if let Some(bad) = self.hosts.iter().find(|h| !valid_pattern(h)) {
            return Err(format!(
                "routes host {bad:?}: use a lowercase DNS name or *.suffix"
            ));
        }
        if self.backends.is_empty() || self.backends.len() > MAX_ROUTE_NETWORKS {
            return Err(format!(
                "routes.backends needs 1 to {MAX_ROUTE_NETWORKS} entries"
            ));
        }
        for n in &self.backends {
            Net::parse(n)?;
        }
        if self.max_entries == 0 || self.max_entries > MAX_ROUTE_ENTRIES {
            return Err(format!(
                "routes.max_entries must be 1 to {MAX_ROUTE_ENTRIES}"
            ));
        }
        Ok(())
    }

    /// Whether this declaration asks for nothing `approved` does not grant.
    pub fn within(&self, approved: &RouteCap) -> bool {
        let nets: Vec<Net> = approved
            .backends
            .iter()
            .filter_map(|n| Net::parse(n).ok())
            .collect();
        self.max_entries <= approved.max_entries
            && self
                .hosts
                .iter()
                .all(|h| approved.hosts.iter().any(|a| pattern_covers(a, h)))
            && self.backends.iter().all(|n| {
                Net::parse(n)
                    .ok()
                    .is_some_and(|want| nets.iter().any(|a| a.covers(&want)))
            })
    }

    /// Checks one route set against the capability: the count, each entry's hostname and
    /// backend, and that no hostname appears twice.
    pub fn check_set(&self, set: &[RouteEntry]) -> Result<(), String> {
        if set.len() > self.max_entries {
            return Err(format!(
                "{} routes, above the cap of {}",
                set.len(),
                self.max_entries
            ));
        }
        let nets: Vec<Net> = self
            .backends
            .iter()
            .filter_map(|n| Net::parse(n).ok())
            .collect();
        let mut seen = BTreeSet::new();
        for e in set {
            if !valid_pattern(&e.host) {
                return Err(format!("route host {:?} is not a valid hostname", e.host));
            }
            if !self.hosts.iter().any(|g| pattern_covers(g, &e.host)) {
                return Err(format!(
                    "route host {} is outside the approved hosts",
                    e.host
                ));
            }
            let addr: SocketAddr = e
                .backend
                .parse()
                .map_err(|_| format!("route backend {:?} is not ip:port", e.backend))?;
            if addr.port() == 0 {
                return Err(format!("route backend {} has port 0", e.backend));
            }
            if !nets.iter().any(|n| n.contains(addr.ip())) {
                return Err(format!(
                    "route backend {} is outside the approved networks",
                    e.backend
                ));
            }
            if !seen.insert(e.host.as_str()) {
                return Err(format!("route host {} appears twice", e.host));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap() -> RouteCap {
        RouteCap {
            hosts: vec!["*.mc.example.com".into(), "play.example.com".into()],
            backends: vec!["10.0.0.0/16".into()],
            max_entries: 2,
        }
    }

    fn e(host: &str, backend: &str) -> RouteEntry {
        RouteEntry {
            host: host.into(),
            backend: backend.into(),
        }
    }

    #[test]
    fn suffix_patterns_cover_deeper_names_but_not_the_apex() {
        assert!(pattern_covers("*.a.com", "x.a.com"));
        assert!(pattern_covers("*.a.com", "x.y.a.com"));
        assert!(pattern_covers("*.a.com", "*.y.a.com"));
        assert!(!pattern_covers("*.a.com", "a.com"));
        assert!(!pattern_covers("*.a.com", "xa.com"));
        assert!(!pattern_covers("a.com", "*.a.com"));
        assert!(pattern_covers("a.com", "a.com"));
    }

    #[test]
    fn a_set_inside_the_capability_passes() {
        let c = cap();
        c.validate().unwrap();
        c.check_set(&[
            e("one.mc.example.com", "10.0.3.4:25565"),
            e("play.example.com", "10.0.0.1:1"),
        ])
        .unwrap();
    }

    #[test]
    fn entries_outside_the_capability_are_refused() {
        let c = cap();
        for (set, why) in [
            (
                vec![e("evil.example.com", "10.0.0.1:1")],
                "outside the approved hosts",
            ),
            (
                vec![e("a.mc.example.com", "192.168.0.1:1")],
                "outside the approved networks",
            ),
            (vec![e("a.mc.example.com", "10.0.0.1")], "not ip:port"),
            (vec![e("a.mc.example.com", "10.0.0.1:0")], "port 0"),
            (
                vec![e("A.mc.example.com", "10.0.0.1:1")],
                "not a valid hostname",
            ),
            (
                vec![
                    e("a.mc.example.com", "10.0.0.1:1"),
                    e("a.mc.example.com", "10.0.0.2:1"),
                ],
                "twice",
            ),
            (
                vec![
                    e("a.mc.example.com", "10.0.0.1:1"),
                    e("b.mc.example.com", "10.0.0.1:1"),
                    e("c.mc.example.com", "10.0.0.1:1"),
                ],
                "above the cap",
            ),
        ] {
            let err = c.check_set(&set).unwrap_err();
            assert!(err.contains(why), "{err} should mention {why}");
        }
    }

    #[test]
    fn a_declaration_must_be_within_what_was_approved() {
        let approved = cap();
        let mut want = cap();
        assert!(want.within(&approved));
        want.hosts = vec!["x.mc.example.com".into()];
        want.backends = vec!["10.0.1.0/24".into()];
        assert!(want.within(&approved));
        for widen in [
            |c: &mut RouteCap| c.hosts.push("other.example.com".into()),
            |c: &mut RouteCap| c.backends = vec!["10.0.0.0/8".into()],
            |c: &mut RouteCap| c.max_entries = 3,
        ] {
            let mut w = cap();
            widen(&mut w);
            assert!(!w.within(&approved));
        }
    }

    #[test]
    fn malformed_declarations_are_refused() {
        for bad in [
            RouteCap {
                hosts: vec![],
                ..cap()
            },
            RouteCap {
                hosts: vec!["*.*.a.com".into()],
                ..cap()
            },
            RouteCap {
                hosts: vec!["UP.a.com".into()],
                ..cap()
            },
            RouteCap {
                backends: vec![],
                ..cap()
            },
            RouteCap {
                backends: vec!["10.0.0.0/33".into()],
                ..cap()
            },
            RouteCap {
                max_entries: 0,
                ..cap()
            },
            RouteCap {
                max_entries: MAX_ROUTE_ENTRIES + 1,
                ..cap()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
