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
use gsp_core::{RuntimeHandle, Snapshot};

/// Debounce window to coalesce a burst of editor writes into one reload.
const DEBOUNCE: Duration = Duration::from_millis(200);

pub async fn run(path: PathBuf, handle: RuntimeHandle) {
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
        loop {
            tokio::select! {
                _ = sighup.recv() => {}
                _ = trigger.notified() => {}
            }
            tokio::time::sleep(DEBOUNCE).await;
            // Coalesce a burst: swallow any trigger that landed during the
            // debounce window so it doesn't cause a second redundant reload.
            let _ = tokio::time::timeout(Duration::ZERO, trigger.notified()).await;
            apply(&path, &handle).await;
        }
    }

    #[cfg(not(unix))]
    {
        loop {
            trigger.notified().await;
            tokio::time::sleep(DEBOUNCE).await;
            // Coalesce a burst: swallow any trigger that landed during the
            // debounce window so it doesn't cause a second redundant reload.
            let _ = tokio::time::timeout(Duration::ZERO, trigger.notified()).await;
            apply(&path, &handle).await;
        }
    }
}

async fn apply(path: &Path, handle: &RuntimeHandle) {
    match gsp_config::load(path) {
        Ok(cfg) => {
            let prev = handle.current();
            let listeners_changed = cfg.listeners != prev.listeners;
            handle.store(Snapshot::build(&cfg, Some(&prev)));
            metrics::counter!(m::CONFIG_RELOAD, "result" => "ok").increment(1);
            metrics::gauge!(m::CONFIG_VERSION).set(unix_now());
            if listeners_changed {
                tracing::warn!(
                    "listener definitions changed; bind / protocol / pool-mapping changes need a \
                     restart (pool membership, balancer and health-check changes are already live)"
                );
            }
            tracing::info!(config = %path.display(), "configuration reloaded");
        }
        Err(e) => {
            metrics::counter!(m::CONFIG_RELOAD, "result" => "failed").increment(1);
            tracing::error!(error = %e, "config reload failed; keeping current configuration");
        }
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
