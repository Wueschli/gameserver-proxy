//! `wayhouse-aggregator` — the fleet read/operational-verb path for a `wayhouse`
//! fleet, separate from `wayhouse-controller`'s Tier-1 write path.
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
//! **Phase 10+11 release is a single tier**: one process, `wayhouse` instances
//! push directly to it. Phase 12 slice 2 adds the recursive
//! aggregator-of-aggregators hierarchy in `docs/10` ("Fleet topology") on
//! top of that: `--parent-url` makes this tier also push its own merged
//! view — every instance it currently knows, namespaced
//! `"{tier_name}/{instance}"` — up to a parent aggregator's `POST /ingest`,
//! via [`parent_push`]. See that module's doc for why namespacing existing
//! instances beats inventing a new "subtree summary" payload shape.
//!
//! Slice 6 (this module set): [`ingest::IngestStore`] + `POST /ingest`
//! ([`api`]). Slice 7 is `wayhouse`'s push client (in the `wayhouse` binary). Slice 8
//! adds the `/fleet/*` read endpoints over the same store; slice 9 adds
//! intent-verb fan-out; slice 10 adds bearer-token auth (mirroring
//! `wayhouse-controller`'s).

pub mod api;
pub mod fanout;
pub mod ingest;
pub mod parent_push;
pub mod target;
pub mod trust;
mod util;
