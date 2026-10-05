//! The operator-intent revision log (phase 12 slice 3, `docs/10`: "Operator
//! intent (backend overlay, admin state, route hints, resolver pins) moves
//! into the controller's revision log, fleet-wide and persisted across
//! restarts; the phase-5 admin verbs become 'controller writes a
//! revision'").
//!
//! Scope for this slice: the **pool-scoped** verbs — backend add/remove,
//! backend admin state, route-hint — because they mean the exact same thing
//! on every instance that runs the named pool, so broadcasting one log to
//! the whole fleet is the right shape (the same reason structural config
//! itself is one shared document). **Whole-instance drain/undrain is
//! deliberately not part of this log** — it targets *one* instance, not "every
//! instance with pool X", and the controller has no instance identity to
//! target with (that's `wayhouse-aggregator`'s job, via `--instance` fan-out);
//! it keeps working exactly as it does today, through the aggregator or
//! direct per-instance admin API. Resolver pins are still open (`docs/08`
//! "Open questions") and not implemented here either.
//!
//! Reuses [`crate::store::Store`] as-is — it's already a generic
//! content-agnostic append-only revision log; an intent op is just a
//! different kind of "bytes" living in its own `sled` database
//! (`<data_dir>/intent`, alongside config's `<data_dir>/config`). [`api`]
//! mirrors `crate::api`'s submit/subscribe shape closely (down to reusing
//! the `slave` write gate), with one difference: `submit` here also does a
//! **minimal structural validation** of the op itself (addr/IP parse, state
//! is a known value) since there's no `wayhouse_config::validate()` equivalent to
//! lean on for a bare intent op — the same checks `wayhouse`'s own admin API
//! already runs before applying, just moved one hop earlier so a malformed
//! op never gets broadcast at all.
//!
//! The wire shape ([`IntentOp`]) is duplicated in `wayhouse`'s
//! `intent_client.rs` rather than shared as a library — same reasoning as
//! `controller_client`'s hand-parsed SSE JSON and `aggregator_client`'s
//! duplicated `IngestPayload`.

pub mod api;
pub mod relay;

use serde::{Deserialize, Serialize};

/// One operator-intent operation. Tagged by `op` in JSON
/// (`{"op":"backend_add","pool":"...","addr":"..."}`) — the exact shape
/// `wayhouse`'s `intent_client` parses back out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IntentOp {
    /// Mirrors `POST /pools/{pool}/backends` on the phase-5 admin API.
    BackendAdd { pool: String, addr: String },
    /// Mirrors `DELETE /pools/{pool}/backends/{addr}`.
    BackendRemove { pool: String, addr: String },
    /// Mirrors `PATCH /pools/{pool}/backends/{addr}`. `state` is
    /// `enabled` | `draining` | `disabled`.
    BackendPatch {
        pool: String,
        addr: String,
        state: String,
    },
    /// Mirrors `POST /route-hint`.
    RouteHint {
        src_ip: String,
        pool: String,
        #[serde(default = "default_ttl_sec")]
        ttl_sec: u64,
    },
}

fn default_ttl_sec() -> u64 {
    30
}

impl IntentOp {
    /// The same structural checks `wayhouse`'s admin API runs before applying —
    /// done here too so a malformed op is rejected at submission (`422`),
    /// never broadcast to a whole fleet only to fail identically on every
    /// instance.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            IntentOp::BackendAdd { pool, addr } | IntentOp::BackendRemove { pool, addr } => {
                require_non_empty(pool, "pool")?;
                require_socket_addr(addr)?;
            }
            IntentOp::BackendPatch { pool, addr, state } => {
                require_non_empty(pool, "pool")?;
                require_socket_addr(addr)?;
                if !matches!(state.as_str(), "enabled" | "draining" | "disabled") {
                    return Err("state must be one of: enabled, draining, disabled".into());
                }
            }
            IntentOp::RouteHint {
                src_ip,
                pool,
                ttl_sec,
            } => {
                require_non_empty(pool, "pool")?;
                if src_ip.parse::<std::net::IpAddr>().is_err() {
                    return Err("src_ip is not a valid IP address".into());
                }
                if *ttl_sec == 0 || *ttl_sec > 3600 {
                    return Err("ttl_sec must be between 1 and 3600".into());
                }
            }
        }
        Ok(())
    }
}

fn require_non_empty(s: &str, field: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    Ok(())
}

fn require_socket_addr(addr: &str) -> Result<(), String> {
    addr.parse::<std::net::SocketAddr>()
        .map(|_| ())
        .map_err(|_| "addr is not a valid ip:port".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_backend_add_round_trips_through_json() {
        let op = IntentOp::BackendAdd {
            pool: "mc".into(),
            addr: "127.0.0.1:25566".into(),
        };
        let json = serde_json::to_string(&op).unwrap();
        assert_eq!(
            json,
            r#"{"op":"backend_add","pool":"mc","addr":"127.0.0.1:25566"}"#
        );
        let back: IntentOp = serde_json::from_str(&json).unwrap();
        assert_eq!(back, op);
        assert!(back.validate().is_ok());
    }

    #[test]
    fn route_hint_defaults_ttl_when_omitted() {
        let op: IntentOp =
            serde_json::from_str(r#"{"op":"route_hint","src_ip":"1.2.3.4","pool":"mc"}"#).unwrap();
        assert_eq!(
            op,
            IntentOp::RouteHint {
                src_ip: "1.2.3.4".into(),
                pool: "mc".into(),
                ttl_sec: 30
            }
        );
    }

    #[test]
    fn an_invalid_addr_fails_validation() {
        let op = IntentOp::BackendAdd {
            pool: "mc".into(),
            addr: "not-an-addr".into(),
        };
        assert!(op.validate().is_err());
    }

    #[test]
    fn an_unknown_backend_state_fails_validation() {
        let op = IntentOp::BackendPatch {
            pool: "mc".into(),
            addr: "127.0.0.1:1".into(),
            state: "sideways".into(),
        };
        assert!(op.validate().is_err());
    }

    #[test]
    fn an_empty_pool_fails_validation() {
        let op = IntentOp::BackendRemove {
            pool: "".into(),
            addr: "127.0.0.1:1".into(),
        };
        assert!(op.validate().is_err());
    }

    #[test]
    fn a_route_hint_ttl_out_of_range_fails_validation() {
        let op = IntentOp::RouteHint {
            src_ip: "1.2.3.4".into(),
            pool: "mc".into(),
            ttl_sec: 9999,
        };
        assert!(op.validate().is_err());
    }
}
