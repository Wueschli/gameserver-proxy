//! `gsp` — game-agnostic game server reverse proxy.
//!
//! Phase 1: load a YAML config, start the TCP listeners and the health
//! checker, serve the admin API (`/healthz`, `/readyz`, `/metrics`, `/pools`),
//! reload on SIGHUP / file change, and shut down cleanly on SIGINT/SIGTERM.

mod admin;
mod discovery;
mod reload;
mod resolver;
mod sniffer_loader;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,

    /// Validate the config and exit without starting anything.
    #[arg(long)]
    check: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GSP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = gsp_config::load(&args.config)?;
    tracing::info!(
        config = %args.config.display(),
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

    // A configured sniffer plugin dir must load cleanly too (phase 9).
    let sniffers = match &cfg.sniffers {
        Some(sc) => Arc::new(
            sniffer_loader::build_sniffers(sc)
                .map_err(|e| anyhow::anyhow!("settings.sniffers: {e:#}"))?,
        ),
        None => Arc::new(gsp_core::sniff::Sniffers::default()),
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

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cfg, args.config, geo_db, sniffers))
}

async fn run(
    cfg: gsp_config::Config,
    config_path: PathBuf,
    geo_db: Option<Arc<gsp_core::GeoDb>>,
    sniffers: Arc<gsp_core::sniff::Sniffers>,
) -> anyhow::Result<()> {
    let prometheus = metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder()?;

    let resolvers = Arc::new(resolver::build_resolvers(&cfg)?);
    if !resolvers.is_empty() {
        tracing::info!(count = resolvers.len(), "external resolvers ready");
    }

    // Backend discovery (phase 8): build one source per pool with a `source`,
    // do a best-effort initial fetch so the first snapshot has real backends,
    // then let the runtime run a refresh task per source.
    let discovery = Arc::new(gsp_core::Discovery::new());
    let sources = discovery::build_sources(&cfg)?;
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

    let snapshot: Arc<Snapshot> =
        Snapshot::build_with_sources(&cfg, None, &gsp_core::BackendOverlay::new(), &discovery);
    let runtime = Runtime::start_with_discovery(
        snapshot,
        resolvers,
        geo_db,
        sniffers,
        discovery,
        sources,
        cfg.workers,
    );
    let handle = runtime.handle();
    metrics::gauge!(gsp_core::metrics_defs::CONFIG_VERSION).set(reload::unix_now());

    let admin = tokio::spawn(admin::serve(cfg.admin_listen, handle.clone(), prometheus));
    let reload = tokio::spawn(reload::run(config_path, handle));

    wait_for_shutdown().await;
    tracing::info!(
        grace_sec = cfg.shutdown_grace.as_secs(),
        "shutdown signal received; draining in-flight connections"
    );

    runtime.shutdown_with_grace(cfg.shutdown_grace).await;
    reload.abort();
    admin.abort();
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
