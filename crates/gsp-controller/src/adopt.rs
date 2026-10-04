//! `POST /admin/adopt` (phase 12 slice 5, `docs/10` "Adoption") — the
//! operator-triggered, one-time role flip that turns a running `standalone`
//! tier into a `slave` of a newly-introduced parent, without a restart.
//!
//! `docs/10` names two correctness details to solve "when this is built,
//! not now" — this slice solves both the same, simple way:
//!
//! - *"The child's own revision history must not outrank the parent's once
//!   adopted."* Solved by **requiring the child to have no revision history
//!   of its own at all**: adoption is refused (`409`) unless both the config
//!   and intent stores are still empty. There is nothing subtler to get
//!   right about "whose history wins" if the child never had one — a
//!   `standalone` tier that has already accepted real writes must be
//!   decommissioned and replaced by a fresh one to join a hierarchy, not
//!   reconciled in place. A future slice could relax this (e.g. by replaying
//!   the child's own history as the *first* revisions accepted from the new
//!   parent), but that's a real design question, not a default to fall into.
//! - *"Adoption should require the child to be quiescent... so a write
//!   doesn't get reordered across the transition."* The same empty-store
//!   requirement gives this for free: an empty store has nothing in flight
//!   to reorder.
//!
//! On success: seeds from the new parent's current config (mirrors
//! `main.rs`'s own slave-startup seed step), flips the shared [`RoleHandle`]
//! to [`Role::Slave`] — instantly visible to `crate::api`'s and
//! `crate::intent::api`'s write gates, since they hold clones of the same
//! handle — then spawns the same `parent_client::run` +
//! `intent::relay::run` tasks `main.rs` would have spawned had `--role
//! slave` been passed at startup. From the outside, a freshly-adopted tier
//! is indistinguishable from one that booted as a slave to begin with.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::intent::api::IntentState;
use crate::role::{Role, RoleHandle};

#[derive(Clone)]
pub struct AdoptState {
    pub role: RoleHandle,
    pub config: Arc<AppState>,
    pub intent: Arc<IntentState>,
    /// Same posture as every other `/…*` surface in this crate: `None`
    /// leaves it open, a token gates it. Reuses the controller's own
    /// `--auth-token` rather than inventing a fourth secret.
    pub auth_token: Option<Arc<str>>,
}

pub fn router(state: AdoptState) -> Router {
    Router::new()
        .route("/admin/adopt", post(adopt))
        .route_layer(axum::middleware::from_fn_with_state(
            gsp_http::server::BearerAuth::new(state.auth_token.as_deref()),
            gsp_http::server::require_bearer,
        ))
        .with_state(state)
}

#[derive(Deserialize)]
struct AdoptRequest {
    parent_url: String,
    #[serde(default)]
    parent_token: Option<String>,
}

#[derive(Serialize)]
struct AdoptResponse {
    /// The parent's config revision this tier seeded from, if the parent
    /// already had one (mirrors `main.rs`'s own startup seed — `None` just
    /// means the parent's config log was empty at adoption time, not an
    /// error; the subscribe tail picks up whatever comes later).
    seeded_config_revision: Option<u64>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

async fn adopt(State(state): State<AdoptState>, Json(req): Json<AdoptRequest>) -> Response {
    if state.role.get() == Role::Slave {
        return conflict("this tier is already a slave");
    }
    let config_empty = matches!(state.config.store.current_revision(), Ok(None));
    let intent_empty = matches!(state.intent.store.current_revision(), Ok(None));
    if !config_empty || !intent_empty {
        return conflict(
            "adoption requires an empty (quiescent) controller — this tier already has its \
             own revision history that would need reconciling first (docs/10 \"Adoption\")",
        );
    }

    let mut seeded_config_revision = None;
    match crate::parent_client::fetch_initial(&req.parent_url, req.parent_token.as_deref()).await {
        Ok(Some((revision, config))) => {
            seeded_config_revision = Some(revision);
            if let Err(e) = state.config.apply_revision(config.into_bytes()) {
                return store_error_response(e);
            }
        }
        Ok(None) => {}
        Err(e) => {
            // Not fatal: the role flip below still happens, and the
            // subscribe loops it spawns will keep retrying — matching how
            // a slave started via `--role slave` behaves if the parent
            // happens to be unreachable at boot.
            tracing::warn!(
                error = %e, parent = %req.parent_url,
                "could not reach the new parent at adoption time; will keep retrying via subscribe"
            );
        }
    }

    state.role.set(Role::Slave);
    tracing::info!(parent = %req.parent_url, "adopted: this tier is now a slave");

    tokio::spawn(crate::parent_client::run(
        req.parent_url.clone(),
        req.parent_token.clone(),
        seeded_config_revision.unwrap_or(0),
        state.config.clone(),
    ));
    tokio::spawn(crate::intent::relay::run(
        req.parent_url,
        req.parent_token,
        state.intent.clone(),
    ));

    (
        StatusCode::OK,
        Json(AdoptResponse {
            seeded_config_revision,
        }),
    )
        .into_response()
}

fn conflict(msg: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(ErrorResponse { error: msg.into() }),
    )
        .into_response()
}

#[allow(clippy::needless_pass_by_value)] // used as a `map_err` callback, which hands the error over by value
fn store_error_response(e: crate::store::StoreError) -> Response {
    tracing::error!(error = %e, "store error during adoption");
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
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use tower::ServiceExt;

    fn test_state() -> (AdoptState, tempfile::TempDir, tempfile::TempDir) {
        let config_dir = tempfile::tempdir().unwrap();
        let intent_dir = tempfile::tempdir().unwrap();
        let role = RoleHandle::new(Role::Standalone);
        let config_store = Arc::new(crate::store::Store::open(config_dir.path()).unwrap());
        let intent_store = Arc::new(crate::store::Store::open(intent_dir.path()).unwrap());
        let config = Arc::new(AppState::new(config_store, None, role.clone()));
        let intent = Arc::new(IntentState::new(intent_store, role.clone(), None));
        (
            AdoptState {
                role,
                config,
                intent,
                auth_token: None,
            },
            config_dir,
            intent_dir,
        )
    }

    /// A minimal mock parent controller: `GET /config` returns one revision.
    async fn spawn_mock_parent() -> String {
        async fn get_config() -> Response {
            (StatusCode::OK, [("X-Config-Revision", "7")], "pools: []").into_response()
        }
        let app = Router::new().route("/config", get(get_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn adopting_an_empty_standalone_tier_flips_it_to_slave() {
        let (state, _c, _i) = test_state();
        let role = state.role.clone();
        let parent_url = spawn_mock_parent().await;
        let app = router(state);

        let resp = app
            .oneshot(
                HttpRequest::post("/admin/adopt")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"parent_url":"{parent_url}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["seeded_config_revision"], 7);
        assert_eq!(role.get(), Role::Slave);
    }

    #[tokio::test]
    async fn adopting_an_already_slave_tier_is_a_conflict() {
        let (state, _c, _i) = test_state();
        state.role.set(Role::Slave);
        let app = router(state);

        let resp = app
            .oneshot(
                HttpRequest::post("/admin/adopt")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"parent_url":"http://127.0.0.1:1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn adopting_a_tier_with_existing_history_is_a_conflict() {
        let (state, _c, _i) = test_state();
        state.config.store.put(b"pools: []".to_vec()).unwrap();
        let app = router(state.clone());

        let resp = app
            .oneshot(
                HttpRequest::post("/admin/adopt")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"parent_url":"http://127.0.0.1:1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(
            state.role.get(),
            Role::Standalone,
            "a refused adoption must not flip the role"
        );
    }

    #[tokio::test]
    async fn after_adoption_a_direct_config_write_is_rejected() {
        let (state, _c, _i) = test_state();
        let config = state.config.clone();
        let parent_url = spawn_mock_parent().await;
        let app = router(state);

        app.oneshot(
            HttpRequest::post("/admin/adopt")
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"parent_url":"{parent_url}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();

        // Exercise the actual config API's own gate, not just the role
        // value — this is what a real client hits.
        let config_router = crate::api::router((*config).clone());
        let resp = config_router
            .oneshot(
                HttpRequest::post("/config")
                    .body(Body::from("pools: []"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}
