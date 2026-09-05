//! `gsp-controller` — the Tier-1 config/intent authority for a `gsp` fleet.
//!
//! Design: [`docs/10-distributed-control-plane.md`](../../../docs/10-distributed-control-plane.md)
//! ("The controller"). Slice plan: [`docs/08-roadmap.md`](../../../docs/08-roadmap.md)
//! phase 10+11.
//!
//! **Phase 10+11 release is a single `standalone` tier**: one process, one
//! embedded store (ADR 20, `docs/09`), no Raft/etcd, no HA. Phase 12 slice 1
//! adds an optional `slave` role ([`role`]) on top of that, unchanged
//! otherwise: a `slave` tier never accepts a write directly (`api::submit`
//! rejects with `403`), instead relaying a parent controller's revision
//! stream into its own store via [`parent_client`]. Intra-tier HA
//! (Raft/etcd), RBAC, canary rollout, and adoption remain phase 12 work not
//! yet built — see `docs/10-distributed-control-plane.md`.
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
pub mod role;
pub mod store;
