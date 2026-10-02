//! `gsp` — game-agnostic game server reverse proxy.
//!
//! Phase 1: load a YAML config, start the TCP listeners and the health
//! checker, serve the admin API (`/healthz`, `/readyz`, `/metrics`, `/pools`),
//! reload on SIGHUP / file change, and shut down cleanly on SIGINT/SIGTERM.

mod admin;
mod aggregator_client;
mod controller_client;
mod discovery;
mod intent_client;
mod procinfo;
mod proxy_register;
mod reload;
mod resolver;
mod sniffer_loader;
mod tunnel_address;
mod tunnel_client;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use gsp_core::{Runtime, Snapshot};

#[derive(Parser, Debug)]
#[command(
    name = "gsp",
    version,
    about = "Game-agnostic game server reverse proxy"
)]
struct Args {
    /// Path to the YAML config file.
    #[arg(
        short,
        long,
        default_value = "config.yaml",
        conflicts_with = "controller"
    )]
    config: PathBuf,

    /// Fleet controller base URL (e.g. "http://127.0.0.1:9901") to pull
    /// structural config from instead of `--config`'s file (docs/10 "The
    /// controller"). Phase 10+11 PoC: one standalone controller, no HA or
    /// hierarchy yet — see docs/08-roadmap.md phase 10+11.
    #[arg(long)]
    controller: Option<String>,

    /// Bearer token to present to `--controller`, if it requires one (its
    /// own `--auth-token`).
    #[arg(long)]
    controller_token: Option<String>,

    /// Validate the config and exit without starting anything. Works with
    /// `--controller` too — fetches its current config and validates that.
    #[arg(long)]
    check: bool,

    /// Fleet aggregator base URL (e.g. "http://127.0.0.1:9902") to push
    /// periodic fleet-state summaries to (docs/10 "The aggregator").
    /// Optional and independent of `--controller` — pushing state and
    /// pulling config are unrelated axes.
    #[arg(long)]
    aggregator: Option<String>,

    /// Self-reported instance name in aggregator pushes. Defaults to this
    /// instance's `settings.admin.listen` address, which is already unique
    /// per running instance.
    #[arg(long)]
    aggregator_instance: Option<String>,

    /// Seconds between pushes to `--aggregator`.
    #[arg(long, default_value = "10")]
    aggregator_interval_sec: u64,

    /// Bearer token to present on every push to `--aggregator`, if it
    /// requires one (its own `--auth-token`).
    #[arg(long)]
    aggregator_token: Option<String>,

    /// Enables phase 14's WireGuard backend transport (`docs/11`):
    /// brings up a local interface with this name and reconciles its peer
    /// list from `--tunnel-controller-url`'s backend-peers registry.
    /// Requires `--tunnel-controller-url`. Omit to
    /// leave this feature off entirely — today's exact behavior otherwise.
    #[arg(long)]
    tunnel_iface: Option<String>,

    /// UDP port the tunnel interface listens on.
    #[arg(long, default_value_t = 51820)]
    tunnel_listen_port: u16,

    /// Optional. This proxy's own tunnel-internal address as `ip/prefix`
    /// (e.g. `10.60.0.1/24`): pins that address with the controller (the
    /// prefix is used only when the controller reports no network). Omit to
    /// let the controller allocate one.
    #[arg(long)]
    tunnel_address: Option<String>,

    /// File holding this proxy's persisted WireGuard private key (created
    /// if missing).
    #[arg(long, default_value = "gsp-tunnel.key")]
    tunnel_key_file: PathBuf,

    /// Base URL of the `gsp-controller` backend-peers registry to subscribe
    /// to — independent of `--controller` (a deployment may pull structural
    /// config from a file while still using a controller's tunnel registry,
    /// or vice versa). Required with `--tunnel-iface`.
    #[arg(long)]
    tunnel_controller_url: Option<String>,

    /// Bearer token for `--tunnel-controller-url`, if it requires one.
    #[arg(long)]
    tunnel_controller_token: Option<String>,

    /// Skip the kernel WireGuard backend and use boringtun userspace
    /// directly — see `gsp-agent --userspace`'s doc for why.
    #[arg(long)]
    tunnel_userspace: bool,

    /// This proxy's stable identity in `--tunnel-controller-url`'s
    /// proxy-peers registry (phase 14 slice 7) — arbitrary, just needs to
    /// be unique fleet-wide. Required with `--tunnel-iface`.
    #[arg(long)]
    tunnel_name: Option<String>,

    /// This proxy's public dial-out address to register (`ip:port`) — every
    /// origin's `gsp-agent` learns it from here and peers with it. Required
    /// (not optional like an origin's `--endpoint`): `docs/11`'s whole
    /// premise is that only the proxy side needs a stable public address.
    #[arg(long)]
    tunnel_endpoint: Option<String>,

    /// How often to re-register with `--tunnel-controller-url`'s
    /// proxy-peers registry.
    #[arg(long, default_value_t = 30)]
    tunnel_register_interval_sec: u64,
}

/// Resolved `--tunnel-*` settings, built once in `async_main` after
/// validating the flag combination — `run` doesn't need to re-check
/// `tunnel_address`/`tunnel_controller_url` are `Some` a second time.
struct TunnelConfig {
    iface: String,
    listen_port: u16,
    address: Option<String>,
    key_file: PathBuf,
    controller_url: String,
    controller_token: Option<String>,
    userspace: bool,
    name: String,
    endpoint: String,
    register_interval: Duration,
}

/// Where this process's config comes from, decided once at startup from
/// `Args`. `reload`/`controller_client` each own the live-update side of one
/// variant; nothing else branches on this after `run` dispatches on it once.
enum ConfigSource {
    File(PathBuf),
    Controller(String, Option<String>),
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GSP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Fetching from a controller needs an async runtime, so config loading
    // itself now happens inside `block_on` rather than before it (file mode
    // is unaffected — `gsp_config::load` is still a plain sync read).
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main(args))
}

async fn async_main(args: Args) -> anyhow::Result<()> {
    let tunnel_config = match &args.tunnel_iface {
        Some(iface) => {
            let controller_url = args.tunnel_controller_url.clone().ok_or_else(|| {
                anyhow::anyhow!("--tunnel-iface requires --tunnel-controller-url")
            })?;
            let name = args
                .tunnel_name
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--tunnel-iface requires --tunnel-name"))?;
            let endpoint = args
                .tunnel_endpoint
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--tunnel-iface requires --tunnel-endpoint"))?;
            Some(TunnelConfig {
                iface: iface.clone(),
                listen_port: args.tunnel_listen_port,
                address: args.tunnel_address.clone(),
                key_file: args.tunnel_key_file.clone(),
                controller_url,
                controller_token: args.tunnel_controller_token.clone(),
                userspace: args.tunnel_userspace,
                name,
                endpoint,
                register_interval: Duration::from_secs(args.tunnel_register_interval_sec),
            })
        }
        None => None,
    };

    let (config_source, cfg, initial_revision) = match &args.controller {
        Some(url) => {
            let (revision, text) =
                controller_client::fetch_current(url, args.controller_token.as_deref()).await?;
            let cfg = gsp_config::parse_str(&text)
                .map_err(|e| anyhow::anyhow!("controller {url} revision {revision}: {e}"))?;
            (
                ConfigSource::Controller(url.clone(), args.controller_token.clone()),
                cfg,
                Some(revision),
            )
        }
        None => {
            let cfg = gsp_config::load(&args.config)?;
            (ConfigSource::File(args.config.clone()), cfg, None)
        }
    };
    tracing::info!(
        source = match &config_source {
            ConfigSource::File(p) => p.display().to_string(),
            ConfigSource::Controller(u, _) => u.clone(),
        },
        listeners = cfg.listeners.len(),
        pools = cfg.pools.len(),
        "configuration loaded"
    );

    // A configured GeoIP DB must load, both for `--check` and at startup.
    let geo_db = match &cfg.geo_db {
        Some(path) => Some(
            gsp_core::GeoDb::open(path)
                .map_err(|e| anyhow::anyhow!("settings.geo_db {path:?}: {e}"))?,
        ),
        None => None,
    };

    // A configured sniffer plugin dir must load cleanly too (phase 9). The
    // loader (its wasmtime engine + epoch-ticker thread) is kept alive and
    // handed to the reload task so a later `dir` rescan reuses it instead of
    // spawning a fresh ticker thread per reload.
    let (sniffer_loader, sniffers) = match &cfg.sniffers {
        Some(sc) => {
            let (loader, registry) = sniffer_loader::build_sniffers(sc)
                .map_err(|e| anyhow::anyhow!("settings.sniffers: {e:#}"))?;
            (Some(Arc::new(loader)), Arc::new(registry))
        }
        None => (None, Arc::new(gsp_core::sniff::Sniffers::default())),
    };

    if args.check {
        println!(
            "config OK: {} listener(s), {} pool(s){}{}",
            cfg.listeners.len(),
            cfg.pools.len(),
            if geo_db.is_some() {
                ", geo_db loaded"
            } else {
                ""
            },
            match &cfg.sniffers {
                Some(sc) => format!(", sniffers loaded from {}", sc.dir),
                None => String::new(),
            },
        );
        return Ok(());
    }

    let aggregator_push = args
        .aggregator
        .map(|base_url| aggregator_client::PushConfig {
            base_url,
            instance: args
                .aggregator_instance
                .unwrap_or_else(|| cfg.admin_listen.to_string()),
            admin_url: format!("http://{}", cfg.admin_listen),
            interval: Duration::from_secs(args.aggregator_interval_sec),
            token: args.aggregator_token,
        });

    run(
        cfg,
        config_source,
        initial_revision,
        geo_db,
        sniffer_loader,
        sniffers,
        aggregator_push,
        tunnel_config,
    )
    .await
}

#[allow(clippy::too_many_arguments)] // mirrors controller_client::run's shape; a struct doesn't earn its keep for one call site
async fn run(
    cfg: gsp_config::Config,
    config_source: ConfigSource,
    initial_revision: Option<u64>,
    geo_db: Option<Arc<gsp_core::GeoDb>>,
    sniffer_loader: Option<Arc<sniffer_loader::SnifferLoader>>,
    sniffers: Arc<gsp_core::sniff::Sniffers>,
    aggregator_push: Option<aggregator_client::PushConfig>,
    tunnel_config: Option<TunnelConfig>,
) -> anyhow::Result<()> {
    let prometheus = metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder()?;

    let resolvers = Arc::new(gsp_core::Resolvers::from_map(resolver::build_resolvers(
        &cfg,
    )?));
    if !resolvers.is_empty() {
        tracing::info!(count = resolvers.len(), "external resolvers ready");
    }

    // Phase 14 slice 5 (docs/11): a `tunnel` backend_sources entry resolves
    // an origin's currently-registered backends from the same backend-peers
    // registry the tunnel reconcile task (below) subscribes to — reuses
    // `--tunnel-controller-url`/`--tunnel-controller-token` rather than a
    // second pair of flags, since it's the identical registry. Borrowed
    // (not moved) here so the bring-up block further down can still
    // consume `tunnel_config` by value.
    let tunnel_registry = tunnel_config.as_ref().map(|tc| discovery::TunnelRegistry {
        controller_url: tc.controller_url.clone(),
        token: tc.controller_token.clone(),
    });

    // Backend discovery (phase 8): build one source per pool with a `source`,
    // do a best-effort initial fetch so the first snapshot has real backends,
    // then let the runtime run a refresh task per source.
    let discovery = Arc::new(gsp_core::Discovery::new());
    let sources = discovery::build_sources(&cfg, tunnel_registry.as_ref())?;
    if !sources.is_empty() {
        tracing::info!(count = sources.len(), "backend discovery sources ready");
        for s in &sources {
            match tokio::time::timeout(Duration::from_secs(5), s.fetch()).await {
                Ok(Ok(addrs)) if !addrs.is_empty() => {
                    discovery.store(s.pool(), addrs);
                }
                Ok(Ok(_)) => tracing::warn!(
                    pool = s.pool(),
                    "initial discovery returned no addresses; starting from the seed"
                ),
                Ok(Err(e)) => tracing::warn!(
                    pool = s.pool(), error = %e,
                    "initial discovery failed; starting from the seed"
                ),
                Err(_) => tracing::warn!(
                    pool = s.pool(),
                    "initial discovery timed out; starting from the seed"
                ),
            }
        }
    }

    // After the best-effort initial fetch, hand a factory (not the prebuilt
    // sources) to the runtime: its `SourceManager` spawns one refresh task per
    // pool `source` and reconciles them on every reload.
    let source_factory: Option<Arc<dyn gsp_core::SourceFactory>> =
        cfg.pools.iter().any(|p| p.source.is_some()).then(|| {
            let f: Arc<dyn gsp_core::SourceFactory> =
                Arc::new(discovery::DiscoveryFactory::new(tunnel_registry.clone()));
            f
        });

    // Phase 14 slice 4 (docs/11): bring up the shared WireGuard interface
    // and start reconciling its peer list from the backend-peers registry,
    // before any listener binds — a failure here is loud, not silent, same
    // posture as `geo_db`/`sniffers` loading earlier: a misconfigured
    // `--tunnel-*` flag set shouldn't start a proxy that silently never
    // forwards tunneled traffic, and shouldn't leave listeners bound behind
    // a startup error either.
    let tunnel = match tunnel_config {
        Some(tc) => {
            let private_key = tunnel_client::load_or_generate_key(&tc.key_file)
                .with_context(|| format!("loading tunnel key from {:?}", tc.key_file))?;
            let pubkey = private_key.public_key().to_string();

            // The controller is the address authority: register BEFORE the
            // interface exists (the answer is its address) and before any
            // listener binds — a failure here is a failing `--tunnel-*`.
            let pinned_cidr = tc.address.clone();
            if let Some(c) = pinned_cidr.as_deref() {
                tunnel_address::ip_of(c)
                    .parse::<std::net::Ipv4Addr>()
                    .with_context(|| format!("--tunnel-address {c:?} must be an IPv4 ip/prefix"))?;
            }
            let reg = proxy_register::Registration {
                name: tc.name.clone(),
                pubkey,
                endpoint: tc.endpoint.clone(),
                address: pinned_cidr
                    .as_deref()
                    .map(|c| tunnel_address::ip_of(c).to_string()),
            };
            let client = proxy_register::http_client();
            let addr_path = {
                let mut p = tc.key_file.clone().into_os_string();
                p.push(".address");
                PathBuf::from(p)
            };
            let outcome = proxy_register::register_with_retry(
                &client,
                &tc.controller_url,
                tc.controller_token.as_deref(),
                &reg,
                Duration::from_secs(30),
            )
            .await;
            let start = tunnel_address::resolve_startup(
                outcome,
                pinned_cidr.as_deref(),
                tunnel_address::load(&addr_path),
            )?;
            match start.source {
                tunnel_address::Source::Controller => {
                    tunnel_address::save(&addr_path, &start.cidr)?
                }
                tunnel_address::Source::Saved => tracing::warn!(
                    address = %start.cidr,
                    "controller unreachable; starting with the last saved tunnel address"
                ),
            }
            let address: defguard_wireguard_rs::net::IpAddrMask = start
                .cidr
                .parse()
                .map_err(|e| anyhow::anyhow!("tunnel address {:?} is invalid: {e}", start.cidr))?;
            let wg: Arc<dyn defguard_wireguard_rs::WireguardInterfaceApi + Send + Sync> =
                Arc::from(tunnel_client::bring_up(
                    &tc.iface,
                    &private_key,
                    tc.listen_port,
                    address,
                    tc.userspace,
                )?);
            tracing::info!(
                iface = %tc.iface,
                port = tc.listen_port,
                address = %start.cidr,
                pubkey = %private_key.public_key(),
                controller = %tc.controller_url,
                "wireguard tunnel interface up; subscribing to backend-peers updates"
            );
            let task = tokio::spawn(tunnel_client::run(
                tc.controller_url.clone(),
                tc.controller_token.clone(),
                wg.clone(),
            ));
            // Register ourselves (periodically) so every origin's `gsp-agent`
            // can peer with us — the mirror image of `task` above.
            let register_task = tokio::spawn(proxy_register::run(
                client,
                tc.controller_url,
                tc.controller_token,
                reg,
                tc.register_interval,
                tunnel_address::ip_of(&start.cidr).to_string(),
            ));
            Some((task, register_task, wg))
        }
        None => None,
    };

    let snapshot: Arc<Snapshot> =
        Snapshot::build_with_sources(&cfg, None, &gsp_core::BackendOverlay::new(), &discovery);
    let runtime = Runtime::start_with_discovery(
        snapshot,
        resolvers.clone(),
        geo_db,
        sniffers.clone(),
        discovery,
        source_factory,
        cfg.gossip.clone(),
        cfg.workers,
    );
    let handle = runtime.handle();
    metrics::gauge!(gsp_core::metrics_defs::CONFIG_VERSION).set(reload::unix_now());
    // Build identity + fd headroom (docs/06 "Planned / not yet built", now
    // built): version/commit are fixed for the process lifetime, so
    // `gsp_build_info` and `gsp_fd_limit` are set once; `gsp_fd_open` needs a
    // live sample, hence the periodic task.
    metrics::gauge!(
        gsp_core::metrics_defs::BUILD_INFO,
        "version" => env!("CARGO_PKG_VERSION"),
        "commit" => env!("GSP_GIT_SHA"),
    )
    .set(1.0);
    if let Some(limit) = procinfo::fd_limit() {
        metrics::gauge!(gsp_core::metrics_defs::FD_LIMIT).set(limit as f64);
    }
    let fd_gauge = procinfo::spawn_fd_gauge(Duration::from_secs(5));

    let admin = tokio::spawn(admin::serve(
        cfg.admin_listen,
        handle.clone(),
        prometheus,
        cfg.admin_auth_token.clone(),
        cfg.sniffers.as_ref().map(|sc| PathBuf::from(&sc.dir)),
        sniffers.clone(),
    ));
    let aggregator = aggregator_push.map(|push_cfg| {
        tracing::info!(
            aggregator = %push_cfg.base_url,
            instance = %push_cfg.instance,
            interval_sec = push_cfg.interval.as_secs(),
            "pushing fleet-state summaries to aggregator"
        );
        tokio::spawn(aggregator_client::run(push_cfg, handle.clone()))
    });
    // The intent log (phase 12 slice 3) lives on the same controller
    // instance as structural config, so it only makes sense to subscribe
    // alongside `--controller` — an "unrelated axis" the other way from
    // `aggregator_client` (that one is independent of `--controller`; this
    // one is a second stream off the *same* connection target).
    let intent = if let ConfigSource::Controller(url, token) = &config_source {
        tracing::info!(controller = %url, "subscribing to controller intent updates");
        Some(tokio::spawn(intent_client::run(
            url.clone(),
            token.clone(),
            handle.clone(),
        )))
    } else {
        None
    };

    // See `controller_client::watch_admin_reloads`'s doc: in `--controller`
    // mode, nothing otherwise rebuilds the snapshot after an admin-API or
    // intent-op backend overlay change (`reload::run`, which normally does,
    // only runs in file-config mode).
    let admin_reload_watch = if let ConfigSource::Controller(url, token) = &config_source {
        Some(tokio::spawn(controller_client::watch_admin_reloads(
            url.clone(),
            token.clone(),
            handle.clone(),
            resolvers.clone(),
            sniffer_loader.clone(),
            sniffers.clone(),
        )))
    } else {
        None
    };

    let reload = match config_source {
        ConfigSource::File(path) => tokio::spawn(reload::run(
            path,
            handle,
            resolvers,
            sniffer_loader,
            sniffers,
        )),
        ConfigSource::Controller(url, token) => tokio::spawn(controller_client::run(
            url,
            token,
            initial_revision.unwrap_or(0),
            handle,
            resolvers,
            sniffer_loader,
            sniffers,
        )),
    };

    wait_for_shutdown().await;
    tracing::info!(
        grace_sec = cfg.shutdown_grace.as_secs(),
        "shutdown signal received; draining in-flight connections"
    );

    runtime.shutdown_with_grace(cfg.shutdown_grace).await;
    reload.abort();
    if let Some(intent) = intent {
        intent.abort();
    }
    if let Some(admin_reload_watch) = admin_reload_watch {
        admin_reload_watch.abort();
    }
    admin.abort();
    if let Some(aggregator) = aggregator {
        aggregator.abort();
    }
    fd_gauge.abort();
    if let Some((task, register_task, wg)) = tunnel {
        task.abort();
        register_task.abort();
        if let Err(e) = wg.remove_interface() {
            tracing::warn!(error = %e, "failed to remove the wireguard tunnel interface cleanly");
        }
    }
    tracing::info!("stopped");
    Ok(())
}

#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
