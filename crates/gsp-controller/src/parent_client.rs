//! The `slave`-role relay client (phase 12 slice 1, `docs/10` "Fleet
//! topology"): subscribes to a parent `gsp-controller` and re-lands every
//! revision it receives in this tier's own [`Store`] via
//! [`crate::api::AppState::apply_revision`] — the only caller allowed to
//! bypass the `slave` write gate in `crate::api::submit`.
//!
//! Deliberately mirrors `gsp`'s own `crates/gsp/src/controller_client.rs`
//! almost line for line (initial `GET /config`, then a long-lived `GET
//! /config/subscribe?since=<cursor>` with capped-backoff reconnect) — a
//! `slave` controller subscribes to its parent exactly the way a proxy
//! subscribes to a `standalone` one (`docs/10`: "this is principle 5 applied
//! recursively, not a new mechanism"). The wire shape is duplicated rather
//! than shared as a library: it's a few dozen lines of SSE framing, and
//! `gsp` has no reason to depend on `gsp-controller` (or vice versa).
//!
//! A dropped parent connection reconnects with backoff from the last
//! *applied* cursor; this tier keeps serving whatever it last held the whole
//! time — the `docs/10` "freeze on last-known-good, never clear" rule,
//! applying at this hop the same way it applies at the proxy-to-controller
//! hop.

use std::sync::Arc;
use std::time::Duration;

use crate::api::AppState;

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

const REVISION_HEADER: &str = "x-config-revision";

/// One-shot `GET /config` against the parent — the initial revision + text,
/// applied before `run` starts tailing. Returns `Ok(None)` if the parent has
/// never had anything submitted to it yet (`404`) — not an error, just
/// nothing to seed with yet; `run`'s subscribe call will pick up the first
/// revision whenever the parent gets one.
pub async fn fetch_initial(
    base_url: &str,
    token: Option<&str>,
) -> anyhow::Result<Option<(u64, String)>> {
    let mut req = gsp_http::client().get(format!("{base_url}/config"));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("fetching initial config from parent {base_url}: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        anyhow::bail!(
            "parent controller {base_url} returned {} for GET /config",
            resp.status()
        );
    }
    let revision: u64 = resp
        .headers()
        .get(REVISION_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| {
            anyhow::anyhow!("parent controller {base_url} response missing {REVISION_HEADER}")
        })?;
    let text = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("reading initial config body from {base_url}: {e}"))?;
    Ok(Some((revision, text)))
}

/// Runs forever, reconnecting to the parent with backoff on disconnect.
/// `tokio::spawn`ed by `main.rs` when `role: slave`; never returns under
/// normal operation.
pub async fn run(base_url: String, token: Option<String>, since: u64, state: Arc<AppState>) {
    let mut cursor = since;
    let mut backoff = RECONNECT_MIN;
    loop {
        match subscribe_once(&base_url, token.as_deref(), &mut cursor, &state).await {
            Ok(()) => {
                backoff = RECONNECT_MIN;
                tracing::warn!(
                    parent = %base_url,
                    cursor,
                    "parent controller subscribe stream ended; reconnecting"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e, parent = %base_url, cursor,
                    "parent controller unreachable; freezing on the last known \
                     configuration and retrying"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

async fn subscribe_once(
    base_url: &str,
    token: Option<&str>,
    cursor: &mut u64,
    state: &AppState,
) -> anyhow::Result<()> {
    let since = *cursor;
    let url = format!("{base_url}/config/subscribe?since={since}");
    let mut req = gsp_http::client().get(&url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("connecting to {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("parent controller {url} returned {}", resp.status());
    }
    tracing::info!(parent = %base_url, since, "subscribed to parent controller config updates");

    let mut buf = String::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| anyhow::anyhow!("reading subscribe stream from {base_url}: {e}"))?;
        let Some(bytes) = chunk else {
            return Ok(()); // parent closed the stream
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(end) = buf.find("\n\n") {
            let event = buf[..end].to_string();
            buf.drain(..end + 2);
            if let Some(revision) = apply_sse_event(&event, state) {
                *cursor = revision;
            }
        }
    }
}

struct SseRevision {
    revision: u64,
    config: String,
}

/// Parses one SSE event block from the parent's `/config/subscribe` (the
/// same `data: {"revision":N,"config":"..."}` shape `gsp-controller`'s own
/// `api::subscribe` emits). `None` for a keep-alive or malformed block.
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

/// Lands one parsed revision in this tier's own store via
/// [`AppState::apply_revision`] — bypassing the `slave` write gate on
/// purpose, since this *is* the sanctioned way a slave gets a new revision.
/// The parent already validated it; this tier re-stores the bytes verbatim
/// and assigns its own local revision number (a slave's revision numbering
/// is local to itself, not required to match the parent's — `docs/10`'s
/// homogeneous-schema requirement is about the payload shape, not about a
/// shared counter across tiers). Returns the revision number so the caller
/// can advance its cursor even when the parent had nothing new to apply for
/// some other reason.
fn apply_sse_event(event: &str, state: &AppState) -> Option<u64> {
    let SseRevision { revision, config } = parse_sse_event(event)?;
    match state.apply_revision(config.into_bytes()) {
        Ok(local_revision) => {
            tracing::info!(
                parent_revision = revision,
                local_revision,
                "relayed a revision from the parent controller"
            );
        }
        Err(e) => {
            tracing::error!(error = %e, parent_revision = revision, "store error relaying a parent revision");
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
        assert!(parse_sse_event(": keep-alive").is_none());
    }

    #[test]
    fn malformed_json_is_ignored_not_a_panic() {
        assert!(parse_sse_event("data: not json").is_none());
    }

    #[tokio::test]
    async fn apply_sse_event_lands_the_revision_in_the_local_store() {
        use crate::role::{Role, RoleHandle};
        use crate::store::Store;
        use std::sync::Arc as StdArc;

        let dir = tempfile::tempdir().unwrap();
        let store = StdArc::new(Store::open(dir.path()).unwrap());
        let state = AppState::new(store, None, RoleHandle::new(Role::Slave));

        let event = r#"data: {"revision":5,"config":"pools: []"}"#;
        let parent_revision = apply_sse_event(event, &state).unwrap();
        assert_eq!(parent_revision, 5);

        let (local_revision, bytes) = state.store.current().unwrap().unwrap();
        assert_eq!(
            local_revision, 1,
            "a slave numbers its own revisions locally"
        );
        assert_eq!(bytes, b"pools: []");
    }
}
