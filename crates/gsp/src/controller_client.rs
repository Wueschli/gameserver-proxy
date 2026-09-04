//! `--controller <url>` config source (`docs/10` "The controller"; phase
//! 10+11 slice 3 in `docs/08-roadmap.md`). Replaces the file `--config` +
//! `reload::run` pair: [`fetch_current`] gets the first revision at startup
//! (used the same way `gsp_config::load` is for file mode), then [`run`]
//! holds a long-lived `GET /config/subscribe?since=<cursor>` connection,
//! feeding every accepted revision through the **same**
//! `reload::apply_config` a file reload uses.
//!
//! A dropped connection reconnects with capped exponential backoff from the
//! last cursor — this is the proxy side of `docs/10` principle 4
//! ("freeze on last-known-good, never clear"): the last-applied snapshot
//! keeps running the whole time, nothing here ever clears it.
//!
//! This is a **single `standalone` controller** client — no hierarchy, no
//! failover between controllers (`docs/10` "Fleet topology" is phase 12).

use std::sync::Arc;
use std::time::Duration;

use gsp_core::metrics_defs as m;
use gsp_core::sniff::Sniffers;
use gsp_core::{Resolvers, RuntimeHandle};

use crate::sniffer_loader::SnifferLoader;

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// `X-Config-Revision` — set by `gsp-controller`'s `GET /config`
/// (`crates/gsp-controller/src/api.rs`).
const REVISION_HEADER: &str = "x-config-revision";

/// One-shot `GET /config` — the initial revision + config text, fetched
/// before anything else exists to build a `Snapshot` from (mirrors
/// `gsp_config::load` in file mode).
pub async fn fetch_current(base_url: &str) -> anyhow::Result<(u64, String)> {
    let resp = reqwest::get(format!("{base_url}/config"))
        .await
        .map_err(|e| anyhow::anyhow!("fetching initial config from {base_url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "controller {base_url} returned {} for GET /config (has any config ever been submitted to it?)",
            resp.status()
        );
    }
    let revision: u64 = resp
        .headers()
        .get(REVISION_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| {
            anyhow::anyhow!("controller {base_url} response missing {REVISION_HEADER}")
        })?;
    let text = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("reading initial config body from {base_url}: {e}"))?;
    Ok((revision, text))
}

/// Runs forever, reconnecting with backoff on disconnect. `tokio::spawn`ed
/// like `reload::run`; never returns under normal operation.
#[allow(clippy::too_many_arguments)] // mirrors reload::apply_config's shape; a struct doesn't earn its keep for one call site
pub async fn run(
    base_url: String,
    since: u64,
    handle: RuntimeHandle,
    resolvers: Arc<Resolvers>,
    sniffer_loader: Option<Arc<SnifferLoader>>,
    sniffers: Arc<Sniffers>,
) {
    // Owned by this loop and passed by `&mut` into `subscribe_once` so a
    // revision applied just before a mid-stream disconnect is still
    // reflected in the reconnect's `?since=` — `subscribe_once` returning
    // `Err` must not roll the cursor back to where the connection started.
    let mut cursor = since;
    let mut backoff = RECONNECT_MIN;
    loop {
        match subscribe_once(
            &base_url,
            &mut cursor,
            &handle,
            &resolvers,
            sniffer_loader.as_deref(),
            &sniffers,
        )
        .await
        {
            Ok(()) => {
                backoff = RECONNECT_MIN;
                tracing::warn!(
                    controller = %base_url,
                    cursor,
                    "controller subscribe stream ended; reconnecting"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e, controller = %base_url, cursor,
                    "controller subscribe connection failed; keeping the last known \
                     configuration and retrying"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

/// Holds one subscribe connection until it drops, advancing `*cursor` as
/// revisions are applied. `*cursor` reflects every revision seen even when
/// this returns `Err` partway through a batch — the caller must reconnect
/// from wherever the connection actually got to, never from where it
/// started (see the comment in `run`).
async fn subscribe_once(
    base_url: &str,
    cursor: &mut u64,
    handle: &RuntimeHandle,
    resolvers: &Resolvers,
    sniffer_loader: Option<&SnifferLoader>,
    sniffers: &Sniffers,
) -> anyhow::Result<()> {
    let since = *cursor;
    let url = format!("{base_url}/config/subscribe?since={since}");
    let mut resp = reqwest::get(&url)
        .await
        .map_err(|e| anyhow::anyhow!("connecting to {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("controller {url} returned {}", resp.status());
    }
    tracing::info!(controller = %base_url, since, "subscribed to controller config updates");

    let mut buf = String::new();

    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| anyhow::anyhow!("reading subscribe stream from {base_url}: {e}"))?;
        let Some(bytes) = chunk else {
            return Ok(()); // server closed the stream
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));

        // SSE frames one event per blank-line-terminated block; a
        // keep-alive is a comment block with no `data:` line and is simply
        // ignored by `apply_sse_event`.
        while let Some(end) = buf.find("\n\n") {
            let event = buf[..end].to_string();
            buf.drain(..end + 2);
            if let Some(revision) =
                apply_sse_event(&event, handle, resolvers, sniffer_loader, sniffers).await
            {
                *cursor = revision;
            }
        }
    }
}

struct SseRevision {
    revision: u64,
    config: String,
}

/// Parses one SSE event block (`gsp-controller`'s `Event::default().data(...)`
/// shape: `data: {"revision":N,"config":"..."}`). `None` for anything that
/// isn't a data event — a keep-alive comment block, or a malformed one —
/// which the caller just ignores rather than treating as a protocol error:
/// the connection is still healthy, there's simply nothing to apply.
fn parse_sse_event(event: &str) -> Option<SseRevision> {
    let data_line = event
        .split('\n')
        .find_map(|line| line.strip_prefix("data:"))?
        .trim_start();

    let payload: serde_json::Value = serde_json::from_str(data_line).ok()?;
    Some(SseRevision {
        revision: payload.get("revision")?.as_u64()?,
        config: payload.get("config")?.as_str()?.to_string(),
    })
}

/// Applies one parsed revision through the same pipeline a file reload uses,
/// and returns the revision number so the caller can advance its cursor —
/// **even when the config was rejected**: an invalid revision must not be
/// replayed forever on every reconnect, the same way a bad file reload
/// doesn't retry itself, it just waits for the next trigger. Returns `None`
/// for anything that wasn't a data event at all (see [`parse_sse_event`]).
async fn apply_sse_event(
    event: &str,
    handle: &RuntimeHandle,
    resolvers: &Resolvers,
    sniffer_loader: Option<&SnifferLoader>,
    sniffers: &Sniffers,
) -> Option<u64> {
    let SseRevision { revision, config } = parse_sse_event(event)?;

    match gsp_config::parse_str(&config) {
        Ok(cfg) => {
            crate::reload::apply_config(
                cfg,
                handle,
                resolvers,
                sniffer_loader,
                sniffers,
                &format!("controller revision {revision}"),
            )
            .await;
        }
        Err(e) => {
            metrics::counter!(m::CONFIG_RELOAD, "result" => "failed").increment(1);
            tracing::error!(
                revision, error = %e,
                "controller pushed an invalid config revision; keeping current configuration"
            );
        }
    }
    Some(revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_data_event() {
        let event = r#"data: {"revision":7,"config":"pools: []"}"#;
        let parsed = parse_sse_event(event).unwrap();
        assert_eq!(parsed.revision, 7);
        assert_eq!(parsed.config, "pools: []");
    }

    #[test]
    fn a_keep_alive_comment_block_is_not_a_data_event() {
        // axum's `Sse::keep_alive` sends a bare comment line, no `data:`.
        assert!(parse_sse_event(": keep-alive").is_none());
    }

    #[test]
    fn malformed_json_is_ignored_not_a_panic() {
        assert!(parse_sse_event("data: not json").is_none());
    }

    #[test]
    fn a_data_event_missing_a_required_field_is_ignored() {
        assert!(parse_sse_event(r#"data: {"revision":1}"#).is_none());
        assert!(parse_sse_event(r#"data: {"config":"x"}"#).is_none());
    }
}
