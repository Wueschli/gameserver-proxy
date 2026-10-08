//! The ingest payload shape and the in-memory per-instance store it lands
//! in. `wayhouse`'s push client (slice 7) builds one [`IngestPayload`] per tick
//! directly from its live `Snapshot` / `ConnTracker` — no scraping of its
//! own admin API's plaintext output, no intermediate format.
//!
//! Deliberately a summary, not the full live session registry (`GET
//! /sessions` already exists per-instance for that, break-glass style):
//! pool/backend health + admin state, and session *counts*, is what a fleet
//! dashboard actually renders per tick.

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};

use serde::{Deserialize, Serialize};

use crate::util::unix_ms;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngestPayload {
    /// Self-reported instance identity (hostname, or an operator-assigned
    /// name) — the key this instance's state is stored and overwritten
    /// under. Two instances sharing a name would clobber each other; naming
    /// them uniquely is the operator's responsibility, the same way it is
    /// for e.g. Prometheus scrape target labels.
    pub instance: String,
    /// This instance's own admin API base URL (e.g. `"http://10.0.1.4:9900"`)
    /// — self-reported, from `settings.admin.listen`. The only "backend
    /// registry" slice 9's intent-verb fan-out needs: no separate discovery
    /// mechanism, the aggregator just proxies to whatever URL an instance
    /// told it to use. A `0.0.0.0`/wildcard bind isn't a reachable address
    /// from the aggregator's side — that's a pre-existing property of the
    /// admin API, not something this field introduces; bind it to a
    /// concretely reachable address if the aggregator runs elsewhere.
    pub admin_url: String,
    pub pools: Vec<PoolSummary>,
    #[serde(default)]
    pub sessions: SessionCounts,
    /// Self-reported fleet organization path (`settings.group`), e.g.
    /// `"eu/frankfurt/cluster-a"` — an older instance that predates this
    /// field simply reports `None`, showing up ungrouped. Purely a display
    /// label for the admin GUI's fleet tree.
    #[serde(default)]
    pub group: Option<String>,
    /// Product version (`CARGO_PKG_VERSION`) this instance runs; empty from a
    /// build that predates the field. Display and skew only.
    #[serde(default)]
    pub version: String,
    /// Component wire protocol `major.minor` it speaks (`wayhouse-http`); empty
    /// from a build that predates the field.
    #[serde(default)]
    pub protocol: String,
    /// Requests it has refused for an incompatible protocol since it started;
    /// the store notes when this rises (see [`InstanceState::mismatch_rose_at_ms`]).
    #[serde(default)]
    pub protocol_mismatches: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PoolSummary {
    pub name: String,
    pub balancer: String,
    pub backends: Vec<BackendSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackendSummary {
    pub addr: String,
    pub healthy: bool,
    /// `enabled` | `draining` | `disabled` — `wayhouse_core::pool::AdminState`,
    /// as a string so this crate never needs `wayhouse-core` as a dependency
    /// (the aggregator stays decoupled from the data-plane crates).
    pub state: String,
    pub active: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SessionCounts {
    #[serde(default)]
    pub tcp: usize,
    #[serde(default)]
    pub udp: usize,
}

/// One instance's most recent ingest, plus when the aggregator itself
/// received it — timestamped by the aggregator's own clock, not the
/// proxy's, so this stays meaningful even with unsynchronised clocks across
/// a fleet (it only ever answers "how long since *we* last heard from it").
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceState {
    pub payload: IngestPayload,
    pub received_at_ms: u64,
    /// When `protocol_mismatches` last went up between two pushes (aggregator
    /// clock), for the "red only after a recent increase" skew rule.
    pub mismatch_rose_at_ms: Option<u64>,
}

/// The whole store: latest-write-wins per `instance`. No history, no
/// eviction policy yet (an instance that stops pushing just goes stale in
/// place — slice 8's `/fleet/healthz` is where "stale" gets a meaning and a
/// threshold, not here).
#[derive(Default)]
pub struct IngestStore {
    instances: RwLock<HashMap<String, InstanceState>>,
}

impl IngestStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `payload` as the latest state for its `instance`, overwriting
    /// whatever was there before.
    pub fn ingest(&self, payload: IngestPayload) {
        let now = unix_ms();
        let mut instances = self
            .instances
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let prev = instances.get(&payload.instance);
        let mismatch_rose_at_ms = match prev {
            Some(p) if payload.protocol_mismatches > p.payload.protocol_mismatches => Some(now),
            Some(p) => p.mismatch_rose_at_ms,
            None => None,
        };
        let state = InstanceState {
            payload,
            received_at_ms: now,
            mismatch_rose_at_ms,
        };
        instances.insert(state.payload.instance.clone(), state);
    }

    /// One instance's latest state, if it has ever pushed.
    pub fn get(&self, instance: &str) -> Option<InstanceState> {
        self.instances
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(instance)
            .cloned()
    }

    /// Every instance's latest state, instance name ascending (a stable
    /// order for a dashboard table, not a performance concern at fleet
    /// sizes this serves).
    pub fn snapshot(&self) -> Vec<InstanceState> {
        let map = self
            .instances
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let mut out: Vec<InstanceState> = map.values().cloned().collect();
        out.sort_by(|a, b| a.payload.instance.cmp(&b.payload.instance));
        out
    }
}

#[cfg(test)]
impl IngestStore {
    /// Test-only seam: insert a fully-formed [`InstanceState`], letting a
    /// test control `received_at_ms` directly instead of always stamping
    /// with the real clock (`ingest`) — needed to test [`crate::api`]'s
    /// staleness threshold without an actual multi-second sleep.
    pub(crate) fn insert_state(&self, state: InstanceState) {
        self.instances
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(state.payload.instance.clone(), state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(instance: &str) -> IngestPayload {
        IngestPayload {
            instance: instance.to_string(),
            admin_url: "http://127.0.0.1:0".to_string(),
            pools: vec![],
            sessions: SessionCounts::default(),
            group: None,
            version: String::new(),
            protocol: String::new(),
            protocol_mismatches: 0,
        }
    }

    #[test]
    fn a_payload_without_a_group_field_still_deserializes() {
        let json = r#"{"instance":"a","admin_url":"http://127.0.0.1:0","pools":[]}"#;
        let payload: IngestPayload = serde_json::from_str(json).unwrap();
        assert!(payload.group.is_none());
    }

    #[test]
    fn a_payload_without_version_fields_still_deserializes() {
        let json = r#"{"instance":"a","admin_url":"http://127.0.0.1:0","pools":[]}"#;
        let payload: IngestPayload = serde_json::from_str(json).unwrap();
        assert_eq!(
            (
                payload.version.as_str(),
                payload.protocol.as_str(),
                payload.protocol_mismatches
            ),
            ("", "", 0)
        );
    }

    #[test]
    fn the_store_notes_when_the_mismatch_counter_rises() {
        let store = IngestStore::new();
        let mut p = payload("a");
        store.ingest(p.clone());
        assert_eq!(store.get("a").unwrap().mismatch_rose_at_ms, None);
        p.protocol_mismatches = 2;
        store.ingest(p.clone());
        let rose = store.get("a").unwrap().mismatch_rose_at_ms;
        assert!(rose.is_some(), "a rise is noted");
        store.ingest(p);
        assert_eq!(
            store.get("a").unwrap().mismatch_rose_at_ms,
            rose,
            "a flat counter keeps the earlier timestamp"
        );
    }

    #[test]
    fn an_unknown_instance_is_none() {
        let store = IngestStore::new();
        assert!(store.get("nope").is_none());
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn ingest_then_get_round_trips() {
        let store = IngestStore::new();
        store.ingest(payload("a"));
        let state = store.get("a").unwrap();
        assert_eq!(state.payload.instance, "a");
    }

    #[test]
    fn a_second_ingest_from_the_same_instance_overwrites_the_first() {
        let store = IngestStore::new();
        store.ingest(payload("a"));
        let mut second = payload("a");
        second.sessions = SessionCounts { tcp: 5, udp: 2 };
        store.ingest(second);

        assert_eq!(store.snapshot().len(), 1, "still one instance, not two");
        assert_eq!(store.get("a").unwrap().payload.sessions.tcp, 5);
    }

    #[test]
    fn snapshot_lists_every_instance_sorted_by_name() {
        let store = IngestStore::new();
        store.ingest(payload("b"));
        store.ingest(payload("a"));
        store.ingest(payload("c"));

        let names: Vec<String> = store
            .snapshot()
            .into_iter()
            .map(|s| s.payload.instance)
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }
}
