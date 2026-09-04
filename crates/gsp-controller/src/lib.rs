//! `gsp-controller` — the Tier-1 config/intent authority for a `gsp` fleet.
//!
//! Design: [`docs/10-distributed-control-plane.md`](../../../docs/10-distributed-control-plane.md)
//! ("The controller"). Slice plan: [`docs/08-roadmap.md`](../../../docs/08-roadmap.md)
//! phase 10+11.
//!
//! **This first release is deliberately a single `standalone` tier**: one
//! process, one embedded store (ADR 20, `docs/09`), no Raft/etcd, no `slave`
//! role, no HA. The hierarchy/replication/adoption design in `docs/10` is
//! phase 12 — additive on top of this, not required to make this crate work.
//!
//! Slice 1: [`store::Store`] persists config revisions. Slice 2 ([`api`]):
//! `POST`/`GET /config`, validated with the same `gsp_config::parse_str` a
//! proxy runs on a file reload. Slice 3: `GET /config/subscribe`, the
//! catch-up + change-stream endpoint `gsp --controller` consumes. Slice 5:
//! revision history/diff/rollback endpoints, and [`auth`]'s bearer-token
//! gate on the whole `/config*` surface.

pub mod api;
pub mod auth;
pub mod store;
