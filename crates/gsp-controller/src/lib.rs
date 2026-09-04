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
//! Slice 1 (this module set) is the store only: [`store::Store`] persists
//! config revisions. Slices 2–5 (the `POST /config` submit API, the
//! subscribe/change-stream endpoint, and revision history/rollback) build on
//! top of it in `main.rs`.

pub mod store;
