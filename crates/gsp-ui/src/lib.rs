//! `gsp-ui` — the operator dashboard's backend-for-frontend (BFF).
//!
//! Design: [`docs/10-distributed-control-plane.md`](../../../docs/10-distributed-control-plane.md)
//! ("The admin GUI"). Slice plan: [`docs/08-roadmap.md`](../../../docs/08-roadmap.md)
//! phase 10+11, slices 11a-11f.
//!
//! **A dedicated process, not hosted in the controller or the aggregator.**
//! The GUI needs both: reads/operational verbs from `gsp-aggregator`,
//! config/revisions from `gsp-controller`. `gsp-ui` holds *both* services'
//! machine bearer tokens itself and calls each on the operator's behalf -
//! the browser never holds a bearer token for anything, only a session
//! cookie for `gsp-ui` itself. `gsp-ui` authorizes nothing beyond "is this a
//! valid session" - it is not a third authority, it has no store and no
//! fleet data of its own; nothing here outlives a restart beyond active
//! sessions (same "ephemeral by design" posture `gsp-aggregator` already
//! has).
//!
//! Slice 11b: [`session::SessionStore`] + `POST /ui/login`/`/ui/logout`
//! ([`api`]) + [`auth::require_session`]. Slice 11c ([`aggregator_proxy`]):
//! fleet reads + the slice-9 operational verbs, proxied to `gsp-aggregator`.
//! Slice 11d ([`fleet_feed`] + [`ws`]): a single shared subscription to the
//! aggregator's `/fleet/subscribe` SSE feed, fanned out to every connected
//! browser over `GET /ws/fleet`. Slice 11e ([`controller_proxy`]): the
//! controller's config API (submit, revision history/diff, rollback) -
//! phase 10's "full management" GUI level. Slice 11f: the actual React +
//! Vite + TypeScript frontend, in `web/` - a standalone `npm` project (own
//! `package.json`, never a Cargo workspace member; `make ui` builds it),
//! served by this binary's `--static-dir` as a fallback under whatever the
//! API routes above don't claim.

pub mod aggregator_proxy;
pub mod api;
pub mod auth;
pub mod controller_proxy;
pub mod fleet_feed;
mod proxy_util;
pub mod session;
pub mod ws;
