//! Module blobs between HA replicas (plugin slice 7; design: the automation hooks
//! addendum, "Module bytes in HA").
//!
//! The Raft log carries install records only. The module bytes (up to 8 MiB) are a
//! content-addressed blob each replica keeps in its own store, moved over the peer
//! channel (`--ha-token`, the protocol gate) by three pieces:
//!
//! - the leader **pushes** a module to every member (`PUT /raft/plugin-blob/{sha256}`)
//!   and proposes the install only once a quorum of voters holds it, so an install cannot
//!   be stranded on a node that dies right after the upload;
//! - every replica runs a **catch-up loop** ([`spawn_sync`]) that fetches the modules its
//!   installs reference and it lacks (`GET /raft/plugin-blob/{sha256}`) from the leader
//!   first, then the other members, and verifies the sha256 before keeping them: a
//!   replica that was down, joined late or restored a snapshot gets its modules this way;
//! - [`check_support`] refuses a new install while any member runs a build that cannot
//!   take part (`/raft/whoami` reports [`PLUGIN_SUPPORT`]).
//!
//! The routes only exist with `--plugins`; a node without them answers 404, which the
//! quorum push counts as "does not hold the module".

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use wayhouse_plugin_host::{inspect, MAX_MODULE_BYTES};

use super::{PluginStore, PluginStoreError};
use crate::ha::members::fetch_whoami;
use crate::ha::peers::peer_url;
use crate::ha::routes::{Whoami, PLUGIN_SUPPORT};
use crate::ha::{HaHandle, NodeId};

/// How often the catch-up loop looks for modules this node lacks.
const SYNC_EVERY: Duration = Duration::from_secs(3);
/// How often a leader that still lacks a module it needs says so in the log.
const ALERT_EVERY: Duration = Duration::from_secs(60);
/// Bound for one module transfer: 8 MiB, over a peer link.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct PeerState {
    store: PluginStore,
}

/// `GET` and `PUT /raft/plugin-blob/{sha256}`, behind the same peer token and protocol
/// gate as the other `/raft/*` routes.
pub fn router(ha: &HaHandle, store: PluginStore) -> Router {
    let routes = Router::new()
        .route(
            "/raft/plugin-blob/{sha256}",
            get(get_blob)
                .put(put_blob)
                .layer(DefaultBodyLimit::max(MAX_MODULE_BYTES)),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            wayhouse_http::server::BearerAuth::new(ha.ha_token.as_deref()),
            wayhouse_http::server::require_bearer,
        ));
    wayhouse_http::protocol::gate(routes, "raft").with_state(PeerState { store })
}

fn internal(e: &PluginStoreError) -> Response {
    tracing::error!(error = %e, "plugin blob store failure");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

async fn get_blob(State(st): State<PeerState>, Path(sha256): Path<String>) -> Response {
    if !super::is_sha256(&sha256) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match st.store.get_blob(&sha256) {
        Ok(Some(bytes)) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal(&e),
    }
}

/// Keeps the body under `sha256` once it is a well-formed module with exactly that hash.
async fn put_blob(
    State(st): State<PeerState>,
    Path(sha256): Path<String>,
    body: Bytes,
) -> Response {
    if !super::is_sha256(&sha256) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match inspect(&body) {
        Ok(info) if info.sha256 == sha256 => match st.store.put_blob(&sha256, &body) {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => internal(&e),
        },
        Ok(_) => (
            StatusCode::BAD_REQUEST,
            "the body does not hash to the path",
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// The other members of the cluster (voters and learners): id and address.
fn others(ha: &HaHandle) -> Vec<(NodeId, String)> {
    let metrics = ha.raft.metrics().borrow().clone();
    metrics
        .membership_config
        .membership()
        .nodes()
        .filter(|(id, _)| **id != ha.node_id)
        .map(|(id, node)| (*id, node.addr.clone()))
        .collect()
}

fn voters(ha: &HaHandle) -> BTreeSet<NodeId> {
    ha.raft
        .metrics()
        .borrow()
        .membership_config
        .membership()
        .voter_ids()
        .collect()
}

/// Whether `acks` replicas holding a module are a majority of `voters`.
pub fn quorum_reached(acks: usize, voters: usize) -> bool {
    acks > voters / 2
}

/// Pushes `bytes` to every other member and succeeds once a quorum of voters (this node
/// included) holds the module. The learners get it too but do not count.
pub async fn replicate_to_quorum(ha: &HaHandle, sha256: &str, bytes: Bytes) -> Result<(), String> {
    let voters = voters(ha);
    let mut acks = usize::from(voters.contains(&ha.node_id));
    let mut tasks = tokio::task::JoinSet::new();
    for (id, addr) in others(ha) {
        let (client, token) = (ha.forward.clone(), ha.ha_token.clone());
        let (url, body) = (
            peer_url(&addr, &format!("/raft/plugin-blob/{sha256}")),
            bytes.clone(),
        );
        tasks.spawn(async move {
            let mut req = client.put(url).timeout(TRANSFER_TIMEOUT).body(body);
            if let Some(token) = &token {
                req = req.bearer_auth(token);
            }
            let held = match req.send().await {
                Ok(resp) if resp.status().is_success() => true,
                Ok(resp) => {
                    tracing::warn!(node = id, status = %resp.status(), "peer refused a plugin module");
                    false
                }
                Err(e) => {
                    tracing::warn!(node = id, error = %wayhouse_http::error_chain(&e), "could not push a plugin module to a peer");
                    false
                }
            };
            (id, held)
        });
    }
    while let Some(done) = tasks.join_next().await {
        if let Ok((id, true)) = done {
            if voters.contains(&id) {
                acks += 1;
            }
        }
    }
    if quorum_reached(acks, voters.len()) {
        Ok(())
    } else {
        Err(format!(
            "only {acks} of {} replicas could take the module; a majority must hold it before \
             the install is recorded. Retry shortly",
            voters.len()
        ))
    }
}

/// `Ok` when every answer says the member can take part in module replication.
pub fn check_support_answers(answers: &[(NodeId, Result<Whoami, String>)]) -> Result<(), String> {
    for (id, answer) in answers {
        match answer {
            Err(why) => {
                return Err(format!(
                    "node {id} did not answer ({why}); plugin installs wait until every member \
                     reports the build it runs"
                ))
            }
            Ok(who) if who.plugin_support < PLUGIN_SUPPORT => {
                return Err(format!(
                    "node {id} runs a build without plugin module replication; upgrade every \
                     replica before installing plugins"
                ))
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// The rolling-upgrade gate for a new install: every member must report a build that
/// takes part in module replication. Existing installs keep working through an upgrade
/// (they exist only because the gate passed); this stops a new one reaching a replica that
/// cannot decode the entry or hold the module.
pub async fn check_support(ha: &HaHandle) -> Result<(), String> {
    let mut tasks = tokio::task::JoinSet::new();
    for (id, addr) in others(ha) {
        let (client, token) = (ha.forward.clone(), ha.ha_token.clone());
        tasks.spawn(async move { (id, fetch_whoami(&client, token.as_deref(), &addr).await) });
    }
    let mut answers = Vec::new();
    while let Some(done) = tasks.join_next().await {
        if let Ok(answer) = done {
            answers.push(answer);
        }
    }
    answers.sort_by_key(|(id, _)| *id);
    check_support_answers(&answers)
}

/// Fetches `sha256` from the first of `addrs` that has it; `None` when none does.
pub async fn fetch_blob(
    client: &reqwest::Client,
    token: Option<&str>,
    addrs: &[String],
    sha256: &str,
) -> Option<Vec<u8>> {
    for addr in addrs {
        let mut req = client
            .get(peer_url(addr, &format!("/raft/plugin-blob/{sha256}")))
            .timeout(TRANSFER_TIMEOUT);
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let Ok(resp) = req.send().await else { continue };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(bytes) = resp.bytes().await else {
            continue;
        };
        // A peer is trusted with the bytes, not with their hash.
        if inspect(&bytes).is_ok_and(|info| info.sha256 == sha256) {
            return Some(bytes.to_vec());
        }
        tracing::warn!(peer = %addr, sha256, "a peer sent a module that does not match its hash");
    }
    None
}

/// Fetches every module an install references and this node lacks, from `addrs` in
/// order. Returns the modules still missing afterwards.
pub async fn sync_once(
    store: &PluginStore,
    client: &reqwest::Client,
    token: Option<&str>,
    addrs: &[String],
) -> Result<Vec<String>, PluginStoreError> {
    let mut still_missing = Vec::new();
    for sha256 in store.missing_blobs()? {
        match fetch_blob(client, token, addrs, &sha256).await {
            Some(bytes) => store.put_blob(&sha256, &bytes)?,
            None => still_missing.push(sha256),
        }
    }
    Ok(still_missing)
}

/// Peer addresses to ask for a module: the leader first, then the others by id.
fn fetch_order(ha: &HaHandle) -> Vec<String> {
    let leader = ha.raft.metrics().borrow().current_leader;
    let mut peers = others(ha);
    peers.sort_by_key(|(id, _)| (Some(*id) != leader, *id));
    peers.into_iter().map(|(_, addr)| addr).collect()
}

/// The catch-up loop: until the task is dropped, fetch the modules this node's installs
/// need. A leader that cannot find one raises a visible alert (a log line, repeated), since
/// it would otherwise silently run nothing.
pub fn spawn_sync(store: PluginStore, ha: Arc<HaHandle>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(SYNC_EVERY);
        let mut last_alert: Option<Instant> = None;
        loop {
            every.tick().await;
            match store.missing_blobs() {
                Ok(missing) if missing.is_empty() => continue,
                Err(e) => {
                    tracing::error!(error = %e, "plugin module catch-up could not list installs");
                    continue;
                }
                Ok(_) => {}
            }
            let addrs = fetch_order(&ha);
            match sync_once(&store, &ha.forward, ha.ha_token.as_deref(), &addrs).await {
                Ok(left) if !left.is_empty() && ha.is_leader() => {
                    if last_alert.is_none_or(|at| at.elapsed() >= ALERT_EVERY) {
                        last_alert = Some(Instant::now());
                        tracing::warn!(
                            modules = ?left,
                            "this node leads but does not hold plugin modules and no peer has \
                             them; those plugins cannot run until a member that holds them is \
                             reachable or the module is uploaded again"
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "plugin module catch-up failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::routes::Whoami;

    fn who(plugin_support: u32) -> Whoami {
        Whoami {
            node_id: 1,
            log_empty: false,
            pre_ha: crate::ha::import::PreHaSummary::default(),
            plugin_support,
        }
    }

    #[test]
    fn a_majority_of_voters_is_a_quorum() {
        assert!(quorum_reached(1, 1));
        assert!(!quorum_reached(1, 2));
        assert!(quorum_reached(2, 3));
        assert!(!quorum_reached(1, 3));
        assert!(!quorum_reached(2, 4));
        assert!(quorum_reached(3, 4));
        assert!(!quorum_reached(0, 0));
    }

    #[test]
    fn an_install_waits_for_every_member_to_report_a_supporting_build() {
        assert!(
            check_support_answers(&[]).is_ok(),
            "a single node has no peers"
        );
        assert!(check_support_answers(&[
            (2, Ok(who(PLUGIN_SUPPORT))),
            (3, Ok(who(PLUGIN_SUPPORT + 1)))
        ])
        .is_ok());
        let old =
            check_support_answers(&[(2, Ok(who(PLUGIN_SUPPORT))), (3, Ok(who(0)))]).unwrap_err();
        assert!(old.contains("node 3") && old.contains("upgrade"), "{old}");
        let silent = check_support_answers(&[(2, Err("connection refused".into()))]).unwrap_err();
        assert!(
            silent.contains("node 2") && silent.contains("did not answer"),
            "{silent}"
        );
    }

    use crate::ha::test_support::single_node_with_plugins;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn module(tag: &str) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(module
  (@custom "wayhouse.plugin-abi" "\00\00\01\00")
  (@custom "wayhouse.plugin-caps" "{{\"log\":true}}")
  (@custom "tag" "{tag}")
  (import "wayhouse" "log" (func $log (param i32 i32 i32)))
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 0))
  (func (export "init") (param i32 i32)))"#
        ))
        .unwrap()
    }

    fn store() -> (PluginStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        (PluginStore::open(&db).unwrap(), dir)
    }

    async fn send(app: &Router, method: &str, path: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
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
        (status, bytes.to_vec())
    }

    #[tokio::test]
    async fn a_peer_can_put_and_get_a_module_by_its_hash() {
        let (ha, _c, _plugins, _d) = single_node_with_plugins(1, "127.0.0.1:1").await;
        let (store, _s) = store();
        let app = router(&ha, store.clone());
        let bytes = module("a");
        let sha = inspect(&bytes).unwrap().sha256;
        let path = format!("/raft/plugin-blob/{sha}");
        assert_eq!(
            send(&app, "GET", &path, vec![]).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(&app, "PUT", &path, bytes.clone()).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            send(&app, "GET", &path, vec![]).await,
            (StatusCode::OK, bytes.clone())
        );
        assert_eq!(store.get_blob(&sha).unwrap().unwrap(), bytes);
    }

    #[tokio::test]
    async fn a_put_that_does_not_hash_to_its_path_or_is_no_module_is_refused() {
        let (ha, _c, _plugins, _d) = single_node_with_plugins(1, "127.0.0.1:1").await;
        let (store, _s) = store();
        let app = router(&ha, store.clone());
        let other = inspect(&module("b")).unwrap().sha256;
        let path = format!("/raft/plugin-blob/{other}");
        assert_eq!(
            send(&app, "PUT", &path, module("a")).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send(&app, "PUT", &path, b"not wasm".to_vec()).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send(&app, "PUT", "/raft/plugin-blob/../../x", module("a"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(&app, "GET", "/raft/plugin-blob/NOTHEX", vec![])
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert!(store.get_blob(&other).unwrap().is_none());
    }

    #[tokio::test]
    async fn the_blob_routes_need_the_peer_token_and_a_compatible_protocol() {
        let (mut_ha, _c, _plugins, _d) = single_node_with_plugins(1, "127.0.0.1:1").await;
        let ha = HaHandle {
            ha_token: Some(Arc::from("peer-token-of-sixteen")),
            raft: mut_ha.raft.clone(),
            node_id: 1,
            forward: mut_ha.forward.clone(),
            pre_ha: crate::ha::import::LocalPreHa::default(),
        };
        let (store, _s) = store();
        let app = router(&ha, store);
        let path = format!("/raft/plugin-blob/{}", "0".repeat(64));
        assert_eq!(
            send(&app, "GET", &path, vec![]).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    /// Serves `store`'s blobs the way a peer does, on an ephemeral port.
    async fn serve_peer(store: PluginStore) -> String {
        let (ha, _c, _plugins, _d) = single_node_with_plugins(9, "127.0.0.1:1").await;
        let app = router(&ha, store);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // The fixture's raft group must outlive the server.
            let _keep = (ha, _c, _plugins, _d);
            axum::serve(listener, app).await.unwrap();
        });
        format!("127.0.0.1:{}", addr.port())
    }

    fn install_of(sha: &str, id: &str) -> super::super::InstallRecord {
        super::super::InstallRecord {
            id: id.into(),
            name: "demo".into(),
            sha256: sha.into(),
            size: 10,
            approved: wayhouse_plugin_host::Capabilities::parse(br#"{"log":true}"#).unwrap(),
            config: serde_json::json!({}),
            enabled: true,
            created_at: 1,
            created_by: None,
        }
    }

    #[tokio::test]
    async fn catch_up_fetches_missing_modules_from_the_first_peer_that_has_them() {
        let (donor, _d1) = store();
        let bytes = module("c");
        let sha = inspect(&bytes).unwrap().sha256;
        donor.put_blob(&sha, &bytes).unwrap();
        let (empty, _d2) = store();
        let addrs = vec![serve_peer(empty).await, serve_peer(donor).await];

        let (mine, _d3) = store();
        mine.write_record(&install_of(&sha, "0000000000000001"))
            .unwrap();
        let client = wayhouse_http::client();
        let left = sync_once(&mine, &client, None, &addrs).await.unwrap();
        assert!(left.is_empty());
        assert_eq!(mine.get_blob(&sha).unwrap().unwrap(), bytes);
    }

    #[tokio::test]
    async fn catch_up_reports_what_no_peer_has_and_ignores_a_peer_that_lies() {
        let (liar, _d1) = store();
        let wanted = module("d");
        let sha = inspect(&wanted).unwrap().sha256;
        // A different, valid module stored under the wanted hash: the peer lies about it.
        liar.put_blob(&sha, &module("other")).unwrap();
        let addrs = vec![serve_peer(liar).await];
        let (mine, _d2) = store();
        mine.write_record(&install_of(&sha, "0000000000000002"))
            .unwrap();
        let client = wayhouse_http::client();
        assert_eq!(
            sync_once(&mine, &client, None, &addrs).await.unwrap(),
            std::slice::from_ref(&sha)
        );
        assert!(mine.get_blob(&sha).unwrap().is_none());
    }
}
