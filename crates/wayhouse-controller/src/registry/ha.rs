//! The registries' write path under HA ("Write path",
//! `docs/superpowers/specs/2026-10-03-ha-replicated-address-allocation-design.md`).
//!
//! The state machine is the only writer of the registries and the address
//! book under HA, so nothing here writes either: `POST` answers an
//! **unchanged** re-registration from this node's replica (proposing at most
//! a background `Touch`) and proposes `Register*` for anything else; `DELETE`
//! proposes `Release`. Each committed [`WriteResponse`] becomes exactly the
//! status and body the non-HA handler gives for the same outcome.
//!
//! Writes are refused with `503` while the cluster has not recorded its
//! tunnel network, and on a node whose own `--tunnel-network` differs from
//! the recorded one ("The cluster's tunnel network"); an unchanged
//! re-registration is a read and is still answered there.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use reqwest::Method;

use crate::addresses::api::claim_error_response;
use crate::addresses::{expand_backends, is_stale, ClaimError, Network, Rejection};
use crate::ha::client::{propose_write, propose_write_as, ForwardHeaders};
use crate::ha::cluster_state::ClusterState;
use crate::ha::{HaHandle, WriteRequest, WriteResponse};

use super::{
    not_registered, store_error_response, wording, DeleteResponse, ErrorResponse, Expiry,
    Registration, RegistryState, SubmitResponse,
};

/// How old an unchanged registration's `last_seen` may get before a
/// re-registration proposes a `Touch`.
pub const TOUCH_AFTER: Duration = Duration::from_secs(3600);

/// What a registry needs to write through Raft.
#[derive(Clone)]
pub struct RegistryHa {
    pub handle: Arc<HaHandle>,
    /// The replicated cluster state (the recorded tunnel network).
    pub cluster: Arc<ClusterState>,
    /// This node's own `--tunnel-network`, checked against the recorded one.
    pub local_network: Option<Network>,
}

/// `s` parsed as a socket address or an IP address and formatted back, so
/// equivalent spellings (`[fd49:0::2]:25565`, `[fd49::2]:25565`) compare
/// equal.
pub fn canonical_addr(s: &str) -> Result<String, String> {
    if let Ok(sa) = s.parse::<SocketAddr>() {
        return Ok(sa.to_string());
    }
    s.parse::<IpAddr>()
        .map(|ip| ip.to_string())
        .map_err(|_| format!("{s:?} is not an address"))
}

/// `incoming` as apply would store it for an owner holding
/// `stored_address`: the address filled in when none was requested,
/// `:port` backends expanded against it and every address in canonical
/// form. An endpoint that is not an address (a peer's endpoint is free-form)
/// is kept as given.
pub fn normalize<R: Registration>(incoming: &R, stored_address: IpAddr) -> Result<R, String> {
    let mut reg = incoming.clone();
    let address = reg.requested_address().unwrap_or(stored_address);
    if let Some(backends) = reg.backends_mut() {
        *backends = expand_backends(backends, address)?;
    }
    reg.set_tunnel_address(address);
    if let Some(endpoint) = reg.endpoint_mut() {
        if let Ok(canonical) = canonical_addr(endpoint) {
            *endpoint = canonical;
        }
    }
    Ok(reg)
}

/// Whether registering `incoming` would store exactly `stored` again (the
/// whole struct, field for field, compared in canonical form).
pub fn is_unchanged<R: Registration>(incoming: &R, stored: &R, stored_address: IpAddr) -> bool {
    // `stored` is canonical except for its endpoint, which apply keeps as
    // given; normalizing it too compares that as a parsed address as well.
    match (
        normalize(incoming, stored_address),
        normalize(stored, stored_address),
    ) {
        (Ok(incoming), Ok(stored)) => incoming == stored,
        _ => false,
    }
}

fn service_unavailable(error: String) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse { error }),
    )
        .into_response()
}

fn network_name(network: Option<Network>) -> String {
    network.map_or_else(|| "none".into(), |n| n.to_string())
}

/// The recorded network, or why a write is refused: `NotInitialized`
/// (`503`) while the cluster has not recorded one, storage (`500`).
fn recorded_network(ha: &RegistryHa) -> Result<Option<Network>, ClaimError> {
    match ha.cluster.network() {
        Ok(Some(network)) => Ok(network),
        Ok(None) => Err(ClaimError::Rejected(Rejection::NotInitialized)),
        Err(e) => Err(ClaimError::Storage(e.0)),
    }
}

/// `503` naming both networks when this node's own differs from the
/// recorded one (compared parsed).
fn mismatch(ha: &RegistryHa, recorded: Option<Network>) -> Option<Response> {
    (ha.local_network != recorded).then(|| {
        tracing::error!(
            local = %network_name(ha.local_network),
            recorded = %network_name(recorded),
            "this node's --tunnel-network differs from the cluster's recorded network; \
             refusing registry writes until the flag is fixed"
        );
        service_unavailable(format!(
            "tunnel network mismatch: this node has {}, the cluster recorded {}",
            network_name(ha.local_network),
            network_name(recorded)
        ))
    })
}

#[allow(clippy::needless_pass_by_value)] // used as a mapper callback, which hands the response over by value
fn unexpected(registry: &str, resp: WriteResponse) -> Response {
    tracing::error!(?resp, "unexpected raft response to a {registry} write");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: format!("unexpected raft response to a {registry} write"),
        }),
    )
        .into_response()
}

fn submitted(revision: u64, address: IpAddr, network: Option<Network>) -> Response {
    (
        StatusCode::OK,
        Json(SubmitResponse {
            revision,
            tunnel_address: address.to_string(),
            tunnel_network: network.map(|n| n.to_string()),
        }),
    )
        .into_response()
}

/// `POST {base}` under HA, after `reg` parsed and validated. `path` and
/// `body` are what a forward to the leader re-sends.
pub(super) async fn register<R: Registration>(
    state: &RegistryState<R>,
    ha: &RegistryHa,
    reg: R,
    path: &str,
    body: String,
    headers: &ForwardHeaders,
) -> Response {
    let words = wording(R::ROLE);
    let recorded = match recorded_network(ha) {
        Ok(n) => n,
        Err(e) => return claim_error_response(&e),
    };

    // The unchanged check, from this node's replica.
    let stored = match state.current_entry(reg.name()) {
        Ok(s) => s,
        Err(e) => return store_error_response(words.log, e),
    };
    let assignment = match state.book.get(R::ROLE, reg.name()) {
        Ok(a) => a,
        Err(e) => return claim_error_response(&e),
    };
    if let (Some((revision, stored)), Some(assignment)) = (stored, assignment) {
        if is_unchanged(&reg, &stored, assignment.address) {
            let now = (state.now_fn)();
            if is_stale(&assignment, now, TOUCH_AFTER) {
                spawn_touch::<R>(ha, reg.name().to_owned(), now, path, body, headers);
            }
            return submitted(revision, assignment.address, recorded);
        }
    }

    if let Some(resp) = mismatch(ha, recorded) {
        return resp;
    }
    let name = reg.name().to_owned();
    let req = reg.register_request((state.now_fn)());
    propose_write(&ha.handle, req, path, body, headers, |resp| match resp {
        WriteResponse::Registered { revision, address } => {
            tracing::info!(revision, name = %name, %address, "registered a {}", words.kind);
            submitted(revision, address, recorded)
        }
        WriteResponse::Rejected(r) => claim_error_response(&ClaimError::Rejected(r)),
        other => unexpected(words.log, other),
    })
    .await
}

/// Proposes a `Touch` for `name` without waiting for it: a lost touch only
/// means the next re-registration proposes it again. A follower forwards the
/// registration itself, so the leader runs the same check and touches.
fn spawn_touch<R: Registration>(
    ha: &RegistryHa,
    name: String,
    now: u64,
    path: &str,
    body: String,
    headers: &ForwardHeaders,
) {
    let handle = ha.handle.clone();
    let headers = headers.clone();
    let path = path.to_owned();
    tokio::spawn(async move {
        let req = WriteRequest::Touch {
            role: R::ROLE,
            name,
            now,
        };
        let resp = propose_write(&handle, req, &path, body, &headers, |resp| match resp {
            WriteResponse::Touched => StatusCode::OK.into_response(),
            other => {
                tracing::debug!(?other, "a background touch changed nothing");
                StatusCode::OK.into_response()
            }
        })
        .await;
        if !resp.status().is_success() {
            tracing::debug!(status = %resp.status(), "a background touch was not committed");
        }
    });
}

/// `DELETE {base}/{name}` under HA: always proposes `Release`.
pub(super) async fn release<R: Registration>(
    ha: &RegistryHa,
    name: String,
    path: &str,
    headers: &ForwardHeaders,
) -> Response {
    let words = wording(R::ROLE);
    let recorded = match recorded_network(ha) {
        Ok(n) => n,
        Err(e) => return claim_error_response(&e),
    };
    if let Some(resp) = mismatch(ha, recorded) {
        return resp;
    }
    let req = WriteRequest::Release {
        role: R::ROLE,
        name: name.clone(),
    };
    propose_write_as(
        &ha.handle,
        req,
        Method::DELETE,
        path,
        String::new(),
        headers,
        |resp| match resp {
            WriteResponse::Released { revision, address } => {
                tracing::info!(revision, name = %name, ?address, "released a {}", words.kind);
                (
                    StatusCode::OK,
                    Json(DeleteResponse {
                        revision,
                        released: address.map(|a| a.to_string()),
                    }),
                )
                    .into_response()
            }
            WriteResponse::NotFound => not_registered(words.noun, &name),
            WriteResponse::Rejected(r) => claim_error_response(&ClaimError::Rejected(r)),
            other => unexpected(words.log, other),
        },
    )
    .await
}

/// One lease expiry under HA: proposes `Expire` on this node's Raft handle,
/// which is only called on the leader (a follower's proposal fails and the
/// sweeper simply tries again next tick).
pub(super) async fn expire<R: Registration>(
    ha: &RegistryHa,
    name: &str,
    last_seen_before: u64,
) -> Result<Expiry, String> {
    let req = WriteRequest::Expire {
        role: R::ROLE,
        name: name.to_owned(),
        last_seen_before,
    };
    let resp = ha
        .handle
        .raft
        .client_write(req)
        .await
        .map_err(|e| format!("proposing an expiry: {e}"))?;
    match resp.data {
        WriteResponse::Released { address, .. } => Ok(Expiry::Released(address)),
        WriteResponse::NotExpired => Ok(Expiry::NotExpired),
        WriteResponse::NotFound | WriteResponse::Revision(None) => Ok(Expiry::Gone),
        WriteResponse::Rejected(r) => Err(ClaimError::Rejected(r).to_string()),
        other => Err(format!("unexpected raft response to an expiry: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::addresses::{AddressBook, Role};
    use crate::api::AppState;
    use crate::ha::state_machine::{Registries, StateMachineStore};
    use crate::ha::{client, log_store, network, raft_config, Raft, WriteRequest, SNAPSHOT_AFTER};
    use crate::intent::api::IntentState;
    use crate::peers::api::PeersState;
    use crate::proxy_peers::api::ProxyPeersState;
    use crate::role::{Role as TierRole, RoleHandle};
    use crate::store::Store;

    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    /// The test clock's start (unix seconds).
    const T0: u64 = 1_000_000;

    /// A single-node Raft controller serving both registries under HA.
    /// Proposals are counted from the Raft log itself: every proposal is one
    /// log entry, so nothing in the production path needs a test hook.
    struct HaApp {
        app: Router,
        raft: Raft,
        peers: PeersState,
        proxy_peers: ProxyPeersState,
        ha: RegistryHa,
        book: Arc<AddressBook>,
        now: Arc<AtomicU64>,
        /// The log index after setup (election blank entry, the network).
        baseline: u64,
        _dirs: tempfile::TempDir,
    }

    impl HaApp {
        fn last_log_index(&self) -> u64 {
            self.raft.metrics().borrow().last_log_index.unwrap_or(0)
        }

        /// Entries proposed since setup.
        fn proposals(&self) -> u64 {
            self.last_log_index() - self.baseline
        }

        /// Waits until `n` entries were proposed since setup.
        async fn wait_for_proposals(&self, n: u64) {
            self.raft
                .wait(Some(Duration::from_secs(5)))
                .log_index(Some(self.baseline + n), "proposals")
                .await
                .unwrap_or_else(|e| {
                    panic!("expected {n} proposals, have {}: {e}", self.proposals())
                });
        }

        /// Gives a wrongly spawned background proposal time to show up.
        async fn settle(&self) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        /// The same registries served by a node whose `--tunnel-network` is
        /// `local`.
        fn node_with_network(&self, local: Option<&str>) -> Router {
            let ha = RegistryHa {
                local_network: local.map(|n| Network::parse(n).unwrap()),
                ..self.ha.clone()
            };
            routers(
                self.peers.clone().with_ha(Some(ha.clone())),
                self.proxy_peers.clone().with_ha(Some(ha)),
            )
        }
    }

    fn routers(peers: PeersState, proxy_peers: ProxyPeersState) -> Router {
        crate::peers::api::router(peers).merge(crate::proxy_peers::api::router(proxy_peers))
    }

    /// `recorded`: the network `SetTunnelNetwork` records (`None` = leave the
    /// cluster uninitialized); `local`: this node's `--tunnel-network`.
    async fn ha_app_with(recorded: Option<Option<&str>>, local: Option<&str>) -> HaApp {
        let dir = tempfile::tempdir().unwrap();
        let store = |sub: &str| Arc::new(Store::open(&dir.path().join(sub)).unwrap());
        let config = Arc::new(AppState::new(
            store("config"),
            None,
            RoleHandle::new(TierRole::Standalone),
        ));
        let intent = Arc::new(IntentState::new(
            store("intent"),
            RoleHandle::new(TierRole::Standalone),
            None,
        ));
        // Under HA the book takes its network from the cluster state.
        let book = Arc::new(AddressBook::open(&dir.path().join("addresses"), None).unwrap());
        let peers = PeersState::new(store("peers"), None, book.clone());
        let proxy_peers = ProxyPeersState::new(store("proxy-peers"), None, book.clone());

        let db = sled::open(dir.path().join("ha")).unwrap();
        let sm = Arc::new(
            StateMachineStore::open(
                &db,
                config,
                intent,
                Registries {
                    peers: Arc::new(peers.clone()),
                    proxy_peers: Arc::new(proxy_peers.clone()),
                    book: book.clone(),
                },
            )
            .unwrap(),
        );
        let cluster = sm.cluster().clone();
        let raft = Raft::new(
            1,
            Arc::new(raft_config(SNAPSHOT_AFTER).validate().unwrap()),
            network::Network::new(None),
            log_store::LogStore::open(&db).unwrap(),
            sm,
        )
        .await
        .unwrap();
        raft.initialize(BTreeMap::from([(
            1,
            openraft::BasicNode::new("127.0.0.1:1"),
        )]))
        .await
        .unwrap();
        raft.wait(Some(Duration::from_secs(10)))
            .current_leader(1, "single node elects itself")
            .await
            .unwrap();
        if let Some(network) = recorded {
            raft.client_write(WriteRequest::SetTunnelNetwork(network.map(String::from)))
                .await
                .unwrap();
        }

        let ha = RegistryHa {
            handle: Arc::new(HaHandle {
                raft: raft.clone(),
                node_id: 1,
                ha_token: None,
                forward: client::forward_client(client::FORWARD_TIMEOUT),
                pre_ha: crate::ha::import::LocalPreHa::default(),
            }),
            cluster,
            local_network: local.map(|n| Network::parse(n).unwrap()),
        };
        let now = Arc::new(AtomicU64::new(T0));
        let clock = now.clone();
        let now_fn: Arc<dyn Fn() -> u64 + Send + Sync> =
            Arc::new(move || clock.load(Ordering::SeqCst));
        let peers = peers.with_now_fn(now_fn.clone());
        let proxy_peers = proxy_peers.with_now_fn(now_fn);
        let app = routers(
            peers.clone().with_ha(Some(ha.clone())),
            proxy_peers.clone().with_ha(Some(ha.clone())),
        );
        let mut out = HaApp {
            app,
            raft,
            peers,
            proxy_peers,
            ha,
            book,
            now,
            baseline: 0,
            _dirs: dir,
        };
        out.baseline = out.last_log_index();
        out
    }

    async fn ha_app(network: &str) -> HaApp {
        ha_app_with(Some(Some(network)), Some(network)).await
    }

    /// Today's single-node controller with `network`.
    fn non_ha_app(network: &str) -> (Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = |sub: &str| Arc::new(Store::open(&dir.path().join(sub)).unwrap());
        let book = Arc::new(
            AddressBook::open(
                &dir.path().join("addresses"),
                Some(Network::parse(network).unwrap()),
            )
            .unwrap(),
        );
        let app = routers(
            PeersState::new(store("peers"), None, book.clone()),
            ProxyPeersState::new(store("proxy-peers"), None, book),
        );
        (app, dir)
    }

    async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let body = if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        };
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(wayhouse_http::protocol::HEADER, "1.0")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, value)
    }

    fn peer(name: &str, backends: &[&str]) -> Value {
        json!({ "name": name, "pubkey": KEY, "backends": backends })
    }

    fn proxy(name: &str, boot_id: &str) -> Value {
        json!({ "name": name, "pubkey": KEY, "endpoint": "198.51.100.7:51820", "boot_id": boot_id })
    }

    #[test]
    fn canonical_addr_folds_equivalent_spellings() {
        assert_eq!(
            canonical_addr("[fd49:0::2]:25565").unwrap(),
            canonical_addr("[fd49::2]:25565").unwrap()
        );
        assert_eq!(canonical_addr("10.60.0.2:1").unwrap(), "10.60.0.2:1");
        assert_eq!(
            canonical_addr("fd49:0:0::7").unwrap(),
            canonical_addr("fd49::7").unwrap()
        );
        assert!(canonical_addr("nonsense").is_err());
    }

    #[test]
    fn normalize_fills_the_address_and_expands_the_backends() {
        let incoming: crate::peers::PeerRegistration =
            serde_json::from_value(peer("home", &[":25565"])).unwrap();
        let stored_address: IpAddr = "10.60.0.1".parse().unwrap();
        let normalized = normalize(&incoming, stored_address).unwrap();
        assert_eq!(normalized.tunnel_address.as_deref(), Some("10.60.0.1"));
        assert_eq!(normalized.backends, vec!["10.60.0.1:25565".to_string()]);
        assert!(is_unchanged(&incoming, &normalized, stored_address));
    }

    #[tokio::test]
    async fn an_unchanged_re_registration_proposes_nothing() {
        let h = ha_app("10.60.0.0/24").await;
        let first = call(&h.app, "POST", "/peers", peer("home", &[":25565"])).await;
        assert_eq!(first.0, StatusCode::OK, "{first:?}");
        assert_eq!(h.proposals(), 1);

        // The same registration, and the same one spelled as stored.
        let again = call(&h.app, "POST", "/peers", peer("home", &[":25565"])).await;
        let mut spelled = peer("home", &["10.60.0.1:25565"]);
        spelled["tunnel_address"] = json!("10.60.0.1");
        let spelled = call(&h.app, "POST", "/peers", spelled).await;
        h.settle().await;
        assert_eq!(again, first);
        assert_eq!(spelled, first);
        assert_eq!(h.proposals(), 1);
    }

    #[tokio::test]
    async fn a_stale_last_seen_answers_at_once_and_proposes_one_touch() {
        let h = ha_app("10.60.0.0/24").await;
        let first = call(&h.app, "POST", "/peers", peer("home", &[])).await;
        assert_eq!(first.0, StatusCode::OK, "{first:?}");
        assert_eq!(h.proposals(), 1);

        let later = T0 + TOUCH_AFTER.as_secs() + 1;
        h.now.store(later, Ordering::SeqCst);
        let again = call(&h.app, "POST", "/peers", peer("home", &[])).await;
        assert_eq!(again, first);
        h.wait_for_proposals(2).await;
        h.settle().await;
        assert_eq!(h.proposals(), 2, "exactly one touch");
        let assignment = h.book.get(Role::Origin, "home").unwrap().unwrap();
        assert_eq!(assignment.last_seen, later);
        assert_eq!(assignment.first_seen, T0);
        // The touch wrote no registry revision.
        assert_eq!(h.peers.store.current_revision().unwrap(), Some(1));
    }

    #[tokio::test]
    async fn the_lease_sweep_expires_through_raft_and_spares_a_touched_owner() {
        use crate::lease::sweep;
        let h = ha_app("10.60.0.0/24").await;
        let ttl = Duration::from_secs(7200);
        for name in ["gone", "alive"] {
            let r = call(&h.app, "POST", "/peers", peer(name, &[])).await;
            assert_eq!(r.0, StatusCode::OK, "{r:?}");
        }
        // "alive" re-registers late enough to be touched (not rewritten).
        let touched_at = T0 + TOUCH_AFTER.as_secs() + 1;
        h.now.store(touched_at, Ordering::SeqCst);
        call(&h.app, "POST", "/peers", peer("alive", &[])).await;
        h.wait_for_proposals(3).await;
        h.settle().await;

        let peers = h.peers.clone().with_ha(Some(h.ha.clone()));
        let proxies = h.proxy_peers.clone().with_ha(Some(h.ha.clone()));
        let now = T0 + ttl.as_secs() + 1;
        assert_eq!(sweep(&h.book, &peers, &proxies, ttl, now).await, 1);

        assert_eq!(h.peers.current_for("gone").unwrap(), None);
        assert!(h.book.get(Role::Origin, "gone").unwrap().is_none());
        assert!(h.peers.current_for("alive").unwrap().is_some());
        // A second sweep has nothing left to do.
        assert_eq!(sweep(&h.book, &peers, &proxies, ttl, now).await, 0);
    }

    #[tokio::test]
    async fn a_changed_backend_proposes_register() {
        let h = ha_app("10.60.0.0/24").await;
        let first = call(&h.app, "POST", "/peers", peer("home", &[":25565"])).await;
        assert_eq!(first.0, StatusCode::OK, "{first:?}");
        let changed = call(&h.app, "POST", "/peers", peer("home", &[":25566"])).await;
        assert_eq!(changed.0, StatusCode::OK, "{changed:?}");
        assert_eq!(changed.1["revision"], 2);
        assert_eq!(changed.1["tunnel_address"], "10.60.0.1");
        assert_eq!(h.proposals(), 2);
        let stored = h.peers.current_for("home").unwrap().unwrap();
        assert_eq!(stored.backends, vec!["10.60.0.1:25566".to_string()]);
    }

    #[tokio::test]
    async fn a_changed_boot_id_proposes_register() {
        let h = ha_app("10.60.0.0/24").await;
        let first = call(&h.app, "POST", "/proxy-peers", proxy("edge", "boot-1")).await;
        assert_eq!(first.0, StatusCode::OK, "{first:?}");
        let same = call(&h.app, "POST", "/proxy-peers", proxy("edge", "boot-1")).await;
        h.settle().await;
        assert_eq!(same, first);
        assert_eq!(h.proposals(), 1);

        let restarted = call(&h.app, "POST", "/proxy-peers", proxy("edge", "boot-2")).await;
        assert_eq!(restarted.0, StatusCode::OK, "{restarted:?}");
        assert_eq!(restarted.1["revision"], 2);
        assert_eq!(h.proposals(), 2);
        let stored = h.proxy_peers.current_for("edge").unwrap().unwrap();
        assert_eq!(stored.boot_id.as_deref(), Some("boot-2"));
    }

    #[tokio::test]
    async fn ha_responses_match_the_non_ha_responses() {
        // A /30 holds two hosts, so the third owner exhausts it.
        let network = "10.60.0.0/30";
        let h = ha_app(network).await;
        let (plain, _dir) = non_ha_app(network);

        let mut pinned = peer("b", &[]);
        pinned["tunnel_address"] = json!("10.60.0.1");
        let steps: Vec<(&str, &str, Value, StatusCode)> = vec![
            ("POST", "/peers", peer("a", &[":25565"]), StatusCode::OK),
            ("POST", "/peers", peer("a", &[":25565"]), StatusCode::OK),
            ("POST", "/peers", pinned, StatusCode::CONFLICT),
            (
                "POST",
                "/peers",
                peer("c", &["10.99.0.9:1"]),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                "POST",
                "/peers",
                json!({ "name": "", "pubkey": KEY }),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                "POST",
                "/proxy-peers",
                proxy("d", "boot-1"),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "DELETE",
                "/peers/nobody",
                Value::Null,
                StatusCode::NOT_FOUND,
            ),
            ("DELETE", "/peers/a", Value::Null, StatusCode::OK),
        ];
        for (i, (method, path, body, status)) in steps.into_iter().enumerate() {
            let want = call(&plain, method, path, body.clone()).await;
            // Non-HA bumps the revision on an idempotent re-registration; HA
            // answers the stored one. Everything else is identical.
            let got = call(&h.app, method, path, body).await;
            assert_eq!(want.0, status, "step {i}: non-HA {want:?}");
            if i == 1 {
                assert_eq!(got.0, want.0, "step {i}");
                assert_eq!(got.1["tunnel_address"], want.1["tunnel_address"]);
                assert_eq!(got.1["tunnel_network"], want.1["tunnel_network"]);
                continue;
            }
            if i > 1 && method == "DELETE" && status == StatusCode::OK {
                // Non-HA logged one more revision at step 1.
                assert_eq!(got.0, want.0, "step {i}");
                assert_eq!(got.1["released"], want.1["released"], "step {i}");
                continue;
            }
            assert_eq!(got, want, "step {i}");
        }
    }

    #[tokio::test]
    async fn a_mismatched_network_node_answers_unchanged_but_refuses_writes() {
        let h = ha_app("10.60.0.0/24").await;
        let first = call(&h.app, "POST", "/peers", peer("home", &[":25565"])).await;
        assert_eq!(first.0, StatusCode::OK, "{first:?}");

        let node = h.node_with_network(Some("10.61.0.0/24"));
        let unchanged = call(&node, "POST", "/peers", peer("home", &[":25565"])).await;
        assert_eq!(unchanged, first);

        for (method, path, body) in [
            ("POST", "/peers", peer("home", &[":25566"])),
            ("POST", "/peers", peer("new", &[])),
            ("DELETE", "/peers/home", Value::Null),
        ] {
            let (status, body) = call(&node, method, path, body).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method} {path}");
            let error = body["error"].as_str().unwrap();
            assert!(
                error.contains("10.61.0.0/24") && error.contains("10.60.0.0/24"),
                "{error}"
            );
        }
        h.settle().await;
        assert_eq!(h.proposals(), 1);

        // A node without a network against a recorded one mismatches too.
        let node = h.node_with_network(None);
        let (status, _) = call(&node, "POST", "/peers", peer("new", &[])).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn an_uninitialized_cluster_refuses_writes() {
        let h = ha_app_with(None, Some("10.60.0.0/24")).await;
        for (method, path, body) in [
            ("POST", "/peers", peer("home", &[])),
            ("DELETE", "/peers/home", Value::Null),
        ] {
            let (status, body) = call(&h.app, method, path, body).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method} {path}");
            assert_eq!(body["error"], "cluster is initializing its registries");
        }
        h.settle().await;
        assert_eq!(h.proposals(), 0);
    }

    #[tokio::test]
    async fn the_leader_records_its_network_once_and_registrations_follow() {
        use crate::ha::init::{initialize_registries, ImportPolicy};
        let h = ha_app_with(None, Some("10.60.0.0/24")).await;
        let local = h.ha.local_network;
        tokio::time::timeout(
            Duration::from_secs(10),
            initialize_registries(
                h.ha.handle.clone(),
                h.ha.cluster.clone(),
                local,
                ImportPolicy::Auto,
            ),
        )
        .await
        .expect("initialization ends once the network is recorded");
        assert_eq!(h.ha.cluster.network().unwrap(), Some(local));
        let (status, body) = call(&h.app, "POST", "/peers", peer("o1", &[":25565"])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["tunnel_address"], "10.60.0.1");
    }

    #[tokio::test]
    async fn a_node_whose_flag_differs_from_the_recorded_network_ends_initialization() {
        use crate::ha::init::{initialize_registries, ImportPolicy};
        let h = ha_app_with(Some(Some("10.60.0.0/24")), Some("10.61.0.0/24")).await;
        tokio::time::timeout(
            Duration::from_secs(10),
            initialize_registries(
                h.ha.handle.clone(),
                h.ha.cluster.clone(),
                h.ha.local_network,
                ImportPolicy::Auto,
            ),
        )
        .await
        .expect("a recorded network ends initialization even on a mismatch");
        // The recorded value is never overwritten by the mismatching flag.
        assert_eq!(
            h.ha.cluster.network().unwrap(),
            Some(Some(Network::parse("10.60.0.0/24").unwrap()))
        );
    }
}
