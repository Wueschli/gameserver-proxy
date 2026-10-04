//! Phase 14 slice 2 (`docs/08` "Phase 14", `docs/11-backend-transport.md`):
//! the backend-peers registry. A new resource alongside the config-revision
//! and intent logs — `gsp-agent` (phase 14 slice 3, not built yet) registers
//! its WireGuard pubkey and the origin's fronted backend addresses here;
//! every subscribed edge `gsp` proxy (phase 14 slice 4) reconciles its own
//! WireGuard peer list from it, the same subscribe-and-reconcile shape
//! config revisions already use (`docs/10` principle 5).
//!
//! **Deliberately standalone-testable with nothing but this crate's own HTTP
//! tests** (`docs/08`'s own framing for this slice) — no real WireGuard, no
//! `gsp-agent`, no `gsp` integration yet. Those are slices 3-5.
//!
//! Shape: unlike the config log (one current document) or the intent log
//! (a sequence of one-shot actions), a peer registration is **per-origin
//! latest-write-wins state** — an origin re-registers as its endpoint or
//! backend set changes, and only the newest registration for a given `name`
//! matters for "what does this origin currently look like". [`api::PeersState`]
//! reuses [`crate::store::Store`] as-is for the append-only log a subscriber
//! catches up on (exactly like `crate::intent`'s reuse of `Store`), plus one
//! sibling `sled` tree (`current`, opened via `Store::db` — the same pattern
//! `crate::api::AppState::stage` already established) mapping `name -> latest
//! revision number`, so `GET /peers`/`GET /peers/{name}` don't need a full
//! log scan to answer "what's current".
//!
//! **Scope cut, matching how `intent`/`config` themselves started**: no
//! `slave`-tier relay and no HA integration in this slice — those are
//! per-tier config/intent concerns (phase 12) this registry doesn't
//! automatically inherit; add them later if a multi-tier fleet actually
//! needs origin registrations relayed, same "build the mechanism only when
//! a slice actually needs it" bias this codebase already follows elsewhere.

pub mod api;

use std::net::IpAddr;

use gsp_config::base64_decode_32;
use serde::{Deserialize, Serialize};

use crate::addresses::Role;
use crate::registry::Registration;

/// One origin's current registration — the whole of what `gsp-agent` submits
/// and what a `tunnel` `BackendSource` (phase 14 slice 5) will read back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerRegistration {
    /// The origin's stable identity — what `backend_sources[].pubkey` pins
    /// against and what a pool's `tunnel` source is ultimately keyed by.
    pub name: String,
    /// The origin's WireGuard public key, base64-encoded (32 bytes).
    pub pubkey: String,
    /// The origin's last-known dial-out endpoint, if any — WireGuard's own
    /// roaming/keepalive mean this is informational, never required to
    /// register (an origin behind a home NAT may not have a stable one).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Backend addresses this origin fronts, reachable once its WireGuard peer
    /// is up. Entries are `host:port`, or the shorthand `:port` ("my tunnel
    /// address plus this port"), which the controller expands when it stores
    /// the registration (spec: Backends).
    #[serde(default)]
    pub backends: Vec<String>,
    /// This origin's tunnel-internal IP address (IPv4 or IPv6). Optional on request (omit to
    /// be allocated one, or give one to claim it); always set once stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_address: Option<String>,
}

impl PeerRegistration {
    /// The address this registration asks for, if it names one.
    pub fn requested_address(&self) -> Option<IpAddr> {
        self.tunnel_address.as_deref().and_then(|a| a.parse().ok())
    }

    /// The same "reject a malformed submission before it's ever broadcast"
    /// posture `crate::intent::IntentOp::validate` uses — there's no
    /// `gsp_config::validate()` equivalent for a bare registration either.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        if base64_decode_32(&self.pubkey).is_none() {
            return Err("pubkey must be a base64-encoded 32-byte WireGuard key".into());
        }
        for b in &self.backends {
            let ok = match b.strip_prefix(':') {
                Some(port) => port.parse::<u16>().is_ok(),
                None => b.parse::<std::net::SocketAddr>().is_ok(),
            };
            if !ok {
                return Err(format!("backends entry {b:?} is not host:port or :port"));
            }
        }
        if let Some(a) = &self.tunnel_address {
            if a.parse::<IpAddr>().is_err() {
                return Err(format!("tunnel_address {a:?} is not an IP address"));
            }
        }
        Ok(())
    }
}

impl Registration for PeerRegistration {
    const ROLE: Role = Role::Origin;

    fn name(&self) -> &str {
        &self.name
    }

    fn requested_address(&self) -> Option<IpAddr> {
        PeerRegistration::requested_address(self)
    }

    fn backends_mut(&mut self) -> Option<&mut Vec<String>> {
        Some(&mut self.backends)
    }

    fn set_tunnel_address(&mut self, a: IpAddr) {
        self.tunnel_address = Some(a.to_string());
    }

    fn validate(&self) -> Result<(), String> {
        PeerRegistration::validate(self)
    }

    fn endpoint_mut(&mut self) -> Option<&mut String> {
        self.endpoint.as_mut()
    }

    fn register_request(self, now: u64) -> crate::ha::WriteRequest {
        crate::ha::WriteRequest::RegisterOrigin { reg: self, now }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> PeerRegistration {
        PeerRegistration {
            name: "home-origin".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: Some("203.0.113.7:51820".into()),
            backends: vec!["10.60.0.2:25565".into()],
            tunnel_address: None,
        }
    }

    #[test]
    fn backends_accept_the_port_shorthand() {
        let mut reg = valid();
        reg.backends = vec![":25565".into(), "10.60.0.2:25566".into()];
        assert!(reg.validate().is_ok());
        reg.backends = vec![":notaport".into()];
        assert!(reg.validate().is_err());
    }

    #[test]
    fn a_malformed_tunnel_address_fails_validation() {
        let mut reg = valid();
        reg.tunnel_address = Some("not-an-ip".into());
        assert!(reg.validate().is_err());
        reg.tunnel_address = Some("10.60.0.9".into());
        assert!(reg.validate().is_ok());
        assert_eq!(reg.requested_address(), Some("10.60.0.9".parse().unwrap()));
        reg.tunnel_address = Some("fd49::2".into());
        assert!(reg.validate().is_ok());
        assert_eq!(reg.requested_address(), Some("fd49::2".parse().unwrap()));
        reg.tunnel_address = Some("fd49::zz".into());
        assert!(reg.validate().is_err());
    }

    #[test]
    fn tunnel_address_is_omitted_from_json_when_absent() {
        let json = serde_json::to_string(&valid()).unwrap();
        assert!(!json.contains("tunnel_address"));
    }

    #[test]
    fn a_registration_from_an_older_log_still_decodes() {
        // Written before this field existed.
        let old = r#"{"name":"home","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","endpoint":null,"backends":["10.60.0.2:1"]}"#;
        let reg: PeerRegistration = serde_json::from_str(old).unwrap();
        assert_eq!(reg.tunnel_address, None);
    }

    #[test]
    fn a_well_formed_registration_round_trips_through_json_and_validates() {
        let reg = valid();
        let json = serde_json::to_string(&reg).unwrap();
        let back: PeerRegistration = serde_json::from_str(&json).unwrap();
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
    fn a_malformed_backend_address_fails_validation() {
        let mut reg = valid();
        reg.backends = vec!["not-an-addr".into()];
        assert!(reg.validate().is_err());
    }

    #[test]
    fn endpoint_and_backends_are_optional() {
        let reg: PeerRegistration = serde_json::from_str(
            r#"{"name":"n","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}"#,
        )
        .unwrap();
        assert_eq!(reg.endpoint, None);
        assert!(reg.backends.is_empty());
        assert!(reg.validate().is_ok());
    }
}
