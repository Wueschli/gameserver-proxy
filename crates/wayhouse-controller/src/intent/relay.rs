//! The `slave`-role intent relay (phase 12 slice 4) — the intent-log
//! counterpart to `crate::parent_client`, which only relayed the config log
//! (phase 12 slice 1). A `slave` tier subscribes to its parent's `GET
//! /intent/subscribe` exactly the way it subscribes to the parent's config
//! stream, and lands every op it receives in its own intent store via
//! [`crate::intent::api::IntentState::apply_revision`] — the same
//! role-gate bypass `parent_client::apply_sse_event` uses for config.
//!
//! No `fetch_initial` equivalent here (unlike `parent_client`, which seeds
//! from `GET /config` before subscribing): the intent log has no single
//! "current document" to seed from — it is nothing but a log, so `since=0`'s
//! catch-up range on the very first subscribe already replays the parent's
//! entire history. A config document needs a seed because a subscriber
//! with an *empty* store must still serve *something* the moment it comes
//! up; an intent log has no equivalent "must serve something" requirement.

use std::sync::Arc;
use std::time::Duration;

use crate::intent::api::IntentState;
use crate::intent::IntentOp;

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Runs forever, reconnecting to the parent's intent log with backoff.
/// `tokio::spawn`ed by `main.rs` alongside `parent_client::run` when
/// `--role slave`; never returns under normal operation.
pub async fn run(base_url: String, token: Option<String>, state: Arc<IntentState>) {
    let mut cursor = 0u64;
    let mut backoff = RECONNECT_MIN;
    loop {
        match subscribe_once(&base_url, token.as_deref(), &mut cursor, &state).await {
            Ok(()) => {
                backoff = RECONNECT_MIN;
                tracing::warn!(
                    parent = %base_url, cursor,
                    "parent controller intent stream ended; reconnecting"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e, parent = %base_url, cursor,
                    "parent controller unreachable for intent updates; retrying"
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
    state: &IntentState,
) -> anyhow::Result<()> {
    let since = *cursor;
    let url = format!("{base_url}/intent/subscribe?since={since}");
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
    tracing::info!(parent = %base_url, since, "subscribed to parent controller intent updates");

    let mut buf = wayhouse_http::sse::EventBuffer::new();
    loop {
        let chunk = resp.chunk().await.map_err(|e| {
            anyhow::anyhow!(
                "reading intent subscribe stream from {base_url}: {}",
                wayhouse_http::error_chain(&e)
            )
        })?;
        let Some(bytes) = chunk else {
            return Ok(()); // parent closed the stream
        };
        buf.push(&bytes)
            .map_err(|e| anyhow::anyhow!("subscribe stream from {base_url}: {e}"))?;

        while let Some(event) = buf.next_event() {
            if let Some(revision) = apply_sse_event(&event, state) {
                *cursor = revision;
            }
        }
    }
}

struct SseIntent {
    revision: u64,
    op: IntentOp,
}

fn parse_sse_event(event: &str) -> Option<SseIntent> {
    let data_line = event
        .split('\n')
        .find_map(|line| line.strip_prefix("data:"))?
        .trim_start();
    let payload: serde_json::Value = serde_json::from_str(data_line).ok()?;
    let revision = payload.get("revision")?.as_u64()?;
    let op = serde_json::from_value(payload.get("op")?.clone()).ok()?;
    Some(SseIntent { revision, op })
}

/// Lands one parsed op in this tier's own intent store via
/// [`IntentState::apply_revision`] — the sanctioned bypass of the `slave`
/// write gate, same as `parent_client::apply_sse_event`'s handling of
/// config. Re-serializes the parsed [`IntentOp`] (rather than forwarding the
/// raw JSON text byte-for-byte) so a further-down `slave` subscribing to
/// *this* tier always sees the same canonical shape this tier itself would
/// have produced from a direct submission.
fn apply_sse_event(event: &str, state: &IntentState) -> Option<u64> {
    let SseIntent { revision, op } = parse_sse_event(event)?;
    let bytes = match serde_json::to_vec(&op) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, parent_revision = revision, "failed to re-serialize a relayed intent op");
            return Some(revision);
        }
    };
    match state.apply_revision(bytes) {
        Ok(local_revision) => {
            tracing::info!(
                parent_revision = revision,
                local_revision,
                "relayed an intent op from the parent controller"
            );
        }
        Err(e) => {
            tracing::error!(error = %e, parent_revision = revision, "store error relaying a parent intent op");
        }
    }
    Some(revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::{Role, RoleHandle};
    use crate::store::Store;

    #[test]
    fn parses_a_well_formed_data_event() {
        let event =
            r#"data: {"revision":4,"op":{"op":"backend_add","pool":"mc","addr":"127.0.0.1:1"}}"#;
        let parsed = parse_sse_event(event).unwrap();
        assert_eq!(parsed.revision, 4);
        assert!(matches!(parsed.op, IntentOp::BackendAdd { .. }));
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
    async fn apply_sse_event_lands_the_op_in_the_local_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let state = IntentState::new(store, RoleHandle::new(Role::Slave), None);

        let event =
            r#"data: {"revision":9,"op":{"op":"backend_remove","pool":"mc","addr":"127.0.0.1:1"}}"#;
        let parent_revision = apply_sse_event(event, &state).unwrap();
        assert_eq!(parent_revision, 9);

        let (local_revision, bytes) = state.store.current().unwrap().unwrap();
        assert_eq!(
            local_revision, 1,
            "a slave numbers its own revisions locally"
        );
        let op: IntentOp = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            op,
            IntentOp::BackendRemove {
                pool: "mc".into(),
                addr: "127.0.0.1:1".into()
            }
        );
    }
}
