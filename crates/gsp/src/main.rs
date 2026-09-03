//! `gsp` — game-agnostic game server reverse proxy.
//!
//! Phase 1: load a YAML config, start the TCP listeners and the health
//! checker, serve the admin API (`/healthz`, `/readyz`, `/metrics`, `/pools`),
//! reload on SIGHUP / file change, and shut down cleanly on SIGINT/SIGTERM.

mod admin;
mod reload;

use std::path::PathBuf;
use std::sync::Arc;

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

    if args.check {
        println!(
            "config OK: {} listener(s), {} pool(s)",
            cfg.listeners.len(),
            cfg.pools.len()
        );
        return Ok(());
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cfg, args.config))
}

async fn run(cfg: gsp_config::Config, config_path: PathBuf) -> anyhow::Result<()> {
    let prometheus = metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder()?;

    let snapshot: Arc<Snapshot> = Snapshot::from_config(&cfg);
    let runtime = Runtime::start(snapshot, cfg.workers);
    let handle = runtime.handle();
    metrics::gauge!(gsp_core::metrics_defs::CONFIG_VERSION).set(reload::unix_now());

    let admin = tokio::spawn(admin::serve(cfg.admin_listen, handle.clone(), prometheus));
    let reload = tokio::spawn(reload::run(config_path, handle));

    wait_for_shutdown().await;
    tracing::info!("shutdown signal received; draining");

    runtime.shutdown().await;
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
