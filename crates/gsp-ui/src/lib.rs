//! `gsp-ui` — the operator dashboard's backend-for-frontend (BFF).
//!
//! Design: [`docs/10-distributed-control-plane.md`](../../../docs/10-distributed-control-plane.md)
//! ("The admin GUI"). Slice plan: [`docs/08-roadmap.md`](../../../docs/08-roadmap.md)
//! phase 10+11, slices 11a–11f.
//!
//! **A dedicated process, not hosted in the controller or the aggregator.**
//! The GUI needs both: reads/operational verbs from `gsp-aggregator`,
//! config/revisions from `gsp-controller`. `gsp-ui` holds *both* services'
//! machine bearer tokens itself and calls each on the operator's behalf —
//! the browser never holds a bearer token for anything, only a session
//! cookie for `gsp-ui` itself. `gsp-ui` authorizes nothing beyond "is this a
//! valid session" — it is not a third authority, it has no store and no
//! fleet data of its own; nothing here outlives a restart beyond active
//! sessions (same "ephemeral by design" posture `gsp-aggregator` already
//! has).
//!
//! Slice 11b (this module set): [`session::SessionStore`] +
//! `POST /ui/login`/`/ui/logout` ([`api`]) + [`auth::require_session`].
//! Slices 11c–11e (proxying to the aggregator and controller, the WebSocket
//! bridge) aren't built yet; slice 11f is the actual frontend.

pub mod api;
pub mod auth;
pub mod session;
