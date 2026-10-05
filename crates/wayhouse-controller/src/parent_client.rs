//! The `slave`-role relay client (phase 12 slice 1, `docs/10` "Fleet
//! topology"): subscribes to a parent `wayhouse-controller` and re-lands every
//! revision it receives in this tier's own [`Store`] via
//! [`crate::api::AppState::apply_revision`] — the only caller allowed to
//! bypass the `slave` write gate in `crate::api::submit`.
//!
//! Deliberately mirrors `wayhouse`'s own `crates/wayhouse/src/controller_client.rs`
//! almost line for line (initial `GET /config`, then a long-lived `GET
//! /config/subscribe?since=<cursor>` with capped-backoff reconnect) — a
//! `slave` controller subscribes to its parent exactly the way a proxy
//! subscribes to a `standalone` one (`docs/10`: "this is principle 5 applied
//! recursively, not a new mechanism"). The wire shape is duplicated rather
//! than shared as a library: it's a few dozen lines of SSE framing, and
//! `wayhouse` has no reason to depend on `wayhouse-controller` (or vice versa).
//!
//! A dropped parent connection reconnects with backoff from the last
//! *applied* cursor; this tier keeps serving whatever it last held the whole
//! time — the `docs/10` "freeze on last-known-good, never clear" rule,
//! applying at this hop the same way it applies at the proxy-to-controller
//! hop.

use std::sync::Arc;
use std::time::Duration;

use crate::api::AppState;
use crate::relay::RelayError;

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
    let mut req = wayhouse_http::client().get(format!("{base_url}/config"));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.map_err(|e| {
        anyhow::anyhow!(
            "fetching initial config from parent {base_url}: {}",
            wayhouse_http::error_chain(&e)
        )
    })?;
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
    let text = resp.text().await.map_err(|e| {
        anyhow::anyhow!(
            "reading initial config body from {base_url}: {}",
            wayhouse_http::error_chain(&e)
        )
    })?;
    Ok(Some((revision, text)))
}

/// Seeds this tier from the parent's current config before the first
/// subscribe: a cold tier (relay cursor `0`) must serve *something* the
/// moment it comes up, not replay the parent's whole history through the
/// catch-up range. `Ok(None)` when the tier already has a cursor or the
/// parent has no config yet. Under HA only the leader may call this (the
/// seed is proposed through Raft).
pub async fn seed_if_cold(
    base_url: &str,
    token: Option<&str>,
    state: &AppState,
) -> anyhow::Result<Option<u64>> {
    if state.relay.get()? > 0 {
        return Ok(None);
    }
    let Some((revision, config)) = fetch_initial(base_url, token).await? else {
        return Ok(None);
    };
    match state.relay_revision(config.into_bytes(), revision).await {
        Ok(()) => {
            tracing::info!(
                parent_revision = revision,
                "seeded initial config from parent controller"
            );
            Ok(Some(revision))
        }
        Err(RelayError::NotLeader) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Runs forever, reconnecting to the parent with backoff on disconnect.
/// `tokio::spawn`ed by `main.rs` when `role: slave`; never returns under
/// normal operation. With HA only the Raft leader subscribes: a node that
/// is not (or stops being) the leader idles until it is, and every
/// subscription resumes from the relay cursor stored with the log.
pub async fn run(base_url: String, token: Option<String>, state: Arc<AppState>) {
    let mut backoff = RECONNECT_MIN;
    loop {
        crate::relay::wait_until_leader(|| state.is_relay_leader()).await;
        match relay_once(&base_url, token.as_deref(), &state).await {
            Ok(()) => {
                backoff = RECONNECT_MIN;
                tracing::warn!(
                    parent = %base_url,
                    "parent controller subscribe stream ended; reconnecting"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e, parent = %base_url,
                    "parent controller unreachable; freezing on the last known \
                     configuration and retrying"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

async fn relay_once(base_url: &str, token: Option<&str>, state: &AppState) -> anyhow::Result<()> {
    seed_if_cold(base_url, token, state).await?;
    let mut cursor = state.relay.get()?;
    subscribe_once(base_url, token, &mut cursor, state).await
}

async fn subscribe_once(
    base_url: &str,
    token: Option<&str>,
    cursor: &mut u64,
    state: &AppState,
) -> anyhow::Result<()> {
    let since = *cursor;
    let url = format!("{base_url}/config/subscribe?since={since}");
    let mut req = wayhouse_http::client().get(&url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("connecting to {url}: {}", wayhouse_http::error_chain(&e)))?;
    if !resp.status().is_success() {
        anyhow::bail!("parent controller {url} returned {}", resp.status());
    }
    tracing::info!(parent = %base_url, since, "subscribed to parent controller config updates");

    let mut buf = wayhouse_http::sse::EventBuffer::new();
    loop {
        let chunk = resp.chunk().await.map_err(|e| {
            anyhow::anyhow!(
                "reading subscribe stream from {base_url}: {}",
                wayhouse_http::error_chain(&e)
            )
        })?;
        let Some(bytes) = chunk else {
            return Ok(()); // parent closed the stream
        };
        buf.push(&bytes)
            .map_err(|e| anyhow::anyhow!("subscribe stream from {base_url}: {e}"))?;

        while let Some(event) = buf.next_event() {
            // A deposed leader must not keep a stream whose events it would
            // skip: dropping it makes the next term resume from the cursor.
            if !state.is_relay_leader() {
                return Ok(());
            }
            match apply_sse_event(&event, state).await {
                Ok(Some(revision)) => *cursor = revision,
                Ok(None) => {}
                Err(RelayError::NotLeader) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

struct SseRevision {
    revision: u64,
    config: String,
}

/// Parses one SSE event block from the parent's `/config/subscribe` (the
/// same `data: {"revision":N,"config":"..."}` shape `wayhouse-controller`'s own
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
/// [`AppState::relay_revision`] — bypassing the `slave` write gate on
/// purpose, since this *is* the sanctioned way a slave gets a new revision.
/// The parent already validated it; this tier re-stores the bytes verbatim
/// and assigns its own local revision number (a slave's revision numbering
/// is local to itself, not required to match the parent's — `docs/10`'s
/// homogeneous-schema requirement is about the payload shape, not about a
/// shared counter across tiers). The parent revision becomes the relay
/// cursor in the same write, so a failed write leaves the cursor behind and
/// the revision is retried. `Ok(None)` for a keep-alive or malformed block.
async fn apply_sse_event(event: &str, state: &AppState) -> Result<Option<u64>, RelayError> {
    let Some(SseRevision { revision, config }) = parse_sse_event(event) else {
        return Ok(None);
    };
    state.relay_revision(config.into_bytes(), revision).await?;
    tracing::info!(
        parent_revision = revision,
        "relayed a revision from the parent controller"
    );
    Ok(Some(revision))
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
        let parent_revision = apply_sse_event(event, &state).await.unwrap();
        assert_eq!(parent_revision, Some(5));

        let (local_revision, bytes) = state.store.current().unwrap().unwrap();
        assert_eq!(
            local_revision, 1,
            "a slave numbers its own revisions locally"
        );
        assert_eq!(bytes, b"pools: []");
        assert_eq!(state.relay.get().unwrap(), 5, "the cursor moves with it");

        // The same parent revision again (a resubscribe overlap) is skipped.
        apply_sse_event(event, &state).await.unwrap();
        assert_eq!(state.store.current_revision().unwrap(), Some(1));
    }
}
