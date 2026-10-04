//! The registry core both tunnel registries share — `/peers` (origins,
//! [`crate::peers`]) and `/proxy-peers` (proxies, [`crate::proxy_peers`]).
//! Each is **per-name latest-write-wins state** on top of
//! [`crate::store::Store`]'s append-only log (what a subscriber catches up
//! on) plus one sibling `sled` tree, `current`, in the same database (opened
//! via `Store::db`), mapping `name -> latest revision number` so `GET` needn't
//! scan the log.
//!
//! The log entry and its `current` move land in **one** transaction
//! ([`RegistryState::register_applied`] / [`RegistryState::remove_applied`]),
//! and with a Raft index the store's `applied_index` joins it too, so a
//! replayed apply step is a no-op ("Crash-idempotent apply",
//! `docs/superpowers/specs/2026-10-03-ha-replicated-address-allocation-design.md`).
//!
//! Under HA ([`RegistryState::with_ha`]) the write handlers never write
//! either store: they propose through Raft and the state machine applies
//! (see [`ha`]); reads are served from the local replica either way.
//!
//! The two registries differ only in their registration type (the
//! [`Registration`] trait), their address-book [`Role`] and their route
//! prefix, which `crate::peers::api` / `crate::proxy_peers::api` supply.

use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::IpAddr;
use std::sync::{Arc, PoisonError};

use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tracing::Instrument;

use crate::addresses::api::claim_error_response;
use crate::addresses::{expand_backends, unix_secs, AddressBook, ClaimError, Rejection, Role};
use crate::store::{Applied, RevisionBytes, SiblingWrite, Store, StoreError};

mod ha;

pub use ha::{canonical_addr, is_unchanged, normalize, RegistryHa, TOUCH_AFTER};

const UPDATES_CAPACITY: usize = 64;

/// One registry's registration type — what `POST` takes, what `current`
/// points at and what every subscriber event carries.
pub trait Registration:
    Serialize + DeserializeOwned + Clone + PartialEq + Send + Sync + 'static
{
    /// The address-book role every registration of this type claims as.
    const ROLE: Role;
    /// The registration's stable identity (the `current` key).
    fn name(&self) -> &str;
    /// The tunnel address the registration pins, if it names one.
    fn requested_address(&self) -> Option<IpAddr>;
    /// The fronted backends, expanded against the granted address before
    /// storing — `None` for a type that has none.
    fn backends_mut(&mut self) -> Option<&mut Vec<String>>;
    /// Records the granted tunnel address in the stored registration.
    fn set_tunnel_address(&mut self, a: IpAddr);
    /// Rejects a malformed submission before it reaches the store.
    fn validate(&self) -> Result<(), String>;
    /// The registrant's own dial-out endpoint, stored as given — `None` for
    /// a type (or a registration) without one. The unchanged check compares
    /// it as a parsed address when it parses.
    fn endpoint_mut(&mut self) -> Option<&mut String> {
        None
    }
    /// The Raft entry that registers `self`, stamped with the proposing
    /// leader's clock.
    fn register_request(self, now: u64) -> crate::ha::WriteRequest;
}

/// The words one registry's responses and logs use.
struct Wording {
    /// In `malformed {noun} registration` / `no {noun} registered as`.
    noun: &'static str,
    /// In `registered a {kind}` / `released a {kind}`.
    kind: &'static str,
    /// The registry's name in log lines.
    log: &'static str,
}

fn wording(role: Role) -> Wording {
    match role {
        Role::Origin => Wording {
            noun: "peer",
            kind: "backend peer",
            log: "peers",
        },
        Role::Proxy => Wording {
            noun: "proxy",
            kind: "proxy peer",
            log: "proxy-peers",
        },
    }
}

#[derive(Clone)]
pub struct RegistryState<R: Registration> {
    pub(crate) store: Arc<Store>,
    /// `name -> latest revision number` (big-endian `u64`), a sibling tree
    /// in the same `sled` database `store` opened — see `Store::db`'s doc
    /// and `crate::api::AppState::stage`'s identical pattern.
    current: sled::Tree,
    pub(crate) updates: broadcast::Sender<u64>,
    /// Bearer token every request must present, or `None` to leave the API
    /// open — same posture as `crate::api::AppState`'s and
    /// `crate::intent::api::IntentState`'s own `auth_token`.
    auth_token: Option<Arc<str>>,
    /// The shared tunnel-address book every registration claims from.
    book: Arc<AddressBook>,
    /// Serialises POST (claim -> register) against DELETE (check -> remove ->
    /// release) so a re-register can never interleave with a delete and leave
    /// a live registration whose address the book freed. Never held across an
    /// `.await`.
    write_lock: Arc<std::sync::Mutex<()>>,
    /// `Some` under HA: writes go through Raft ([`ha::RegistryHa`]); `None`
    /// keeps today's direct claim-and-register path.
    ha: Option<ha::RegistryHa>,
    /// The clock (unix seconds) a write proposed here is stamped with —
    /// [`unix_secs`] outside tests.
    now_fn: Arc<dyn Fn() -> u64 + Send + Sync>,
    registration: PhantomData<R>,
}

impl<R: Registration> RegistryState<R> {
    pub fn new(store: Arc<Store>, auth_token: Option<String>, book: Arc<AddressBook>) -> Self {
        let (updates, _rx) = broadcast::channel(UPDATES_CAPACITY);
        let current = store
            .db()
            .open_tree("current")
            .unwrap_or_else(|e| panic!("opening the {} current tree: {e}", wording(R::ROLE).log));
        RegistryState {
            store,
            current,
            updates,
            auth_token: auth_token.map(Arc::from),
            book,
            write_lock: Arc::new(std::sync::Mutex::new(())),
            ha: None,
            now_fn: Arc::new(unix_secs),
            registration: PhantomData,
        }
    }

    /// Routes this registry's writes through Raft (`Some`), or keeps them on
    /// the direct non-HA path (`None`). The state machine's own copy never
    /// gets one: it is the writer HA writes end up at.
    pub fn with_ha(mut self, ha: Option<ha::RegistryHa>) -> Self {
        self.ha = ha;
        self
    }

    /// Replaces the clock writes are stamped with (tests age `last_seen`).
    pub fn with_now_fn(mut self, now_fn: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.now_fn = now_fn;
        self
    }

    /// Logs `reg` as a new revision and points `current[reg.name()]` at it,
    /// in one transaction. With a Raft `index` the store's `applied_index`
    /// joins that transaction and a replayed index writes nothing
    /// ([`Applied::AlreadyApplied`]); `None` (non-HA) always writes.
    pub fn register_applied(&self, reg: &R, index: Option<u64>) -> Result<Applied, StoreError> {
        let bytes = serde_json::to_vec(reg).expect("a registration always serializes");
        self.append(bytes, reg.name(), true, index)
    }

    /// Logs a tombstone for `name` and drops it from `current`, in one
    /// transaction — `index` as for [`RegistryState::register_applied`].
    pub fn remove_applied(&self, name: &str, index: Option<u64>) -> Result<Applied, StoreError> {
        self.append(tombstone_bytes(name), name, false, index)
    }

    /// The shared write: `bytes` as the next revision, plus `current[name]`
    /// set to it (`keep`) or removed.
    fn append(
        &self,
        bytes: RevisionBytes,
        name: &str,
        keep: bool,
        index: Option<u64>,
    ) -> Result<Applied, StoreError> {
        let current = |revision: u64| {
            vec![SiblingWrite {
                tree: &self.current,
                key: name.as_bytes().to_vec(),
                value: keep.then(|| revision.to_be_bytes().to_vec()),
            }]
        };
        let applied = match index {
            Some(index) => self.store.put_applied_with(bytes, index, &current)?,
            None => Applied::Written(self.store.put_with(bytes, &current)?),
        };
        if let Applied::Written(revision) = applied {
            let _ = self.updates.send(revision);
        }
        Ok(applied)
    }

    /// The current registration for `name`, if it has ever registered.
    pub fn current_for(&self, name: &str) -> Result<Option<R>, StoreError> {
        Ok(self.current_entry(name)?.map(|(_, reg)| reg))
    }

    /// [`RegistryState::current_for`] with the revision it was logged at.
    pub fn current_entry(&self, name: &str) -> Result<Option<(u64, R)>, StoreError> {
        let Some(rev_bytes) = self.current.get(name.as_bytes())? else {
            return Ok(None);
        };
        let revision = decode_revision(&rev_bytes);
        Ok(self
            .store
            .get(revision)?
            .map(|bytes| (revision, decode_registration(&bytes))))
    }

    /// Whether `name` has a current registration (not removed).
    pub(crate) fn has_current(&self, name: &str) -> Result<bool, StoreError> {
        Ok(self.current.contains_key(name.as_bytes())?)
    }

    /// Every name's current registration, ordered by name.
    pub(crate) fn all_current(&self) -> Result<Vec<R>, StoreError> {
        let mut out = Vec::new();
        for item in self.current.iter() {
            let (_, rev_bytes) = item?;
            if let Some(bytes) = self.store.get(decode_revision(&rev_bytes))? {
                out.push(decode_registration(&bytes));
            }
        }
        Ok(out)
    }
}

/// One registry as a Raft snapshot carries it: every revision at its number,
/// the `current` map and the store's `applied_index`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrySnapshot {
    pub revisions: Vec<(u64, RevisionBytes)>,
    /// `name -> latest revision`, ordered by name.
    pub current: Vec<(String, u64)>,
    pub applied_index: Option<u64>,
}

impl<R: Registration> RegistryState<R> {
    /// An owned copy of the registry for a Raft snapshot. The caller must be
    /// the only writer (the state-machine worker).
    pub fn snapshot(&self) -> Result<RegistrySnapshot, StoreError> {
        let mut current = Vec::new();
        for item in self.current.iter() {
            let (name, rev_bytes) = item?;
            current.push((
                String::from_utf8_lossy(&name).into_owned(),
                decode_revision(&rev_bytes),
            ));
        }
        Ok(RegistrySnapshot {
            revisions: self.store.all_revisions()?,
            current,
            applied_index: self.store.applied_index()?,
        })
    }

    /// Replaces the log, the `current` map and `applied_index` by `snapshot`
    /// in one transaction (a Raft snapshot install), then wakes subscribers
    /// so they re-read from the replaced store.
    pub fn replace(&self, snapshot: &RegistrySnapshot) -> Result<(), StoreError> {
        let siblings = snapshot
            .current
            .iter()
            .map(|(name, revision)| SiblingWrite {
                tree: &self.current,
                key: name.as_bytes().to_vec(),
                value: Some(revision.to_be_bytes().to_vec()),
            })
            .collect();
        self.store.replace_all_with(
            &snapshot.revisions,
            snapshot.applied_index,
            &[&self.current],
            siblings,
        )?;
        if let Some((revision, _)) = snapshot.revisions.last() {
            let _ = self.updates.send(*revision);
        }
        Ok(())
    }
}

fn decode_revision(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(bytes);
    u64::from_be_bytes(buf)
}

fn decode_registration<R: Registration>(bytes: &[u8]) -> R {
    serde_json::from_slice(bytes)
        .expect("only RegistryState::register_applied writes what current points at")
}

/// The registry's HTTP surface under `base` (`/peers`, `/proxy-peers`):
/// `POST`/`GET {base}`, `GET {base}/subscribe`, `GET`/`DELETE {base}/{name}`.
pub fn router<R: Registration>(state: RegistryState<R>, base: &str) -> Router {
    Router::new()
        .route(base, axum::routing::post(register::<R>).get(list::<R>))
        .route(&format!("{base}/subscribe"), get(subscribe::<R>))
        .route(
            &format!("{base}/{{name}}"),
            get(get_one::<R>).delete(delete_one::<R>),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            gsp_http::server::BearerAuth::new(state.auth_token.as_deref()),
            gsp_http::server::require_bearer,
        ))
        .with_state(state)
}

#[derive(Serialize)]
struct SubmitResponse {
    revision: u64,
    tunnel_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tunnel_network: Option<String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

fn unprocessable(error: String) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(ErrorResponse { error }),
    )
        .into_response()
}

fn not_registered(noun: &str, name: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("no {noun} registered as {name:?}"),
        }),
    )
        .into_response()
}

/// `POST {base}` — body is one JSON registration. Validated before it ever
/// reaches the store, same posture as `crate::intent::api::submit_intent`.
async fn register<R: Registration>(
    State(state): State<RegistryState<R>>,
    OriginalUri(uri): OriginalUri,
    body: String,
) -> Response {
    let words = wording(R::ROLE);
    let mut reg: R = match serde_json::from_str(&body) {
        Ok(reg) => reg,
        Err(e) => return unprocessable(format!("malformed {} registration: {e}", words.noun)),
    };
    if let Err(e) = reg.validate() {
        return unprocessable(e);
    }
    if let Some(ha) = &state.ha {
        return ha::register(&state, ha, reg, uri.path(), body).await;
    }

    let guard = state
        .write_lock
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let assignment =
        match state
            .book
            .claim(R::ROLE, reg.name(), reg.requested_address(), unix_secs())
        {
            Ok(a) => a,
            Err(e) => return claim_error_response(&e),
        };
    // Note: the claim above is kept even if the backends below are rejected
    // (the owner's corrected retry gets the same address, Review Focus 1), and
    // also if storing the registration fails afterwards (a `5xx`; the retry
    // finds the claim idempotent). A stream of distinct names with bad backends
    // can therefore use up the pool; the endpoint is bearer-gated and
    // `DELETE` (or the stale-address warning) is the remedy.
    if let Some(backends) = reg.backends_mut() {
        *backends = match expand_backends(backends, assignment.address) {
            Ok(b) => b,
            Err(e) => {
                return claim_error_response(&ClaimError::Rejected(Rejection::BackendHost(e)))
            }
        };
    }
    reg.set_tunnel_address(assignment.address);

    let registered = state.register_applied(&reg, None);
    drop(guard);
    match registered {
        Ok(Applied::Written(revision)) => {
            tracing::info!(
                revision,
                name = %reg.name(),
                address = %assignment.address,
                "registered a {}",
                words.kind
            );
            (
                StatusCode::OK,
                Json(SubmitResponse {
                    revision,
                    tunnel_address: assignment.address.to_string(),
                    tunnel_network: state.book.network().map(|n| n.to_string()),
                }),
            )
                .into_response()
        }
        Ok(Applied::AlreadyApplied) => store_error_response(words.log, StoreError::UnexpectedSkip),
        Err(e) => store_error_response(words.log, e),
    }
}

/// `GET {base}` — every name's current registration.
async fn list<R: Registration>(State(state): State<RegistryState<R>>) -> Response {
    match state.all_current() {
        Ok(regs) => Json(regs).into_response(),
        Err(e) => store_error_response(wording(R::ROLE).log, e),
    }
}

/// `GET {base}/{name}` — one name's current registration, `404` if it has
/// never registered.
async fn get_one<R: Registration>(
    State(state): State<RegistryState<R>>,
    Path(name): Path<String>,
) -> Response {
    let words = wording(R::ROLE);
    match state.current_for(&name) {
        Ok(Some(reg)) => Json(reg).into_response(),
        Ok(None) => not_registered(words.noun, &name),
        Err(e) => store_error_response(words.log, e),
    }
}

#[derive(Serialize)]
struct DeleteResponse {
    revision: u64,
    released: Option<String>,
}

/// `DELETE {base}/{name}` — releases the name's tunnel address and tells
/// subscribers (a tombstone) to drop its WireGuard peer. `404` only when the
/// name is unknown everywhere (no current registration *and* no address), so a
/// retry after a crash still completes.
async fn delete_one<R: Registration>(
    State(state): State<RegistryState<R>>,
    OriginalUri(uri): OriginalUri,
    Path(name): Path<String>,
) -> Response {
    if let Some(ha) = &state.ha {
        return ha::release::<R>(ha, name, uri.path()).await;
    }
    let words = wording(R::ROLE);
    let guard = state
        .write_lock
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let has_current = match state.current.contains_key(name.as_bytes()) {
        Ok(b) => b,
        Err(e) => return store_error_response(words.log, StoreError::from(e)),
    };
    let has_address = match state.book.get(R::ROLE, &name) {
        Ok(a) => a.is_some(),
        Err(e) => return claim_error_response(&e),
    };
    if !has_current && !has_address {
        return not_registered(words.noun, &name);
    }
    let revision = match state.remove_applied(&name, None) {
        Ok(Applied::Written(r)) => r,
        Ok(Applied::AlreadyApplied) => {
            return store_error_response(words.log, StoreError::UnexpectedSkip)
        }
        Err(e) => return store_error_response(words.log, e),
    };
    let released = match state.book.release(R::ROLE, &name) {
        Ok(a) => a,
        Err(e) => return claim_error_response(&e),
    };
    drop(guard);
    tracing::info!(revision, name = %name, address = ?released, "released a {}", words.kind);
    (
        StatusCode::OK,
        Json(DeleteResponse {
            revision,
            released: released.map(|a| a.to_string()),
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SubscribeParams {
    since: Option<u64>,
}

/// `GET {base}/subscribe?since=<revision>` — the exact catch-up-then-tail
/// shape `crate::intent::api::subscribe` uses, applied to this registry's
/// log. Every event is a full registration or a tombstone
/// ([`event_payload`]); a subscriber keeps its own
/// latest-by-name view, exactly like the `current` tree.
async fn subscribe<R: Registration>(
    State(state): State<RegistryState<R>>,
    Query(params): Query<SubscribeParams>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel(16);
    let updates = state.updates.subscribe();
    tokio::spawn(
        subscribe_worker(state.store.clone(), updates, params.since.unwrap_or(0), tx).instrument(
            tracing::info_span!("subscribe", registry = wording(R::ROLE).log),
        ),
    );

    let events = ReceiverStream::new(rx).map(|(revision, bytes)| {
        Ok(Event::default().data(event_payload(revision, &bytes).to_string()))
    });
    Sse::new(events).keep_alive(KeepAlive::default())
}

pub(crate) async fn subscribe_worker(
    store: Arc<Store>,
    mut updates: broadcast::Receiver<u64>,
    since: u64,
    tx: mpsc::Sender<(u64, RevisionBytes)>,
) {
    let mut last_sent = since;

    if !catch_up(&store, &mut last_sent, &tx).await {
        return;
    }

    loop {
        match updates.recv().await {
            Ok(revision) if revision <= last_sent => {}
            // Replay from the cursor rather than fetching just `revision`: a
            // snapshot install wakes once, for its newest revision, and the
            // ones before it must still be sent.
            Ok(_) => {
                if !catch_up(&store, &mut last_sent, &tx).await {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "registry subscriber lagged; replaying from the store"
                );
                if !catch_up(&store, &mut last_sent, &tx).await {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn catch_up(
    store: &Store,
    last_sent: &mut u64,
    tx: &mpsc::Sender<(u64, RevisionBytes)>,
) -> bool {
    let revisions = match store.revisions_after(*last_sent) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "store error building the registry catch-up range");
            return false;
        }
    };
    for (revision, bytes) in revisions {
        if tx.send((revision, bytes)).await.is_err() {
            return false;
        }
        *last_sent = revision;
    }
    true
}

/// Log payload for a deleted registration. Deliberately not a
/// [`Registration`]: `current` never points at a tombstone, so only
/// subscribers (via [`event_payload`]) ever read one.
pub(crate) fn tombstone_bytes(name: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "removed": name }))
        .expect("a json! object always serializes")
}

/// The SSE `data:` payload for one log entry:
/// `{"revision":N,"registration":{…}}` for a registration,
/// `{"revision":N,"removed":{"name":"…"}}` for a tombstone.
pub(crate) fn event_payload(revision: u64, bytes: &[u8]) -> serde_json::Value {
    let value: serde_json::Value = serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null);
    match value.get("removed").and_then(|v| v.as_str()) {
        Some(name) => serde_json::json!({ "revision": revision, "removed": { "name": name } }),
        None => serde_json::json!({ "revision": revision, "registration": value }),
    }
}

#[allow(clippy::needless_pass_by_value)] // used as a `map_err` callback, which hands the error over by value
fn store_error_response(registry: &str, e: StoreError) -> Response {
    tracing::error!(error = %e, "store error serving the {registry} API");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: e.to_string(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peers::PeerRegistration;

    fn reg(name: &str) -> PeerRegistration {
        PeerRegistration {
            name: name.into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: None,
            backends: vec![],
            tunnel_address: Some("10.60.0.1".into()),
        }
    }

    fn state() -> (RegistryState<PeerRegistration>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("peers")).unwrap());
        let book = Arc::new(AddressBook::open(&dir.path().join("addresses"), None).unwrap());
        (RegistryState::new(store, None, book), dir)
    }

    #[test]
    fn current_is_written_in_the_log_transaction() {
        let (state, _dir) = state();
        let applied = state.register_applied(&reg("home"), Some(7)).unwrap();
        assert_eq!(applied, Applied::Written(1));
        assert_eq!(state.store.applied_index().unwrap(), Some(7));
        assert_eq!(state.current_entry("home").unwrap(), Some((1, reg("home"))));

        // A replay of the same index writes nothing.
        assert_eq!(
            state.register_applied(&reg("home"), Some(7)).unwrap(),
            Applied::AlreadyApplied
        );
        assert_eq!(state.store.current_revision().unwrap(), Some(1));
    }

    #[test]
    fn remove_drops_current_in_the_tombstone_transaction() {
        let (state, _dir) = state();
        state.register_applied(&reg("home"), Some(1)).unwrap();
        assert_eq!(
            state.remove_applied("home", Some(2)).unwrap(),
            Applied::Written(2)
        );
        assert_eq!(state.store.applied_index().unwrap(), Some(2));
        assert_eq!(state.current_for("home").unwrap(), None);
        let tombstone = state.store.get(2).unwrap().unwrap();
        assert_eq!(
            crate::registry::event_payload(2, &tombstone)["removed"]["name"],
            "home"
        );
        assert_eq!(
            state.remove_applied("home", Some(2)).unwrap(),
            Applied::AlreadyApplied
        );
    }

    #[test]
    fn a_write_without_an_index_records_no_applied_index() {
        let (state, _dir) = state();
        assert_eq!(
            state.register_applied(&reg("home"), None).unwrap(),
            Applied::Written(1)
        );
        assert_eq!(
            state.register_applied(&reg("home"), None).unwrap(),
            Applied::Written(2)
        );
        assert_eq!(state.store.applied_index().unwrap(), None);
        assert_eq!(state.current_entry("home").unwrap(), Some((2, reg("home"))));
    }

    #[test]
    fn replace_installs_a_snapshot_with_its_revision_numbers() {
        let (src, _s) = state();
        src.register_applied(&reg("a"), Some(1)).unwrap();
        src.register_applied(&reg("b"), Some(2)).unwrap();
        src.remove_applied("a", Some(3)).unwrap();
        let snapshot = src.snapshot().unwrap();
        assert_eq!(snapshot.current, vec![("b".to_string(), 2)]);
        assert_eq!(snapshot.applied_index, Some(3));

        let (dst, _d) = state();
        dst.register_applied(&reg("stale"), Some(9)).unwrap();
        dst.replace(&snapshot).unwrap();
        assert_eq!(dst.snapshot().unwrap(), snapshot);
        assert_eq!(dst.current_for("stale").unwrap(), None);
        assert_eq!(dst.current_entry("b").unwrap(), Some((2, reg("b"))));
        // The next write continues the log after the installed revisions.
        assert_eq!(
            dst.register_applied(&reg("c"), Some(4)).unwrap(),
            Applied::Written(4)
        );
    }

    #[test]
    fn event_payload_distinguishes_a_registration_from_a_tombstone() {
        let reg = serde_json::to_vec(&reg("home-origin")).unwrap();
        let p = event_payload(3, &reg);
        assert_eq!(p["revision"], 3);
        assert_eq!(p["registration"]["name"], "home-origin");
        assert!(p.get("removed").is_none());

        let p = event_payload(4, &tombstone_bytes("home-origin"));
        assert_eq!(p["revision"], 4);
        assert_eq!(p["removed"]["name"], "home-origin");
        assert!(p.get("registration").is_none());
    }

    #[tokio::test]
    async fn subscribe_worker_replays_every_revision_behind_a_single_wake() {
        // A snapshot install wakes subscribers once, for the newest
        // revision; the ones before it must still reach the subscriber.
        let (state, _d) = state();
        state.register_applied(&reg("a"), None).unwrap();
        let (wake, updates) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(state.store.clone(), updates, 0, tx));
        assert_eq!(rx.recv().await.unwrap().0, 1);

        state.register_applied(&reg("b"), None).unwrap();
        state.register_applied(&reg("c"), None).unwrap();
        wake.send(3).unwrap();
        assert_eq!(rx.recv().await.unwrap().0, 2);
        assert_eq!(rx.recv().await.unwrap().0, 3);
    }
}
