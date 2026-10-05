//! `POST /config` / `GET /config` — the config submit API (slice 2). Every
//! submission runs the **same `gsp_config::parse_str`** (parse + `validate()`)
//! a proxy runs on a file reload before it ever reaches [`Store::put`]; a
//! rejected submission never touches the store, so the previous revision
//! stays current — the same "bad reload keeps the old snapshot" rule the
//! proxy already has, one hop earlier.
//!
//! `GET /config` returns the current revision's raw text. Revision history /
//! diff / rollback (slice 5) build on top of the same [`Store`].
//!
//! `GET /config/subscribe?since=<revision>` (slice 3) is the pull side of
//! Tier 1 (`docs/10` principle 5): a subscriber gets the catch-up range
//! (every revision after `since`, from [`Store::revisions_after`]) then a
//! live tail of whatever [`submit_config`] accepts next, as
//! [server-sent events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events).
//! A lagging subscriber (slow reader vs. a burst of submissions) just
//! re-runs the catch-up query from wherever it left off — `Store` never
//! forgets a revision, so there is no delivery state to track here.
//!
//! Slice 5 adds `GET /config/revisions` (history), `GET
//! /config/revisions/{revision}` (one past revision's raw text), `GET
//! /config/revisions/{revision}/diff[?against=<revision>]` (a line diff
//! against `current`, or another revision), and `POST
//! /config/rollback/{revision}` — which **never rewrites history**, it
//! re-submits that revision's bytes as a brand new one through the same
//! validate-then-[`Store::put`] path `submit_config` uses, so a subscriber
//! sees rollback as just another ordinary revision, no special-casing
//! needed anywhere else. The whole surface is gated by
//! [`crate::auth::require_bearer`] when `AppState::auth_token` is set.
//!
//! Slice 7 (`docs/10` "Staged / canary rollout (design)") adds **staged
//! rollout on top of the same single, never-forked revision log**: `POST
//! /config?stage=canary&group=<name>` writes a revision that's visible only
//! to subscribers reporting that `group`; `POST /config/promote/{revision}`
//! is the one deliberate exception to "every change is a new revision" — it
//! flips an existing revision's visibility in place rather than minting a
//! new one, since promoting isn't new content. [`Stage`] (a small
//! `{promoted, canary_groups}` record per revision, in its own `stage` tree
//! in the same `sled` database `Store::db` opened, never folded into the
//! revision bytes) is the whole mechanism; "current for group G" is the
//! highest revision that's `promoted` or has `G` in `canary_groups`, found
//! by scanning backward from the top of the log — `Store` itself gains
//! nothing new, it's still exactly the content-agnostic log slice 1 built.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use crate::role::{Role, RoleHandle};
use crate::store::{RevisionBytes, Store, StoreError};

/// Capacity of the update-notification broadcast: how many accepted
/// submissions can land between two ticks of a subscriber's tail loop before
/// it has to fall back to re-querying the store. Generous for a control
/// plane (submissions are rare, human-paced events).
const UPDATES_CAPACITY: usize = 64;

/// A revision's rollout visibility (phase 12 slice 7). Stored per revision
/// in `AppState::stage`, a `sled` tree sibling to `Store`'s own — never
/// folded into the revision bytes `Store` holds, so `Store` stays exactly
/// as content-agnostic as it was before this slice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Stage {
    /// Visible to every subscriber, regardless of `canary_groups`.
    pub promoted: bool,
    /// Visible additionally to a subscriber reporting one of these groups,
    /// even while `promoted` is `false`. A single group per submission in
    /// this release (`POST /config` takes one `?group=`) — the type stays a
    /// `Vec` so `promote`/storage need no shape change if multi-group
    /// canary is added later.
    pub canary_groups: Vec<String>,
}

impl Stage {
    /// Every plain `POST /config` (no `?stage=canary`) and every rollback
    /// gets this — visible to everyone immediately, byte-for-byte the only
    /// behavior that existed before this slice.
    pub fn promoted() -> Self {
        Stage {
            promoted: true,
            canary_groups: Vec::new(),
        }
    }

    /// Visible to nobody: what a revision with a missing or unreadable stage
    /// entry is treated as.
    pub fn hidden() -> Self {
        Stage {
            promoted: false,
            canary_groups: Vec::new(),
        }
    }

    fn canary(group: String) -> Self {
        Stage {
            promoted: false,
            canary_groups: vec![group],
        }
    }

    fn visible_to(&self, group: Option<&str>) -> bool {
        self.promoted || group.is_some_and(|g| self.canary_groups.iter().any(|c| c == g))
    }
}

fn encode_rev(rev: u64) -> [u8; 8] {
    rev.to_be_bytes()
}

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    /// Per-revision [`Stage`], keyed the same way `Store`'s own trees are —
    /// see the module doc.
    pub stage: sled::Tree,
    /// Notifies every live subscriber of a newly accepted revision number;
    /// the subscriber re-fetches the bytes from `store` itself (the channel
    /// only ever carries a `u64`, never the config text).
    pub updates: broadcast::Sender<u64>,
    /// Bearer token every `/config*` request must present, or `None` to
    /// leave the API open (`crate::auth`).
    pub auth_token: Option<Arc<str>>,
    /// `standalone` (default) accepts writes directly; `slave` (phase 12
    /// slice 1) never does — every revision it holds arrived via
    /// [`crate::parent_client`], which writes the store directly and is the
    /// only caller allowed to bypass this gate. See `crate::role`.
    pub role: RoleHandle,
    /// Per-revision submitter, phase 12 slice 8's audit trail — the
    /// `X-Actor` header `gsp-ui` sets on a proxied write, if any. Another
    /// sibling `sled` tree, same pattern as `stage`.
    pub actors: sled::Tree,
    /// `Some` when `--ha-peers` is set (phase 12 slice 6): `submit()`
    /// proposes via Raft instead of writing `store` directly. `None` (the
    /// default, `replicas: 1`) is today's behaviour, byte-for-byte —
    /// see `crate::ha`'s module doc for the scope cut (HA and `slave` are
    /// mutually exclusive in this slice).
    pub ha: Option<Arc<crate::ha::HaHandle>>,
}

impl AppState {
    pub fn new(store: Arc<Store>, auth_token: Option<String>, role: RoleHandle) -> Self {
        let (updates, _rx) = broadcast::channel(UPDATES_CAPACITY);
        // A sibling tree in the exact same `sled` database `store` opened —
        // see `Store::db`'s doc.
        let stage = store
            .db()
            .open_tree("stage")
            .expect("opening the stage tree");
        let actors = store
            .db()
            .open_tree("actors")
            .expect("opening the actors tree");
        AppState {
            store,
            stage,
            updates,
            auth_token: auth_token.map(Arc::from),
            role,
            actors,
            ha: None,
        }
    }

    pub fn with_ha(mut self, ha: Option<Arc<crate::ha::HaHandle>>) -> Self {
        self.ha = ha;
        self
    }

    /// Persists `bytes` as a new, immediately-[`Stage::promoted`] revision
    /// and notifies subscribers — the one path
    /// [`crate::parent_client`]'s relay and [`crate::adopt`] use to land a
    /// revision, after each has done its own role check (a local submit
    /// must be rejected first; a parent relay is always allowed regardless
    /// of role). A relayed/adopted revision is always promoted: staging is
    /// an operator decision made at the point of *submission*, and neither
    /// caller here is a submission — a relay is replaying facts the parent
    /// tier already promoted (or not — see `crate::intent::relay`'s doc on
    /// canary not applying to intent); `adopt`'s seed is the parent's
    /// current (i.e. already-promoted) config.
    pub fn apply_revision(&self, bytes: RevisionBytes) -> Result<u64, StoreError> {
        self.apply_revision_with_stage(bytes, Stage::promoted())
    }

    /// [`apply_revision`] with an explicit [`Stage`] but no actor (a relay
    /// or `adopt` seed has no `X-Actor` to carry — it isn't a submission).
    pub fn apply_revision_with_stage(
        &self,
        bytes: RevisionBytes,
        stage: Stage,
    ) -> Result<u64, StoreError> {
        self.apply_revision_with_stage_and_actor(bytes, stage, None)
    }

    /// The general form the others wrap — used by `submit()` (this tier's
    /// own direct writes, `actor` from `X-Actor`) and by
    /// `crate::ha::state_machine` (a committed Raft entry, which already
    /// carries whatever stage/actor the original submission specified).
    pub fn apply_revision_with_stage_and_actor(
        &self,
        bytes: RevisionBytes,
        stage: Stage,
        actor: Option<&str>,
    ) -> Result<u64, StoreError> {
        let mut side = vec![(&self.stage, encode_stage(&stage))];
        if let Some(actor) = actor {
            side.push((&self.actors, actor.as_bytes().to_vec()));
        }
        let revision = self.store.put_with(bytes, &side)?;
        let _ = self.updates.send(revision);
        Ok(revision)
    }

    /// Flips an existing revision's `promoted` to `true` in place — the one
    /// deliberate exception to "every change is a new revision" (see the
    /// module doc). `Ok(false)` if `revision` doesn't exist (the caller
    /// turns that into a `404`); never an error just for "already
    /// promoted" (promoting twice is a no-op, not a conflict).
    pub fn promote_revision(&self, revision: u64) -> Result<bool, StoreError> {
        if self.store.get(revision)?.is_none() {
            return Ok(false);
        }
        let mut stage = self.stage_of(revision);
        stage.promoted = true;
        self.set_stage(revision, &stage)?;
        let _ = self.updates.send(revision);
        Ok(true)
    }

    /// `revision`'s current [`Stage`]. A missing, unreadable or undecodable
    /// entry is [`Stage::hidden`] (fail closed): hiding a revision is
    /// recoverable, leaking a canary fleet-wide is not.
    pub fn stage_of(&self, revision: u64) -> Stage {
        read_stage(&self.stage, revision)
    }

    fn set_stage(&self, revision: u64, stage: &Stage) -> Result<(), StoreError> {
        self.stage
            .insert(encode_rev(revision), encode_stage(stage))?;
        self.stage.flush()?;
        Ok(())
    }

    /// `revision`'s submitter, if `X-Actor` named one at submission time —
    /// phase 12 slice 8's audit trail.
    pub fn actor_of(&self, revision: u64) -> Option<String> {
        self.actors
            .get(encode_rev(revision))
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
    }

    /// Is `revision` visible to a subscriber reporting `group` (`None` =
    /// no canary group, the default / non-canary case)?
    pub fn is_visible(&self, revision: u64, group: Option<&str>) -> bool {
        self.stage_of(revision).visible_to(group)
    }

    /// "Current, for group `group`": the highest revision that's visible to
    /// it — `promoted`, or carrying `group` in its `canary_groups`. An
    /// `O(revisions)` backward scan from the top of the log, not an index —
    /// see the module doc / `docs/10`'s "Staged / canary rollout (design)"
    /// for why that's the right tradeoff at this scale.
    pub fn current_for_group(
        &self,
        group: Option<&str>,
    ) -> Result<Option<(u64, RevisionBytes)>, StoreError> {
        let Some(mut rev) = self.store.current_revision()? else {
            return Ok(None);
        };
        loop {
            if self.is_visible(rev, group) {
                let bytes = self
                    .store
                    .get(rev)?
                    .expect("a revision number from current_revision/the scan always exists");
                return Ok(Some((rev, bytes)));
            }
            if rev == 1 {
                return Ok(None);
            }
            rev -= 1;
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/config", post(submit_config).get(get_current_config))
        .route("/config/subscribe", get(subscribe))
        .route("/config/revisions", get(list_revisions))
        .route("/config/revisions/{revision}", get(get_revision))
        .route("/config/revisions/{revision}/diff", get(diff_revision))
        .route("/config/rollback/{revision}", post(rollback))
        .route("/config/promote/{revision}", post(promote))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_bearer,
        ))
        .with_state(state)
}

#[derive(Serialize)]
struct SubmitResponse {
    revision: u64,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Deserialize)]
struct SubmitParams {
    /// `"canary"` to stage rather than promote immediately; anything else
    /// (including absent) means "promote now," today's only behavior before
    /// this slice.
    stage: Option<String>,
    /// Required when `stage=canary`. One group per submission in this
    /// release — see [`Stage`]'s doc.
    group: Option<String>,
}

/// `POST /config[?stage=canary&group=<name>]` — body is a raw YAML config
/// document, the same shape a proxy's `--config` file has. Validates, then
/// persists on success.
async fn submit_config(
    State(state): State<AppState>,
    Query(params): Query<SubmitParams>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    let actor = actor_header(&headers);
    let stage = match params.stage.as_deref() {
        None => Stage::promoted(),
        Some("canary") => match params.group {
            Some(group) if !group.trim().is_empty() => Stage::canary(group),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "stage=canary requires a non-empty group".into(),
                    }),
                )
                    .into_response()
            }
        },
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("unknown stage {other:?}; expected \"canary\" or omitted"),
                }),
            )
                .into_response()
        }
    };
    let query_suffix = raw_query.map(|q| format!("?{q}")).unwrap_or_default();
    submit(
        &state,
        body,
        stage,
        &format!("/config{query_suffix}"),
        actor,
    )
    .await
}

/// The `X-Actor` header, if present — phase 12 slice 8's audit trail.
/// `gsp-ui` sets this on every write it proxies (the browser session's
/// username, if `--users-file` mode named one); a direct `curl` against
/// this controller's own token simply carries none, which is still valid
/// break-glass access, just a less specific audit entry.
fn actor_header(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("X-Actor")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Validates `text` and persists it (at `stage`, attributed to `actor`) as
/// a new revision on success — shared by [`submit_config`] and [`rollback`]
/// (a rollback is just an immediately-promoted re-submission of an old
/// revision's bytes, never a rewrite of history). Rejects outright on a
/// `slave` tier (`docs/10` "never accepts a write directly") before even
/// parsing — a slave's only source of new revisions is
/// [`crate::parent_client`], which never calls this function.
/// `forward_path` is this write's own route + query string, forwarded
/// byte-for-byte (`actor` along with it, as `X-Actor` again) to the raft
/// leader if this replica isn't it (`crate::ha::client::propose_write`) —
/// irrelevant when HA is off.
async fn submit(
    state: &AppState,
    text: String,
    stage: Stage,
    forward_path: &str,
    actor: Option<String>,
) -> Response {
    if state.role.get() == Role::Slave {
        return slave_rejects_write();
    }

    if let Err(e) = gsp_config::parse_str(&text) {
        tracing::warn!(error = %e, "rejected an invalid config submission");
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response();
    }

    if let Some(ha) = &state.ha {
        return crate::ha::client::propose_write(
            ha,
            crate::ha::WriteRequest::Config {
                bytes: text.clone().into_bytes(),
                stage,
                actor: actor.clone(),
            },
            forward_path,
            text,
            actor.as_deref(),
        )
        .await;
    }

    match state.apply_revision_with_stage_and_actor(text.into_bytes(), stage, actor.as_deref()) {
        Ok(revision) => {
            tracing::info!(revision, ?actor, "accepted a new config revision");
            (StatusCode::OK, Json(SubmitResponse { revision })).into_response()
        }
        Err(e) => store_error_response(e),
    }
}

/// `POST /config/promote/{revision}` — flips a previously-staged revision's
/// `Stage::promoted` to `true` (see the module doc). `404` if the revision
/// never existed; promoting an already-promoted revision is a harmless
/// no-op, not a conflict.
async fn promote(
    State(state): State<AppState>,
    Path(revision): Path<u64>,
    headers: axum::http::HeaderMap,
) -> Response {
    if state.role.get() == Role::Slave {
        return slave_rejects_write();
    }

    if let Some(ha) = &state.ha {
        let actor = actor_header(&headers);
        return crate::ha::client::propose_write(
            ha,
            crate::ha::WriteRequest::Promote(revision),
            &format!("/config/promote/{revision}"),
            String::new(),
            actor.as_deref(),
        )
        .await;
    }

    match state.promote_revision(revision) {
        Ok(true) => {
            tracing::info!(revision, "promoted a config revision");
            (StatusCode::OK, Json(SubmitResponse { revision })).into_response()
        }
        Ok(false) => revision_not_found(revision),
        Err(e) => store_error_response(e),
    }
}

fn slave_rejects_write() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(ErrorResponse {
            error: "this controller is a slave tier and does not accept writes directly; \
                    submit to the root controller instead"
                .into(),
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
struct GetConfigParams {
    /// Reports this instance's canary group, if any — see the module doc.
    /// Absent means "no group": only ever see `promoted` revisions, exactly
    /// today's behavior.
    group: Option<String>,
}

/// `GET /config[?group=<name>]` — the current revision *visible to `group`*
/// (see [`AppState::current_for_group`]), or `404` before anything visible
/// to it has ever landed.
async fn get_current_config(
    State(state): State<AppState>,
    Query(params): Query<GetConfigParams>,
) -> Response {
    match state.current_for_group(params.group.as_deref()) {
        Ok(Some((revision, bytes))) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            (
                StatusCode::OK,
                [("X-Config-Revision", revision.to_string())],
                text,
            )
                .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "no config has been submitted yet".into(),
            }),
        )
            .into_response(),
        Err(e) => store_error_response(e),
    }
}

#[derive(Serialize)]
struct RevisionSummary {
    revision: u64,
    size_bytes: usize,
    current: bool,
    /// Slice 7: this revision's rollout visibility — see [`Stage`].
    promoted: bool,
    canary_groups: Vec<String>,
    /// Slice 8: who submitted it, if `X-Actor` named someone.
    actor: Option<String>,
}

/// `GET /config/revisions` — every revision, oldest first, with its size,
/// whether it's the current (highest-numbered, regardless of promotion)
/// one, and its rollout stage. Reuses `revisions_after(0)` rather than
/// adding a store method that keeps only lengths — revisions are YAML config
/// text (KB-sized), and this is a control-plane, human-paced call, not
/// something worth a dedicated storage path.
async fn list_revisions(State(state): State<AppState>) -> Response {
    let current = match state.store.current_revision() {
        Ok(c) => c,
        Err(e) => return store_error_response(e),
    };
    match state.store.revisions_after(0) {
        Ok(revisions) => {
            let summaries: Vec<RevisionSummary> = revisions
                .into_iter()
                .map(|(revision, bytes)| {
                    let stage = state.stage_of(revision);
                    RevisionSummary {
                        revision,
                        size_bytes: bytes.len(),
                        current: Some(revision) == current,
                        promoted: stage.promoted,
                        canary_groups: stage.canary_groups,
                        actor: state.actor_of(revision),
                    }
                })
                .collect();
            (StatusCode::OK, Json(summaries)).into_response()
        }
        Err(e) => store_error_response(e),
    }
}

/// `GET /config/revisions/{revision}` — one past revision's raw text (the
/// same shape `GET /config` returns for the current one), `404` if it never
/// existed.
async fn get_revision(State(state): State<AppState>, Path(revision): Path<u64>) -> Response {
    match state.store.get(revision) {
        Ok(Some(bytes)) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            (StatusCode::OK, text).into_response()
        }
        Ok(None) => revision_not_found(revision),
        Err(e) => store_error_response(e),
    }
}

#[derive(Deserialize)]
struct DiffParams {
    /// Defaults to the current revision — "what changed between this old
    /// revision and now" is the common question; diffing two arbitrary past
    /// revisions is still possible by passing this explicitly.
    against: Option<u64>,
}

/// `GET /config/revisions/{revision}/diff[?against=<revision>]` — a
/// line-level diff (`similar::TextDiff`) between `revision` and `against`
/// (default: current), rendered as plain text with `+`/`-`/` ` line prefixes.
async fn diff_revision(
    State(state): State<AppState>,
    Path(revision): Path<u64>,
    Query(params): Query<DiffParams>,
) -> Response {
    let old = match state.store.get(revision) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return revision_not_found(revision),
        Err(e) => return store_error_response(e),
    };

    let (against, new) = match params.against {
        Some(rev) => match state.store.get(rev) {
            Ok(Some(bytes)) => (rev, bytes),
            Ok(None) => return revision_not_found(rev),
            Err(e) => return store_error_response(e),
        },
        None => match state.store.current() {
            Ok(Some((rev, bytes))) => (rev, bytes),
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse {
                        error: "no current config to diff against".into(),
                    }),
                )
                    .into_response()
            }
            Err(e) => return store_error_response(e),
        },
    };

    let old_text = String::from_utf8_lossy(&old).into_owned();
    let new_text = String::from_utf8_lossy(&new).into_owned();
    let diff = similar::TextDiff::from_lines(&old_text, &new_text);
    let mut out = format!("--- revision {revision}\n+++ revision {against}\n");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => '-',
            similar::ChangeTag::Insert => '+',
            similar::ChangeTag::Equal => ' ',
        };
        out.push(sign);
        out.push_str(change.as_str().unwrap_or(""));
    }
    (StatusCode::OK, out).into_response()
}

/// `POST /config/rollback/{revision}` — re-submits an old revision's exact
/// bytes as a brand-new one (see the module doc: this never rewrites
/// history). Same response shape as `POST /config`.
async fn rollback(
    State(state): State<AppState>,
    Path(revision): Path<u64>,
    headers: axum::http::HeaderMap,
) -> Response {
    match state.store.get(revision) {
        Ok(Some(bytes)) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            // A rollback is always immediately promoted — it's presumably
            // urgent, never something to stage (see the module doc).
            let resp = submit(
                &state,
                text,
                Stage::promoted(),
                "/config",
                actor_header(&headers),
            )
            .await;
            tracing::info!(from_revision = revision, "rolled back");
            resp
        }
        Ok(None) => revision_not_found(revision),
        Err(e) => store_error_response(e),
    }
}

fn revision_not_found(revision: u64) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("no revision {revision}"),
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SubscribeParams {
    since: Option<u64>,
    /// This subscriber's canary group, if any — see the module doc. Absent
    /// means only ever see `promoted` revisions.
    group: Option<String>,
}

/// `GET /config/subscribe?since=<revision>[&group=<name>]` — see the module
/// doc. Spawns [`subscribe_worker`] to do the actual catch-up + tail work
/// and turns its output into SSE `Event`s; the split keeps the worker's
/// logic (the part worth testing) free of any HTTP/SSE framing.
async fn subscribe(
    State(state): State<AppState>,
    Query(params): Query<SubscribeParams>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel(16);
    let updates = state.updates.subscribe();
    tokio::spawn(subscribe_worker(
        state.store,
        state.stage,
        params.group,
        updates,
        params.since.unwrap_or(0),
        tx,
    ));

    let events = ReceiverStream::new(rx).map(|(revision, bytes)| {
        let config = String::from_utf8_lossy(&bytes).into_owned();
        let payload = serde_json::json!({ "revision": revision, "config": config });
        Ok(Event::default().data(payload.to_string()))
    });
    Sse::new(events).keep_alive(KeepAlive::default())
}

/// Sends the catch-up range for `since`, then tails `updates` for whatever
/// [`submit_config`]/[`promote`] accept or change next, forever (until the
/// receiving end of `tx` drops — the client disconnected). A
/// [`broadcast::error::RecvError::Lagged`] (the worker fell behind a burst
/// of submissions) just re-runs the catch-up query from wherever it last
/// got to — every revision lives in `store` forever, so nothing is lost,
/// only replayed.
///
/// Slice 7: every candidate revision is filtered through
/// [`AppState::is_visible`] for `group` before being sent, and `last_sent`
/// only ever advances *past* an invisible one — a revision this subscriber
/// can't see yet is simply left for the next signal (its own later
/// `promote`, or any subsequent submission) to re-evaluate, rather than
/// being permanently skipped. For `group: None` and every revision that
/// predates this slice (always `Stage::promoted`), every candidate is
/// trivially visible and this is byte-for-byte the pre-slice-7 behavior.
async fn subscribe_worker(
    store: Arc<Store>,
    stage: sled::Tree,
    group: Option<String>,
    mut updates: broadcast::Receiver<u64>,
    since: u64,
    tx: mpsc::Sender<(u64, RevisionBytes)>,
) {
    let group = group.as_deref();
    let mut last_sent = since;

    if !catch_up(&store, &stage, group, &mut last_sent, &tx).await {
        return;
    }

    loop {
        match updates.recv().await {
            Ok(revision) if revision <= last_sent => {} // already sent by catch-up
            Ok(revision) if !is_visible(&stage, revision, group) => {} // not (yet) ours to see
            Ok(revision) => match store.get(revision) {
                Ok(Some(bytes)) => {
                    if tx.send((revision, bytes)).await.is_err() {
                        return;
                    }
                    last_sent = revision;
                }
                Ok(None) => {
                    tracing::warn!(revision, "update notification for a revision store lost");
                }
                Err(e) => {
                    tracing::error!(error = %e, "store error tailing config updates");
                    return;
                }
            },
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "subscriber lagged; replaying from the store");
                if !catch_up(&store, &stage, group, &mut last_sent, &tx).await {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Sends every *visible-to-`group`* revision after `*last_sent`, advancing
/// it only for the ones actually sent — see [`subscribe_worker`]'s doc on
/// why an invisible one doesn't advance the cursor. Returns `false` if the
/// receiver dropped (stop the worker).
async fn catch_up(
    store: &Store,
    stage: &sled::Tree,
    group: Option<&str>,
    last_sent: &mut u64,
    tx: &mpsc::Sender<(u64, RevisionBytes)>,
) -> bool {
    let revisions = match store.revisions_after(*last_sent) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "store error building the catch-up range");
            return false;
        }
    };
    for (revision, bytes) in revisions {
        if !is_visible(stage, revision, group) {
            continue;
        }
        if tx.send((revision, bytes)).await.is_err() {
            return false;
        }
        *last_sent = revision;
    }
    true
}

/// Free-function form of [`AppState::is_visible`] — `subscribe_worker`
/// holds a bare `stage` tree (cloned out of `AppState` once, at subscribe
/// time) rather than a whole `AppState`, so its tests don't need to spin up
/// everything else `AppState` carries just to check visibility.
fn is_visible(stage: &sled::Tree, revision: u64, group: Option<&str>) -> bool {
    read_stage(stage, revision).visible_to(group)
}

fn encode_stage(stage: &Stage) -> Vec<u8> {
    serde_json::to_vec(stage).expect("Stage always serializes")
}

/// Reads `revision`'s [`Stage`], failing closed ([`Stage::hidden`], logged)
/// when the entry is missing, unreadable or undecodable.
fn read_stage(stage: &sled::Tree, revision: u64) -> Stage {
    match stage.get(encode_rev(revision)) {
        Ok(Some(v)) => serde_json::from_slice(&v).unwrap_or_else(|e| {
            tracing::error!(revision, error = %e, "undecodable stage entry; hiding the revision");
            Stage::hidden()
        }),
        Ok(None) => {
            tracing::error!(revision, "revision has no stage entry; hiding it");
            Stage::hidden()
        }
        Err(e) => {
            tracing::error!(revision, error = %e, "unreadable stage entry; hiding the revision");
            Stage::hidden()
        }
    }
}

fn store_error_response(e: StoreError) -> Response {
    tracing::error!(error = %e, "store error serving the config API");
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
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        (
            AppState::new(store, None, RoleHandle::new(Role::Standalone)),
            dir,
        )
    }

    const VALID_CONFIG: &str = r#"
pools:
  - name: mc
    targets: ["127.0.0.1:25566"]
listeners:
  - name: main
    bind: "0.0.0.0:25565"
    protocol: tcp
    pool: mc
"#;

    #[tokio::test]
    async fn get_config_before_any_submission_is_404() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_valid_submission_is_accepted_and_becomes_current() {
        let (state, _dir) = test_state();
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/config")
                    .body(Body::from(VALID_CONFIG))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("X-Config-Revision").unwrap(), "1");
    }

    #[tokio::test]
    async fn an_invalid_submission_is_rejected_and_leaves_current_untouched() {
        let (state, _dir) = test_state();
        let app = router(state);

        app.clone()
            .oneshot(
                Request::post("/config")
                    .body(Body::from(VALID_CONFIG))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(
                Request::post("/config")
                    .body(Body::from("not: [valid, yaml: at all"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let resp = app
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.headers().get("X-Config-Revision").unwrap(),
            "1",
            "the rejected submission must not have bumped the current revision"
        );
    }

    /// The `stage` tree in `store`'s own database, as `AppState::new` opens it.
    fn test_stage_tree(store: &Store) -> sled::Tree {
        store.db().open_tree("stage").unwrap()
    }

    /// Puts `bytes` as a promoted revision — a revision with no stage entry
    /// is hidden (fail closed), so bare-`Store` tests must stage explicitly.
    fn put_promoted(store: &Store, bytes: &[u8]) -> u64 {
        let tree = test_stage_tree(store);
        store
            .put_with(bytes.to_vec(), &[(&tree, encode_stage(&Stage::promoted()))])
            .unwrap()
    }

    #[tokio::test]
    async fn subscribe_worker_sends_the_catch_up_range_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = put_promoted(&store, b"one");
        let rev2 = put_promoted(&store, b"two");

        let (_updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(
            store.clone(),
            test_stage_tree(&store),
            None,
            updates_rx,
            0,
            tx,
        ));

        assert_eq!(rx.recv().await.unwrap(), (rev1, b"one".to_vec()));
        assert_eq!(rx.recv().await.unwrap(), (rev2, b"two".to_vec()));
    }

    #[tokio::test]
    async fn subscribe_worker_tails_a_revision_accepted_after_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = put_promoted(&store, b"one");

        let (updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(
            store.clone(),
            test_stage_tree(&store),
            None,
            updates_rx,
            0,
            tx,
        ));

        assert_eq!(rx.recv().await.unwrap(), (rev1, b"one".to_vec()));

        let rev2 = put_promoted(&store, b"two");
        updates_tx.send(rev2).unwrap();
        assert_eq!(rx.recv().await.unwrap(), (rev2, b"two".to_vec()));
    }

    #[tokio::test]
    async fn subscribe_worker_since_a_revision_skips_everything_up_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = put_promoted(&store, b"one");
        let rev2 = put_promoted(&store, b"two");

        let (_updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(
            store.clone(),
            test_stage_tree(&store),
            None,
            updates_rx,
            rev1,
            tx,
        ));

        assert_eq!(rx.recv().await.unwrap(), (rev2, b"two".to_vec()));
    }

    #[tokio::test]
    async fn a_lagged_subscriber_replays_from_the_store_instead_of_losing_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = put_promoted(&store, b"one");

        let (updates_tx, updates_rx) = broadcast::channel(1);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(
            store.clone(),
            test_stage_tree(&store),
            None,
            updates_rx,
            0,
            tx,
        ));

        // Initial catch-up delivers rev1.
        assert_eq!(rx.recv().await.unwrap(), (rev1, b"one".to_vec()));

        // Two more revisions land back-to-back; a capacity-1 broadcast
        // channel can only hold the last notification, so whether or not
        // the worker's next `recv()` actually observes `Lagged` (it may win
        // the race and see `rev2` directly) is a scheduling detail — either
        // way it must end up delivering *both* rev2 and rev3, never skip
        // straight to rev3.
        let rev2 = put_promoted(&store, b"two");
        let rev3 = put_promoted(&store, b"three");
        updates_tx.send(rev2).unwrap();
        updates_tx.send(rev3).unwrap();

        assert_eq!(rx.recv().await.unwrap(), (rev2, b"two".to_vec()));
        assert_eq!(rx.recv().await.unwrap(), (rev3, b"three".to_vec()));
    }

    const OTHER_VALID_CONFIG: &str = r#"
pools:
  - name: mc
    targets: ["127.0.0.1:9999"]
listeners:
  - name: main
    bind: "0.0.0.0:25565"
    protocol: tcp
    pool: mc
"#;

    async fn submit(app: &Router, body: &'static str) -> u64 {
        let resp = app
            .clone()
            .oneshot(Request::post("/config").body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json["revision"].as_u64().unwrap()
    }

    #[tokio::test]
    async fn list_revisions_reports_size_and_marks_current() {
        let (state, _dir) = test_state();
        let app = router(state);
        let rev1 = submit(&app, VALID_CONFIG).await;
        let rev2 = submit(&app, OTHER_VALID_CONFIG).await;

        let resp = app
            .oneshot(
                Request::get("/config/revisions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["revision"], rev1);
        assert_eq!(list[0]["current"], false);
        assert_eq!(list[1]["revision"], rev2);
        assert_eq!(list[1]["current"], true);
        assert_eq!(list[0]["size_bytes"], VALID_CONFIG.len());
    }

    #[tokio::test]
    async fn get_revision_returns_a_past_revisions_text_and_404s_a_missing_one() {
        let (state, _dir) = test_state();
        let app = router(state);
        let rev1 = submit(&app, VALID_CONFIG).await;

        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/config/revisions/{rev1}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes, VALID_CONFIG.as_bytes());

        let resp = app
            .oneshot(
                Request::get("/config/revisions/9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn diff_against_current_shows_the_changed_line() {
        let (state, _dir) = test_state();
        let app = router(state);
        let rev1 = submit(&app, VALID_CONFIG).await;
        submit(&app, OTHER_VALID_CONFIG).await;

        let resp = app
            .oneshot(
                Request::get(format!("/config/revisions/{rev1}/diff"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("-    targets: [\"127.0.0.1:25566\"]"));
        assert!(text.contains("+    targets: [\"127.0.0.1:9999\"]"));
    }

    #[tokio::test]
    async fn rollback_resubmits_the_old_bytes_as_a_new_revision() {
        let (state, _dir) = test_state();
        let app = router(state);
        let rev1 = submit(&app, VALID_CONFIG).await;
        submit(&app, OTHER_VALID_CONFIG).await;

        let resp = app
            .clone()
            .oneshot(
                Request::post(format!("/config/rollback/{rev1}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let rollback_rev = json["revision"].as_u64().unwrap();
        assert_eq!(
            rollback_rev, 3,
            "a rollback is a new revision, never a rewrite of history"
        );

        let resp = app
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes, VALID_CONFIG.as_bytes());
    }

    #[tokio::test]
    async fn rollback_to_a_missing_revision_is_404() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/config/rollback/9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_missing_token_is_unauthorized_when_one_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let state = AppState::new(
            store,
            Some("secret".into()),
            RoleHandle::new(Role::Standalone),
        );
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .oneshot(
                Request::get("/config")
                    .header("Authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_slave_tier_rejects_direct_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        // Seed a revision the way a real slave would (via `apply_revision`,
        // never `submit()`) so the rollback path below has something to
        // find before it hits the role gate.
        store.put(VALID_CONFIG.as_bytes().to_vec()).unwrap();
        let state = AppState::new(store, None, RoleHandle::new(Role::Slave));
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/config")
                    .body(Body::from(VALID_CONFIG))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Rollback is a re-submission under the hood — must be rejected too.
        let resp = app
            .oneshot(
                Request::post("/config/rollback/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn apply_revision_bypasses_the_slave_write_gate() {
        // `crate::parent_client` calls `apply_revision` directly, never
        // `submit()` — this is what lets a slave *hold* revisions while
        // still rejecting direct writes through the HTTP API.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let state = AppState::new(store, None, RoleHandle::new(Role::Slave));
        let revision = state.apply_revision(b"pools: []".to_vec()).unwrap();
        assert_eq!(revision, 1);
        assert_eq!(state.store.current_revision().unwrap(), Some(1));
    }

    #[tokio::test]
    async fn the_right_token_is_admitted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let state = AppState::new(
            store,
            Some("secret".into()),
            RoleHandle::new(Role::Standalone),
        );
        let app = router(state);

        let resp = app
            .oneshot(
                Request::get("/config")
                    .header("Authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // No config submitted yet in this fresh store — 404, not 401, proves
        // the request got *past* the auth layer.
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- Slice 7: staged / canary rollout ---

    #[tokio::test]
    async fn a_plain_submission_is_immediately_promoted_and_visible_to_everyone() {
        let (state, _dir) = test_state();
        let app = router(state);
        submit(&app, VALID_CONFIG).await;

        let resp = app
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("X-Config-Revision").unwrap(), "1");
    }

    #[tokio::test]
    async fn a_canary_submission_is_hidden_from_a_plain_get_and_from_other_groups() {
        let (state, _dir) = test_state();
        let app = router(state);
        submit(&app, VALID_CONFIG).await;

        let resp = app
            .clone()
            .oneshot(
                Request::post("/config?stage=canary&group=region-a")
                    .body(Body::from(OTHER_VALID_CONFIG))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // No group: still sees the last promoted revision (1), not the canary.
        let resp = app
            .clone()
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.headers().get("X-Config-Revision").unwrap(), "1");

        // A different group: same — not enrolled in region-a.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/config?group=region-b")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.headers().get("X-Config-Revision").unwrap(), "1");

        // region-a: sees the canary revision (2).
        let resp = app
            .oneshot(
                Request::get("/config?group=region-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.headers().get("X-Config-Revision").unwrap(), "2");
    }

    #[tokio::test]
    async fn promoting_a_canary_revision_makes_it_visible_to_everyone() {
        let (state, _dir) = test_state();
        let app = router(state);
        submit(&app, VALID_CONFIG).await;
        app.clone()
            .oneshot(
                Request::post("/config?stage=canary&group=region-a")
                    .body(Body::from(OTHER_VALID_CONFIG))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(
                Request::post("/config/promote/2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.headers().get("X-Config-Revision").unwrap(), "2");
    }

    #[tokio::test]
    async fn promoting_a_missing_revision_is_404() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/config/promote/9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn canary_without_a_group_is_a_bad_request() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/config?stage=canary")
                    .body(Body::from(VALID_CONFIG))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn list_revisions_reports_stage() {
        let (state, _dir) = test_state();
        let app = router(state);
        submit(&app, VALID_CONFIG).await;
        app.clone()
            .oneshot(
                Request::post("/config?stage=canary&group=region-a")
                    .body(Body::from(OTHER_VALID_CONFIG))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(
                Request::get("/config/revisions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(list[0]["promoted"], true);
        assert_eq!(list[1]["promoted"], false);
        assert_eq!(list[1]["canary_groups"], serde_json::json!(["region-a"]));
    }

    #[tokio::test]
    async fn an_x_actor_header_is_recorded_and_surfaced() {
        let (state, _dir) = test_state();
        let app = router(state);
        app.clone()
            .oneshot(
                Request::post("/config")
                    .header("X-Actor", "alice")
                    .body(Body::from(VALID_CONFIG))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(
                Request::get("/config/revisions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(list[0]["actor"], "alice");
    }

    #[tokio::test]
    async fn no_x_actor_header_leaves_actor_null() {
        let (state, _dir) = test_state();
        let app = router(state);
        submit(&app, VALID_CONFIG).await;

        let resp = app
            .oneshot(
                Request::get("/config/revisions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(list[0]["actor"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn subscribe_worker_holds_back_an_unpromoted_canary_revision_until_promoted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = put_promoted(&store, b"one");
        let stage_tree = test_stage_tree(&store);
        // rev2 is canary-only, not visible to a group-less subscriber.
        let rev2 = store
            .put_with(
                b"two".to_vec(),
                &[(&stage_tree, encode_stage(&Stage::canary("region-a".into())))],
            )
            .unwrap();

        let (updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(
            store,
            stage_tree.clone(),
            None,
            updates_rx,
            0,
            tx,
        ));

        // Only rev1 shows up in the catch-up range; rev2 stays held back.
        assert_eq!(rx.recv().await.unwrap(), (rev1, b"one".to_vec()));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "an un-promoted canary revision must not reach a group-less subscriber"
        );

        // Promoting rev2 and re-signaling makes it visible on the next tick.
        stage_tree
            .insert(
                encode_rev(rev2),
                serde_json::to_vec(&Stage::promoted()).unwrap(),
            )
            .unwrap();
        updates_tx.send(rev2).unwrap();
        assert_eq!(rx.recv().await.unwrap(), (rev2, b"two".to_vec()));
    }

    #[test]
    fn a_revision_with_no_stage_entry_is_hidden_from_everyone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let stage_tree = store.db().open_tree("stage").unwrap();
        let rev = store.put(b"one".to_vec()).unwrap();
        assert!(!is_visible(&stage_tree, rev, None));
        assert!(!is_visible(&stage_tree, rev, Some("canary")));
    }

    #[test]
    fn an_undecodable_stage_entry_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let stage_tree = store.db().open_tree("stage").unwrap();
        let rev = store.put(b"one".to_vec()).unwrap();
        stage_tree
            .insert(encode_rev(rev), b"not json".to_vec())
            .unwrap();
        assert!(!is_visible(&stage_tree, rev, None));
    }
}
