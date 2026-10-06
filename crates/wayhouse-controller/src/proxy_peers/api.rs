//! `POST /proxy-peers` / `GET /proxy-peers` / `GET /proxy-peers/{name}` /
//! `DELETE /proxy-peers/{name}` / `GET /proxy-peers/subscribe` — the
//! proxy-peers registry's HTTP surface: the shared [`crate::registry`] core
//! under `/proxy-peers`. See the module doc in `crate::proxy_peers`.

use axum::Router;

use super::ProxyRegistration;
use crate::registry::{self, RegistryState};

/// The proxy-peers registry: the shared registry core over [`ProxyRegistration`].
pub type ProxyPeersState = RegistryState<ProxyRegistration>;

pub fn router(state: ProxyPeersState) -> Router {
    registry::router(state, "/proxy-peers")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::http::StatusCode;
    use tokio::sync::{broadcast, mpsc};

    use crate::registry::subscribe_worker;
    use crate::store::Store;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    use crate::addresses::{AddressBook, Network, Role};

    fn test_state() -> (ProxyPeersState, Arc<AddressBook>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("proxy-peers")).unwrap());
        let book = Arc::new(
            AddressBook::open(
                &dir.path().join("addresses"),
                Some(Network::parse("10.60.0.0/16").unwrap()),
            )
            .unwrap(),
        );
        (ProxyPeersState::new(store, None, book.clone()), book, dir)
    }

    async fn post(app: &Router, body: String) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::post("/proxy-peers")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn body_with(name: &str, tunnel_address: Option<&str>) -> String {
        let mut v = serde_json::json!({
            "name": name,
            "pubkey": KEY,
            "endpoint": "203.0.113.9:51820",
        });
        if let Some(a) = tunnel_address {
            v["tunnel_address"] = serde_json::json!(a);
        }
        v.to_string()
    }

    fn reg_body(name: &str, endpoint: &str) -> String {
        serde_json::json!({
            "name": name,
            "pubkey": KEY,
            "endpoint": endpoint,
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_valid_registration_is_accepted() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/proxy-peers")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::from(reg_body("edge-1", "203.0.113.9:51820")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_invalid_registration_is_rejected_before_touching_the_store() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/proxy-peers")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::from(
                        r#"{"name":"edge-1","pubkey":"garbage","endpoint":"x"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn malformed_json_is_rejected() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/proxy-peers")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn get_one_is_404_before_any_registration() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::get("/proxy-peers/nope")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_second_registration_replaces_the_current_view_for_that_name() {
        let (state, _book, _dir) = test_state();
        let app = router(state);

        app.clone()
            .oneshot(
                Request::post("/proxy-peers")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::from(reg_body("edge-1", "203.0.113.9:51820")))
                    .unwrap(),
            )
            .await
            .unwrap();
        app.clone()
            .oneshot(
                Request::post("/proxy-peers")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::from(reg_body("edge-1", "203.0.113.9:51821")))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(
                Request::get("/proxy-peers/edge-1")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let reg: ProxyRegistration = serde_json::from_slice(&body).unwrap();
        assert_eq!(reg.endpoint, "203.0.113.9:51821");
    }

    #[tokio::test]
    async fn list_returns_every_currently_registered_proxy() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        app.clone()
            .oneshot(
                Request::post("/proxy-peers")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::from(reg_body("a", "203.0.113.1:51820")))
                    .unwrap(),
            )
            .await
            .unwrap();
        app.clone()
            .oneshot(
                Request::post("/proxy-peers")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::from(reg_body("b", "203.0.113.2:51820")))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(
                Request::get("/proxy-peers")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let regs: Vec<ProxyRegistration> = serde_json::from_slice(&body).unwrap();
        let mut names: Vec<_> = regs.into_iter().map(|r| r.name).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn a_missing_bearer_token_is_rejected_when_one_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("proxy-peers")).unwrap());
        let book = Arc::new(
            AddressBook::open(
                &dir.path().join("addresses"),
                Some(Network::parse("10.60.0.0/16").unwrap()),
            )
            .unwrap(),
        );
        let state = ProxyPeersState::new(store, Some("secret".into()), book);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::get("/proxy-peers")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn subscribe_worker_sends_the_catch_up_range_then_tails() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = store
            .put(
                serde_json::to_vec(&ProxyRegistration {
                    name: "edge-1".into(),
                    pubkey: KEY.into(),
                    endpoint: "203.0.113.9:51820".into(),
                    tunnel_address: None,
                    boot_id: None,
                    max_config_schema: None,
                    refresh_sec: None,
                })
                .unwrap(),
            )
            .unwrap();

        let (updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(store.clone(), updates_rx, 0, tx));

        let (got_rev, _) = rx.recv().await.unwrap();
        assert_eq!(got_rev, rev1);

        let rev2 = store
            .put(
                serde_json::to_vec(&ProxyRegistration {
                    name: "edge-1".into(),
                    pubkey: KEY.into(),
                    endpoint: "203.0.113.9:51821".into(),
                    tunnel_address: None,
                    boot_id: None,
                    max_config_schema: None,
                    refresh_sec: None,
                })
                .unwrap(),
            )
            .unwrap();
        updates_tx.send(rev2).unwrap();
        let (got_rev2, _) = rx.recv().await.unwrap();
        assert_eq!(got_rev2, rev2);
    }

    #[tokio::test]
    async fn a_registration_without_an_address_is_allocated_one_and_told_the_network() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, body) = post(&app, body_with("edge-1", None)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tunnel_address"], "10.60.0.1");
        assert_eq!(body["tunnel_network"], "10.60.0.0/16");
    }

    #[tokio::test]
    async fn re_registering_returns_the_same_address_and_two_proxies_get_distinct_ones() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (_, first) = post(&app, body_with("a", None)).await;
        let (_, again) = post(&app, body_with("a", None)).await;
        let (_, other) = post(&app, body_with("b", None)).await;
        assert_eq!(first["tunnel_address"], again["tunnel_address"]);
        assert_ne!(first["tunnel_address"], other["tunnel_address"]);
    }

    #[tokio::test]
    async fn a_pinned_address_held_by_an_origin_is_a_409_naming_the_holder() {
        let (state, book, _dir) = test_state();
        book.claim(Role::Origin, "home", Some("10.60.0.9".parse().unwrap()), 1)
            .unwrap();
        let app = router(state);
        let (status, body) = post(&app, body_with("edge-1", Some("10.60.0.9"))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body["error"].as_str().unwrap().contains("origin \"home\""));
    }

    #[tokio::test]
    async fn a_pin_outside_the_network_is_a_422() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, _) = post(&app, body_with("edge-1", Some("192.168.1.5"))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn the_stored_registration_carries_the_allocated_address() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        post(&app, body_with("edge-1", None)).await;
        let resp = app
            .oneshot(
                Request::get("/proxy-peers/edge-1")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let reg: ProxyRegistration = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reg.tunnel_address.as_deref(), Some("10.60.0.1"));
    }

    #[tokio::test]
    async fn the_boot_id_is_stored_as_submitted_and_a_bad_one_is_a_422() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let mut v: serde_json::Value = serde_json::from_str(&body_with("edge-1", None)).unwrap();
        v["boot_id"] = serde_json::json!("0123456789abcdef");
        assert_eq!(post(&app, v.to_string()).await.0, StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                Request::get("/proxy-peers/edge-1")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let reg: ProxyRegistration = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reg.boot_id.as_deref(), Some("0123456789abcdef"));

        v["boot_id"] = serde_json::json!("not valid!");
        assert_eq!(
            post(&app, v.to_string()).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    async fn delete(app: &Router, name: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::delete(format!("/proxy-peers/{name}"))
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn delete_frees_the_address_and_removes_the_current_registration() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (_, a) = post(&app, body_with("a", None)).await;
        assert_eq!(a["tunnel_address"], "10.60.0.1");

        let (status, body) = delete(&app, "a").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["released"], "10.60.0.1");

        let resp = app
            .clone()
            .oneshot(
                Request::get("/proxy-peers/a")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let (_, b) = post(&app, body_with("b", None)).await;
        assert_eq!(b["tunnel_address"], "10.60.0.1");
    }

    #[tokio::test]
    async fn delete_of_an_unknown_name_is_404() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, _) = delete(&app, "nobody").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    const ROUNDS: usize = 200;

    /// A POST racing a DELETE for the same name must never leave a live
    /// registration whose address the book has freed (or the reverse).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn post_racing_delete_never_splits_registry_and_book() {
        let (state, book, _dir) = test_state();
        let app = router(state.clone());
        for round in 0..ROUNDS {
            post(&app, body_with("race", None)).await;
            let _ = tokio::join!(
                tokio::spawn({
                    let app = app.clone();
                    async move { post(&app, body_with("race", None)).await }
                }),
                tokio::spawn({
                    let app = app.clone();
                    async move { delete(&app, "race").await }
                }),
            );
            let current = state.current_for("race").unwrap();
            let held = book.get(Role::Proxy, "race").unwrap();
            match (&current, &held) {
                (None, None) => {}
                (Some(c), Some(h)) => assert_eq!(
                    c.tunnel_address.as_deref(),
                    Some(h.address.to_string().as_str()),
                    "round {round}: registration and book disagree on the address"
                ),
                _ => panic!(
                    "round {round}: registration present={} but book present={}",
                    current.is_some(),
                    held.is_some()
                ),
            }
            // Reset for the next round.
            let _ = delete(&app, "race").await;
        }
    }

    #[tokio::test]
    async fn delete_logs_a_tombstone_a_catch_up_subscriber_receives() {
        let (state, _book, _dir) = test_state();
        let app = router(state.clone());
        post(&app, body_with("edge-1", None)).await;
        delete(&app, "edge-1").await;

        let (tx, mut rx) = mpsc::channel(8);
        let updates = state.updates.subscribe();
        tokio::spawn(subscribe_worker(state.store.clone(), updates, 0, tx));
        let (r1, b1) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for event 1")
            .expect("channel closed before event 1");
        let (r2, b2) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for event 2")
            .expect("channel closed before event 2");
        assert!(r2 > r1);
        assert_eq!(
            crate::registry::event_payload(r1, &b1)["registration"]["name"],
            "edge-1"
        );
        assert_eq!(
            crate::registry::event_payload(r2, &b2)["removed"]["name"],
            "edge-1"
        );
    }

    #[tokio::test]
    async fn proxy_peers_rejects_other_major_with_426() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/proxy-peers")
                    .header(wayhouse_http::protocol::HEADER, "2.0")
                    .body(Body::from(reg_body("edge-1", "203.0.113.9:51820")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
    }

    #[tokio::test]
    async fn proxy_peers_subscribe_is_refused_before_the_stream_starts() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::get("/proxy-peers/subscribe")
                    .header(wayhouse_http::protocol::HEADER, "2.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
    }

    fn schema_body(name: &str, max: Option<u32>, refresh_sec: Option<u64>) -> String {
        let mut v: serde_json::Value =
            serde_json::from_str(&reg_body(name, "203.0.113.9:51820")).unwrap();
        // A distinct key per name; the endpoint stays shared (allowed).
        if let Some(m) = max {
            v["max_config_schema"] = serde_json::json!(m);
        }
        if let Some(r) = refresh_sec {
            v["refresh_sec"] = serde_json::json!(r);
        }
        v.to_string()
    }

    fn clocked(state: ProxyPeersState) -> (ProxyPeersState, Arc<std::sync::atomic::AtomicU64>) {
        let now = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
        let n = now.clone();
        let state = state.with_now_fn(Arc::new(move || {
            n.load(std::sync::atomic::Ordering::SeqCst)
        }));
        (state, now)
    }

    #[tokio::test]
    async fn the_floor_is_the_lowest_max_config_schema_reported() {
        let (state, _book, _dir) = test_state();
        let (state, _now) = clocked(state);
        let app = router(state.clone());
        assert_eq!(
            state.min_live_config_schema().unwrap(),
            None,
            "no proxies, no floor"
        );
        assert_eq!(
            post(&app, schema_body("a", Some(3), None)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            post(&app, schema_body("b", Some(2), None)).await.0,
            StatusCode::OK
        );
        assert_eq!(state.min_live_config_schema().unwrap(), Some(2));
        // A live proxy that reports nothing predates `schema_version`: it cannot
        // parse a document that carries the key, so it pins the floor at 0.
        assert_eq!(
            post(&app, schema_body("c", None, None)).await.0,
            StatusCode::OK
        );
        assert_eq!(state.min_live_config_schema().unwrap(), Some(0));
    }

    #[tokio::test]
    async fn stale_registration_does_not_pin_the_minimum() {
        let (state, _book, _dir) = test_state();
        let (state, now) = clocked(state);
        let app = router(state.clone());
        assert_eq!(
            post(&app, schema_body("old", Some(1), Some(30))).await.0,
            StatusCode::OK
        );
        now.store(1_000 + 90, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            post(&app, schema_body("new", Some(5), Some(30))).await.0,
            StatusCode::OK
        );
        // Three intervals since `old` last registered: still live.
        assert_eq!(state.min_live_config_schema().unwrap(), Some(1));
        now.store(1_000 + 91, std::sync::atomic::Ordering::SeqCst);
        // One second more and it is stale; only `new` counts.
        assert_eq!(state.min_live_config_schema().unwrap(), Some(5));
    }

    #[tokio::test]
    async fn a_deleted_registration_no_longer_counts() {
        let (state, _book, _dir) = test_state();
        let (state, _now) = clocked(state);
        let app = router(state.clone());
        assert_eq!(
            post(&app, schema_body("gone", Some(1), None)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            post(&app, schema_body("kept", Some(4), None)).await.0,
            StatusCode::OK
        );
        let resp = app
            .clone()
            .oneshot(
                Request::delete("/proxy-peers/gone")
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(state.min_live_config_schema().unwrap(), Some(4));
    }

    #[tokio::test]
    async fn a_registration_without_the_protocol_header_is_refused() {
        let (state, _book, _dir) = test_state();
        let resp = router(state)
            .oneshot(
                Request::post("/proxy-peers")
                    .body(Body::from(reg_body("edge-1", "203.0.113.9:51820")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
    }

    #[tokio::test]
    async fn nonsense_schema_reports_are_refused() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        for (max, refresh) in [
            (Some(0), None),
            (None, Some(0)),
            (None, Some(u64::MAX / 3 + 1)),
        ] {
            let (status, _) = post(&app, schema_body("x", max, refresh)).await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{max:?} {refresh:?}"
            );
        }
        let (status, _) = post(&app, schema_body("x", Some(1), Some(u64::MAX / 3))).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the largest allowed interval is accepted"
        );
    }
}
