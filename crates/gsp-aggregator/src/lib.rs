//! `gsp-aggregator` — the fleet read/operational-verb path for a `gsp`
//! fleet, separate from `gsp-controller`'s Tier-1 write path.
//!
//! Design: [`docs/10-distributed-control-plane.md`](../../../docs/10-distributed-control-plane.md)
//! ("The aggregator"). Slice plan: [`docs/08-roadmap.md`](../../../docs/08-roadmap.md)
//! phase 10+11.
//!
//! **Stateless and ephemeral by design** — unlike the controller's `sled`
//! store, [`ingest::IngestStore`] is a plain in-memory map, never persisted.
//! That's not a shortcut for this PoC; it's the actual design: the
//! aggregator carries no authority and forgets nothing important, because
//! every fact it holds is a proxy's own state, re-pushed on the next tick.
//! Restarting it loses nothing that isn't already about to arrive again.
//!
//! **This first release is a single tier**: one process, `gsp` instances
//! push directly to it. The recursive aggregator-of-aggregators hierarchy in
//! `docs/10` ("Fleet topology") is phase 12 — additive on top of this, not
//! required to make this crate work.
//!
//! Slice 6 (this module set): [`ingest::IngestStore`] + `POST /ingest`
//! ([`api`]). Slice 7 is `gsp`'s push client (in the `gsp` binary). Slice 8
//! adds the `/fleet/*` read endpoints over the same store; slice 9 adds
//! intent-verb fan-out; slice 10 adds bearer-token auth (mirroring
//! `gsp-controller`'s).

pub mod api;
pub mod ingest;
mod util;
