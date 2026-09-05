//! Aggregator-of-aggregators push (phase 12 slice 2, `docs/10` "Fleet
//! topology"): this tier's merged view, pushed up to a parent aggregator's
//! `POST /ingest` on the same fixed interval `gsp`'s own `aggregator_client`
//! uses to push a proxy's view — "a tier's aggregator is itself a valid
//! 'leaf' to its parent aggregator" (`docs/10` "The aggregator"), so the
//! parent's `IngestStore` needs no distinction between a proxy's push and a
//! child aggregator's relay; it's the exact same [`IngestPayload`] shape,
//! just more of them per tick.
//!
//! **Namespacing, not merging**: rather than inventing a new "subtree
//! summary" payload, this pushes every instance this tier currently knows
//! about individually, with its `instance` field re-written to
//! `"{tier_name}/{instance}"`. That keeps the parent's schema, storage, and
//! `/fleet/*` endpoints completely unchanged — it just sees more, longer-
//! named instances — and keeps this tier's own local names intact for its
//! own `/fleet/*` view and `crate::fanout`'s `admin_url`-based fan-out
//! (namespacing only touches the copy sent upward, `crate::ingest::
//! IngestStore` itself is never renamed in place).
//!
//! No retry buffer, same reasoning as `gsp`'s `aggregator_client`: each tick
//! pushes this tier's *current* state, so a failed push is simply
//! superseded by the next tick's fresher one rather than replayed.

use std::sync::Arc;
use std::time::Duration;

use crate::ingest::{IngestPayload, IngestStore};

/// `--parent-*` settings, resolved once in `main.rs`.
pub struct PushConfig {
    pub base_url: String,
    /// Prefixes every instance name pushed upward
    /// (`"{tier_name}/{instance}"`) so two tiers' instances never collide in
    /// the parent's flat `IngestStore`.
    pub tier_name: String,
    pub interval: Duration,
    pub token: Option<String>,
}

/// Runs forever, pushing one round of namespaced instance summaries per
/// `cfg.interval`. `tokio::spawn`ed from `main.rs` when `--parent-url` is
/// set; never returns under normal operation.
pub async fn run(cfg: PushConfig, store: Arc<IngestStore>) {
    let PushConfig {
        base_url,
        tier_name,
        interval,
        token,
    } = cfg;
    let client = reqwest::Client::new();
    let url = format!("{base_url}/ingest");
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // fire on the first *real* interval, not immediately

    loop {
        ticker.tick().await;
        let instances = store.snapshot();
        for state in instances {
            let payload = namespaced(&tier_name, state.payload);
            let mut req = client.post(&url).json(&payload);
            if let Some(token) = &token {
                req = req.bearer_auth(token);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => tracing::debug!(
                    parent = %base_url, instance = %payload_instance(&payload),
                    "relayed an instance summary to the parent aggregator"
                ),
                Ok(resp) => tracing::warn!(
                    parent = %base_url, status = %resp.status(),
                    "parent aggregator rejected a relayed instance summary"
                ),
                Err(e) => tracing::warn!(
                    parent = %base_url, error = ?e,
                    "pushing to the parent aggregator failed; the next tick will retry"
                ),
            }
        }
    }
}

fn namespaced(tier_name: &str, mut payload: IngestPayload) -> IngestPayload {
    payload.instance = format!("{tier_name}/{}", payload.instance);
    payload
}

fn payload_instance(payload: &IngestPayload) -> &str {
    &payload.instance
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{PoolSummary, SessionCounts};
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::Mutex;

    fn payload(instance: &str) -> IngestPayload {
        IngestPayload {
            instance: instance.to_string(),
            admin_url: "http://127.0.0.1:0".to_string(),
            pools: vec![PoolSummary {
                name: "p".into(),
                balancer: "rr".into(),
                backends: vec![],
            }],
            sessions: SessionCounts { tcp: 1, udp: 0 },
        }
    }

    #[test]
    fn namespacing_prefixes_the_instance_name_and_keeps_the_rest() {
        let out = namespaced("region-a", payload("proxy-1"));
        assert_eq!(out.instance, "region-a/proxy-1");
        assert_eq!(out.sessions.tcp, 1);
        assert_eq!(out.pools.len(), 1);
    }

    #[tokio::test]
    async fn run_relays_every_known_instance_namespaced_to_the_parent() {
        let received: Arc<Mutex<Vec<IngestPayload>>> = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();

        async fn capture(
            State(store): State<Arc<Mutex<Vec<IngestPayload>>>>,
            Json(payload): Json<IngestPayload>,
        ) -> axum::http::StatusCode {
            store.lock().unwrap().push(payload);
            axum::http::StatusCode::OK
        }

        let app = Router::new()
            .route("/ingest", post(capture))
            .with_state(received_clone);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let store = Arc::new(IngestStore::new());
        store.ingest(payload("proxy-1"));
        store.ingest(payload("proxy-2"));

        let cfg = PushConfig {
            base_url: format!("http://{addr}"),
            tier_name: "region-a".into(),
            interval: Duration::from_millis(20),
            token: None,
        };
        let handle = tokio::spawn(run(cfg, store));

        tokio::time::sleep(Duration::from_millis(120)).await;
        handle.abort();

        let names: Vec<String> = received
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.instance.clone())
            .collect();
        assert!(names.contains(&"region-a/proxy-1".to_string()));
        assert!(names.contains(&"region-a/proxy-2".to_string()));
    }
}
