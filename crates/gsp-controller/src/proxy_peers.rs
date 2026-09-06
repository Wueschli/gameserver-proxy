//! Phase 14 slice 7 (`docs/08` "Phase 14", `docs/11-backend-transport.md`):
//! the proxy-peers registry — the mirror image of [`crate::peers`]. Where
//! that registry lets every edge `gsp` proxy learn about every origin's
//! `gsp-agent`, this one lets every origin's `gsp-agent` learn about every
//! edge `gsp` proxy, so a growing fleet of proxies (or one added after an
//! origin was already deployed) doesn't need any origin-side reconfiguration
//! — `gsp-agent` just keeps subscribing, the same as `gsp` already does for
//! origins.
//!
//! **Why this wasn't needed from slice 2**: `docs/11`'s locked topology
//! ("one shared interface with every proxy PoP it's paired with as a peer")
//! always implied this, but slice 6's end-to-end verification shipped
//! `gsp-agent --peer-pubkey`/`--peer-endpoint` as a static single-proxy pin
//! instead — enough to prove the tunnel data plane end to end, but it
//! doesn't scale past one proxy and doesn't handle a proxy added later.
//! This registry (and `gsp-agent`'s new subscribe-and-reconcile task) closes
//! that gap the same way slice 2-5 closed it for the other direction.
//!
//! Shape: identical to [`crate::peers`] — per-name latest-write-wins state
//! on top of [`crate::store::Store`]'s append-only log + a sibling `current`
//! tree — deliberately its own module and its own `sled` database rather
//! than a generalized "registry" abstraction shared with `peers`, matching
//! how `config`/`intent`/`peers` are each already their own module wrapping
//! the same `Store` primitive independently.
//!
//! One real difference from [`crate::peers::PeerRegistration`]: `endpoint`
//! is required here, not optional. `docs/11`'s whole premise is that only
//! the proxy side ever needs a stable public address — an origin behind a
//! home NAT is the expected case, a proxy with no reachable endpoint is a
//! misconfiguration, not a supported topology.

pub mod api;

use gsp_config::base64_decode_32;
use serde::{Deserialize, Serialize};

/// One proxy instance's current registration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProxyRegistration {
    /// This proxy's stable identity — arbitrary, chosen by whoever runs it
    /// (`gsp --tunnel-name`), just needs to be unique fleet-wide.
    pub name: String,
    /// This proxy's WireGuard public key, base64-encoded (32 bytes).
    pub pubkey: String,
    /// This proxy's public dial-out address — required (see module doc).
    pub endpoint: String,
}

impl ProxyRegistration {
    /// Same posture as `crate::peers::PeerRegistration::validate` — reject a
    /// malformed submission before it's ever broadcast.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        if base64_decode_32(&self.pubkey).is_none() {
            return Err("pubkey must be a base64-encoded 32-byte WireGuard key".into());
        }
        if self.endpoint.parse::<std::net::SocketAddr>().is_err() {
            return Err(format!(
                "endpoint {:?} is not a valid ip:port",
                self.endpoint
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> ProxyRegistration {
        ProxyRegistration {
            name: "edge-eu-1".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: "203.0.113.9:51820".into(),
        }
    }

    #[test]
    fn a_well_formed_registration_round_trips_through_json_and_validates() {
        let reg = valid();
        let json = serde_json::to_string(&reg).unwrap();
        let back: ProxyRegistration = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reg);
        assert!(back.validate().is_ok());
    }

    #[test]
    fn an_empty_name_fails_validation() {
        let mut reg = valid();
        reg.name = "  ".into();
        assert!(reg.validate().is_err());
    }

    #[test]
    fn a_malformed_pubkey_fails_validation() {
        let mut reg = valid();
        reg.pubkey = "not-a-key".into();
        assert!(reg.validate().is_err());
    }

    #[test]
    fn a_missing_endpoint_fails_validation() {
        let mut reg = valid();
        reg.endpoint = "".into();
        assert!(reg.validate().is_err());
    }

    #[test]
    fn a_malformed_endpoint_fails_validation() {
        let mut reg = valid();
        reg.endpoint = "not-an-addr".into();
        assert!(reg.validate().is_err());
    }
}
