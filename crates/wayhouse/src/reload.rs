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

use wayhouse_core::metrics_defs as m;
use wayhouse_core::sniff::Sniffers;
use wayhouse_core::{Resolvers, RuntimeHandle, Snapshot};

use crate::sniffer_loader::SnifferLoader;

/// Quiet period the trigger must stay silent for before a reload runs.
const DEBOUNCE: Duration = Duration::from_millis(200);
/// Upper bound on how long a continuous stream of triggers can postpone a reload.
const MAX_DEBOUNCE: Duration = Duration::from_secs(2);

/// Trailing-edge debounce: waits until `trigger` has been quiet for `quiet`, so
/// a burst of file events spaced wider than one window still coalesces into a
/// single reload. `max` caps the total wait so a steady stream can't starve it.
async fn settle(trigger: &Notify, quiet: Duration, max: Duration) {
    let deadline = tokio::time::Instant::now() + max;
    loop {
        let wait = quiet.min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        if tokio::time::timeout(wait, trigger.notified())
            .await
            .is_err()
        {
            return;
        }
    }
}

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
            settle(&trigger, DEBOUNCE, MAX_DEBOUNCE).await;
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
            settle(&trigger, DEBOUNCE, MAX_DEBOUNCE).await;
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
    match wayhouse_config::load(path) {
        Ok(cfg) => {
            apply_config(
                cfg,
                handle,
                resolvers,
                sniffer_loader,
                sniffers,
                &format!("file {}", path.display()),
            )
            .await
        }
        Err(e) => {
            metrics::counter!(m::CONFIG_RELOAD, "result" => "failed").increment(1);
            tracing::error!(error = %e, "config reload failed; keeping current configuration");
        }
    }
}

/// Rebuilds the snapshot from an already-parsed-and-validated [`wayhouse_config::Config`]
/// and reconciles everything that reads it — the shared tail of a file
/// reload (`apply`, above) and a controller-pushed revision
/// (`controller_client::run`, phase 10+11 slice 3). Splitting this out means
/// both triggers get the exact same rebuild/reconcile behavior; only how the
/// `Config` was obtained (and validated — the caller ran `wayhouse_config::load` /
/// `parse_str` before this) differs.
pub(crate) async fn apply_config(
    cfg: wayhouse_config::Config,
    handle: &RuntimeHandle,
    resolvers: &Resolvers,
    sniffer_loader: Option<&SnifferLoader>,
    sniffers: &Sniffers,
    source_desc: &str,
) {
    let prev = handle.current();
    let listeners_changed = cfg.listeners != prev.listeners;
    let resolvers_changed = cfg.resolvers != prev.resolvers;
    let next = Snapshot::build_with_sources(
        &cfg,
        Some(&prev),
        handle.backend_overlay(),
        handle.discovery(),
    );
    let sources_changed = next.sources != prev.sources;
    handle.store(next);
    metrics::counter!(m::CONFIG_RELOAD, "result" => "ok").increment(1);
    metrics::gauge!(m::CONFIG_VERSION).set(unix_now());
    if listeners_changed {
        let out = handle.reconcile_listeners().await;
        tracing::info!(
            running = out.running,
            stopped = out.stopped,
            failed = out.failed.len(),
            "listener definitions changed; listeners reconciled (added / removed / rebound)"
        );
        for e in &out.failed {
            metrics::counter!(m::LISTENER_BIND_FAILURES, "listener" => e.listener.clone())
                .increment(1);
        }
    }
    if sources_changed {
        let (running, stopped) = handle.reconcile_sources().await;
        tracing::info!(
            running,
            stopped,
            "backend_sources changed; discovery refresh tasks reconciled \
             (added / removed / restarted)"
        );
    }
    if resolvers_changed {
        rebuild_resolvers(&cfg, resolvers);
    }
    rescan_sniffers(&cfg, sniffer_loader, sniffers);
    tracing::info!(source = %source_desc, "configuration reloaded");
}

/// Rebuild the external-resolver clients from the new `resolvers:` and swap the
/// whole set in (`Resolvers::replace`). Only called when `ResolverConfig`
/// actually changed, so a plain reload / discovery tick / overlay edit never
/// drops a `CachedResolver`'s LRU cache. A build error (bad endpoint URL) keeps
/// the previous set, same spirit as `rescan_sniffers`; the snapshot has already
/// swapped so pool / routing changes still land.
fn rebuild_resolvers(cfg: &wayhouse_config::Config, resolvers: &Resolvers) {
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
    cfg: &wayhouse_config::Config,
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

/// Unix timestamp (whole seconds) for the `wayhouse_config_version` gauge.
pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as f64)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn settle_coalesces_events_spaced_inside_the_quiet_period() {
        let n = Arc::new(Notify::new());
        let n2 = n.clone();
        tokio::spawn(async move {
            // Events 150 ms apart: each is within one 200 ms window of the last.
            for _ in 0..4 {
                tokio::time::sleep(Duration::from_millis(150)).await;
                n2.notify_one();
            }
        });
        let start = tokio::time::Instant::now();
        settle(&n, Duration::from_millis(200), Duration::from_secs(2)).await;
        // Last event at 600 ms, then 200 ms of quiet.
        assert_eq!(start.elapsed(), Duration::from_millis(800));
    }

    #[tokio::test(start_paused = true)]
    async fn settle_is_capped_by_max() {
        let n = Arc::new(Notify::new());
        let n2 = n.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                n2.notify_one();
            }
        });
        let start = tokio::time::Instant::now();
        settle(&n, Duration::from_millis(200), Duration::from_secs(1)).await;
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }
}
