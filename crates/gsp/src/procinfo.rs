//! Process-level OS metrics (`gsp_fd_open` / `gsp_fd_limit`, `docs/06`): the
//! process's own open file descriptor count and its `RLIMIT_NOFILE`. Lives in
//! the `gsp` binary, not `gsp-core` — this is host/ops observability, not
//! data-plane state, and needs no seam.

use std::time::Duration;

/// Count of currently open file descriptors for this process, via
/// `/proc/self/fd`. `None` off Linux (no such directory).
#[cfg(target_os = "linux")]
pub fn open_fd_count() -> Option<usize> {
    std::fs::read_dir("/proc/self/fd")
        .ok()
        .map(std::iter::Iterator::count)
}

#[cfg(not(target_os = "linux"))]
pub fn open_fd_count() -> Option<usize> {
    None
}

/// This process's `RLIMIT_NOFILE` soft limit — the cap `open_fd_count()` is
/// actually bounded by (the hard limit only matters for raising it).
pub fn fd_limit() -> Option<u64> {
    nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
        .ok()
        .map(|(soft, _hard)| soft)
}

/// Spawn a background task sampling `open_fd_count()` onto `gsp_fd_open`
/// every `interval`. Detached: like the `admin`/`reload` tasks, it is
/// `.abort()`ed on shutdown rather than joined — it holds no socket or lock
/// to release, so there's nothing to drain.
pub fn spawn_fd_gauge(interval: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if let Some(n) = open_fd_count() {
                metrics::gauge!(gsp_core::metrics_defs::FD_OPEN).set(n as f64);
            }
        }
    })
}
