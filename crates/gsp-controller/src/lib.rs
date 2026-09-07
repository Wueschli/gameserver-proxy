//! `gsp-controller` — the Tier-1 config/intent authority for a `gsp` fleet.
//!
//! Design: [`docs/10-distributed-control-plane.md`](../../../docs/10-distributed-control-plane.md)
//! ("The controller"). Slice plan: [`docs/08-roadmap.md`](../../../docs/08-roadmap.md)
//! phase 10+11.
//!
//! **Phase 10+11 release was a single `standalone` tier**: one process, one
//! embedded store (ADR 20, `docs/09`), no Raft/etcd, no HA. Phase 12 (all
//! slices built) adds, on top of that: an optional `slave` role ([`role`],
//! slice 1) — a `slave` tier never accepts a write directly (`api::submit`
//! rejects with `403`), instead relaying a parent controller's revision
//! stream into its own store via [`parent_client`]; the operator-intent log
//! ([`intent`], slice 3) and its relay (slice 4); intra-tier HA via embedded
//! `openraft` ([`ha`], slice 6); staged/canary rollout ([`api::Stage`], slice
//! 7); post-install adoption ([`adopt`], slice 5); and RBAC/audit — the
//! `X-Actor` field is recorded here, the roles live in `gsp-ui` (slice 8).
//! See `docs/10-distributed-control-plane.md`.
//!
//! Slice 1: [`store::Store`] persists config revisions. Slice 2 ([`api`]):
//! `POST`/`GET /config`, validated with the same `gsp_config::parse_str` a
//! proxy runs on a file reload. Slice 3: `GET /config/subscribe`, the
//! catch-up + change-stream endpoint `gsp --controller` consumes. Slice 5:
//! revision history/diff/rollback endpoints, and [`auth`]'s bearer-token
//! gate on the whole `/config*` surface.

pub mod adopt;
pub mod api;
pub mod auth;
pub mod ha;
pub mod intent;
pub mod parent_client;
pub mod peers;
pub mod proxy_peers;
pub mod role;
pub mod store;
