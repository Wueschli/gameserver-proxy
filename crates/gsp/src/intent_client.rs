//! The intent-log consumer (phase 12 slice 3, `docs/10` "Operator intent
//! ... moves into the controller's revision log"). Only active alongside
//! `--controller` (the intent log lives on the same controller instance) —
//! subscribes to `GET /intent/subscribe?since=<cursor>` and applies each
//! accepted op straight to the live [`RuntimeHandle`], through the exact
//! same calls `crate::admin`'s handlers make for the equivalent verb
//! (`backend_overlay().add/remove`, `Backend::set_admin_state`,
//! `route_hints().set`) — an intent op received from the controller is not
//! a new code path, just a new *source* for the same mutation the admin API
//! already does directly.
//!
//! Structurally identical to `controller_client`'s subscribe-with-backoff
//! shape (same SSE framing, same reconnect policy) — deliberately
//! duplicated rather than shared, same reasoning as that module's own doc
//! comment on why it doesn't depend on `gsp-controller`. Starts from
//! `since=0` on every process start (unlike `controller_client`, which
//! resumes from the config revision it booted with): every op here is
//! idempotent to replay (add/remove/patch a specific backend, or set a
//! route-hint with a fresh TTL), so there's no persisted cursor to restore,
//! and replaying the whole log on a cold start is exactly what should
//! happen — it's how a freshly (re)started instance catches up to whatever
//! the fleet's operators have done so far.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use gsp_core::pool::AdminState as BackendState;
use gsp_core::RuntimeHandle;
use serde::Deserialize;

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Mirrors `gsp_controller::intent::IntentOp` — see that type's doc for why
/// the wire shape is duplicated rather than shared as a dependency.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum IntentOp {
    BackendAdd {
        pool: String,
        addr: String,
    },
    BackendRemove {
        pool: String,
        addr: String,
    },
    BackendPatch {
        pool: String,
        addr: String,
        state: String,
    },
    RouteHint {
        src_ip: String,
        pool: String,
        #[serde(default = "default_ttl_sec")]
        ttl_sec: u64,
    },
}

fn default_ttl_sec() -> u64 {
    30
}

/// Runs forever, reconnecting to the controller's intent log with backoff on
/// disconnect. `tokio::spawn`ed from `main.rs` alongside `controller_client`
/// whenever `--controller` is set; never returns under normal operation.
pub async fn run(base_url: String, token: Option<String>, handle: RuntimeHandle) {
    let mut cursor = 0u64;
    let mut backoff = RECONNECT_MIN;
    loop {
        match subscribe_once(&base_url, token.as_deref(), &mut cursor, &handle).await {
            Ok(()) => {
                backoff = RECONNECT_MIN;
                tracing::warn!(
                    controller = %base_url, cursor,
                    "intent subscribe stream ended; reconnecting"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e, controller = %base_url, cursor,
                    "controller unreachable for intent updates; retrying"
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
    handle: &RuntimeHandle,
) -> anyhow::Result<()> {
    let since = *cursor;
    let url = format!("{base_url}/intent/subscribe?since={since}");
    let mut req = gsp_http::client().get(&url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("connecting to {url}: {}", gsp_http::error_chain(&e)))?;
    if !resp.status().is_success() {
        anyhow::bail!("controller {url} returned {}", resp.status());
    }
    tracing::info!(controller = %base_url, since, "subscribed to controller intent updates");

    let mut buf = String::new();
    loop {
        let chunk = resp.chunk().await.map_err(|e| {
            anyhow::anyhow!(
                "reading intent subscribe stream from {base_url}: {}",
                gsp_http::error_chain(&e)
            )
        })?;
        let Some(bytes) = chunk else {
            return Ok(()); // server closed the stream
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(end) = buf.find("\n\n") {
            let event = buf[..end].to_string();
            buf.drain(..end + 2);
            if let Some(revision) = apply_sse_event(&event, handle) {
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

/// Applies one parsed op and returns its revision so the caller can advance
/// the cursor — even when applying failed for a local reason (unknown pool,
/// say): the controller already validated the op's *shape*, but "does pool
/// X exist on this particular instance" is instance-local information the
/// controller doesn't have, so a per-instance mismatch is logged and
/// skipped here, never treated as a reason to replay forever.
fn apply_sse_event(event: &str, handle: &RuntimeHandle) -> Option<u64> {
    let SseIntent { revision, op } = parse_sse_event(event)?;
    if let Err(e) = apply(&op, handle) {
        tracing::warn!(revision, ?op, error = %e, "could not apply intent op on this instance");
    } else {
        tracing::info!(revision, ?op, "applied an intent op from the controller");
    }
    Some(revision)
}

fn apply(op: &IntentOp, handle: &RuntimeHandle) -> Result<(), String> {
    match op {
        IntentOp::BackendAdd { pool, addr } => {
            let addr: SocketAddr = addr.parse().map_err(|_| "invalid addr".to_string())?;
            if handle.snapshot().pool(pool).is_none() {
                return Err(format!("unknown pool {pool}"));
            }
            handle.backend_overlay().add(pool, addr);
            handle.request_reload();
            Ok(())
        }
        IntentOp::BackendRemove { pool, addr } => {
            let addr: SocketAddr = addr.parse().map_err(|_| "invalid addr".to_string())?;
            if handle.snapshot().pool(pool).is_none() {
                return Err(format!("unknown pool {pool}"));
            }
            handle.backend_overlay().remove(pool, addr);
            handle.request_reload();
            Ok(())
        }
        IntentOp::BackendPatch { pool, addr, state } => {
            let addr: SocketAddr = addr.parse().map_err(|_| "invalid addr".to_string())?;
            let state = BackendState::parse(state).ok_or_else(|| "invalid state".to_string())?;
            let snap = handle.snapshot();
            let pool = snap
                .pool(pool)
                .ok_or_else(|| format!("unknown pool {pool}"))?;
            let backend = pool
                .backend(addr)
                .ok_or_else(|| format!("unknown backend {addr} in pool {}", pool.name))?;
            backend.set_admin_state(state);
            Ok(())
        }
        IntentOp::RouteHint {
            src_ip,
            pool,
            ttl_sec,
        } => {
            let ip: IpAddr = src_ip.parse().map_err(|_| "invalid src_ip".to_string())?;
            if handle.snapshot().pool(pool).is_none() {
                return Err(format!("unknown pool {pool}"));
            }
            handle
                .route_hints()
                .set(ip, pool.clone(), Duration::from_secs(*ttl_sec));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_data_event() {
        let event =
            r#"data: {"revision":3,"op":{"op":"backend_add","pool":"mc","addr":"127.0.0.1:1"}}"#;
        let parsed = parse_sse_event(event).unwrap();
        assert_eq!(parsed.revision, 3);
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

    #[test]
    fn route_hint_defaults_ttl_when_omitted() {
        let event =
            r#"data: {"revision":1,"op":{"op":"route_hint","src_ip":"1.2.3.4","pool":"mc"}}"#;
        let parsed = parse_sse_event(event).unwrap();
        assert!(matches!(parsed.op, IntentOp::RouteHint { ttl_sec: 30, .. }));
    }

    #[tokio::test]
    async fn applying_backend_add_to_an_unknown_pool_is_a_clean_error_not_a_panic() {
        use gsp_core::{Runtime, Snapshot};

        const YAML: &str = r#"
pools:
  - name: mc
    targets: ["127.0.0.1:1"]
listeners:
  - name: main
    bind: "127.0.0.1:0"
    protocol: tcp
    pool: mc
"#;
        let cfg = gsp_config::parse_str(YAML).unwrap();
        let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
        let op = IntentOp::BackendAdd {
            pool: "nope".into(),
            addr: "127.0.0.1:2".into(),
        };
        assert!(apply(&op, &runtime.handle()).is_err());
    }
}
