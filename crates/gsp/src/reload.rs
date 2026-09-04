//! Hot configuration reload.
//!
//! A `SIGHUP` or a change to the config file rebuilds the snapshot and swaps it
//! in atomically. Pool membership, balancer, health-check and capacity changes
//! take effect live; changes to a listener's bind / protocol / pool mapping
//! need a restart and are logged as a warning.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::Notify;

use gsp_core::metrics_defs as m;
use gsp_core::sniff::Sniffers;
use gsp_core::{Resolvers, RuntimeHandle, Snapshot};

use crate::sniffer_loader::SnifferLoader;

/// Debounce window to coalesce a burst of editor writes into one reload.
const DEBOUNCE: Duration = Duration::from_millis(200);

pub async fn run(
    path: PathBuf,
    handle: RuntimeHandle,
    resolvers: Arc<Resolvers>,
    sniffer_loader: Option<Arc<SnifferLoader>>,
    sniffers: Arc<Sniffers>,
) {
    let trigger = Arc::new(Notify::new());
    // Kept alive for the lifetime of this task; dropping it stops the watch.
    let _watcher = spawn_watcher(&path, trigger.clone())
        .inspect_err(
            |e| tracing::warn!(error = %e, "config file watch unavailable; SIGHUP-only reload"),
        )
        .ok();

    #[cfg(unix)]
    {
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("install SIGHUP handler");
        let admin = handle.reload_requested().clone();
        loop {
            tokio::select! {
                _ = sighup.recv() => {}
                _ = trigger.notified() => {}
                _ = admin.notified() => {}
            }
            tokio::time::sleep(DEBOUNCE).await;
            // Coalesce a burst: swallow any trigger that landed during the
            // debounce window so it doesn't cause a second redundant reload.
            let _ = tokio::time::timeout(Duration::ZERO, trigger.notified()).await;
            apply(
                &path,
                &handle,
                &resolvers,
                sniffer_loader.as_deref(),
                &sniffers,
            )
            .await;
        }
    }

    #[cfg(not(unix))]
    {
        let admin = handle.reload_requested().clone();
        loop {
            tokio::select! {
                _ = trigger.notified() => {}
                _ = admin.notified() => {}
            }
            tokio::time::sleep(DEBOUNCE).await;
            // Coalesce a burst: swallow any trigger that landed during the
            // debounce window so it doesn't cause a second redundant reload.
            let _ = tokio::time::timeout(Duration::ZERO, trigger.notified()).await;
            apply(
                &path,
                &handle,
                &resolvers,
                sniffer_loader.as_deref(),
                &sniffers,
            )
            .await;
        }
    }
}

async fn apply(
    path: &Path,
    handle: &RuntimeHandle,
    resolvers: &Resolvers,
    sniffer_loader: Option<&SnifferLoader>,
    sniffers: &Sniffers,
) {
    match gsp_config::load(path) {
        Ok(cfg) => {
            let prev = handle.current();
            let listeners_changed = cfg.listeners != prev.listeners;
            let resolvers_changed = cfg.resolvers != prev.resolvers;
            handle.store(Snapshot::build_with_sources(
                &cfg,
                Some(&prev),
                handle.backend_overlay(),
                handle.discovery(),
            ));
            metrics::counter!(m::CONFIG_RELOAD, "result" => "ok").increment(1);
            metrics::gauge!(m::CONFIG_VERSION).set(unix_now());
            if listeners_changed {
                let (running, stopped) = handle.reconcile_listeners().await;
                tracing::info!(
                    running, stopped,
                    "listener definitions changed; listeners reconciled (added / removed / rebound)"
                );
            }
            if resolvers_changed {
                rebuild_resolvers(&cfg, resolvers);
            }
            rescan_sniffers(&cfg, sniffer_loader, sniffers);
            tracing::info!(config = %path.display(), "configuration reloaded");
        }
        Err(e) => {
            metrics::counter!(m::CONFIG_RELOAD, "result" => "failed").increment(1);
            tracing::error!(error = %e, "config reload failed; keeping current configuration");
        }
    }
}

/// Rebuild the external-resolver clients from the new `resolvers:` and swap the
/// whole set in (`Resolvers::replace`). Only called when `ResolverConfig`
/// actually changed, so a plain reload / discovery tick / overlay edit never
/// drops a `CachedResolver`'s LRU cache. A build error (bad endpoint URL) keeps
/// the previous set, same spirit as `rescan_sniffers`; the snapshot has already
/// swapped so pool / routing changes still land.
fn rebuild_resolvers(cfg: &gsp_config::Config, resolvers: &Resolvers) {
    match crate::resolver::build_resolvers(cfg) {
        Ok(map) => {
            let n = map.len();
            resolvers.replace(map);
            tracing::info!(count = n, "external resolvers rebuilt (caches reset)");
        }
        Err(e) => tracing::error!(
            error = %e,
            "resolver rebuild failed; keeping the previous resolver set"
        ),
    }
}

/// Phase 9 slice 4: `settings.sniffers.dir` is rescanned on every reload — an
/// added module loads, a removed one drops, a changed one recompiles (its
/// hash no longer matches whatever's already registered under that name, so
/// it's simply a fresh `WasmSniffer`). The engine itself (hence
/// `call_timeout_ms` / `max_memory_bytes`) is startup-only, like
/// `settings.workers` — `sniffer_loader` is `None` unless `settings.sniffers`
/// was present at process start, and enabling/disabling the block itself
/// still needs a restart.
fn rescan_sniffers(
    cfg: &gsp_config::Config,
    sniffer_loader: Option<&SnifferLoader>,
    sniffers: &Sniffers,
) {
    match (sniffer_loader, &cfg.sniffers) {
        (Some(loader), Some(sc)) => match loader.scan(sc) {
            Ok(map) => {
                let n = map.len();
                sniffers.replace(map);
                tracing::info!(count = n, dir = %sc.dir, "sniffer plugins rescanned");
            }
            Err(e) => tracing::error!(
                error = %e,
                "sniffer plugin rescan failed; keeping the previous plugin set"
            ),
        },
        (None, Some(_)) | (Some(_), None) => tracing::warn!(
            "settings.sniffers presence changed; a restart is needed for that to take effect"
        ),
        (None, None) => {}
    }
}

fn spawn_watcher(path: &Path, trigger: Arc<Notify>) -> notify::Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            trigger.notify_one();
        }
    })?;
    // Watch the parent directory so atomic-rename saves are seen; fall back to
    // the file itself when there is no parent component.
    let target = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf());
    watcher.watch(&target, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// Unix timestamp (whole seconds) for the `gsp_config_version` gauge.
pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as f64)
        .unwrap_or(0.0)
}
