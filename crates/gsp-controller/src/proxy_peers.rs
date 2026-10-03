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
//! tree, in its own `sled` database. Both registries run on the shared core
//! in [`crate::registry`]; this module supplies only the registration type
//! (a [`Registration`] with no backends) and `api` the route prefix.
//!
//! One real difference from [`crate::peers::PeerRegistration`]: `endpoint`
//! is required here, not optional. `docs/11`'s whole premise is that only
//! the proxy side ever needs a stable public address — an origin behind a
//! home NAT is the expected case, a proxy with no reachable endpoint is a
//! misconfiguration, not a supported topology.

pub mod api;

use std::net::IpAddr;

use gsp_config::base64_decode_32;
use serde::{Deserialize, Serialize};

use crate::addresses::Role;
use crate::registry::Registration;

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
    /// This proxy's tunnel-internal IP address (IPv4 or IPv6). Optional on request (omit to
    /// be allocated one, or give one to claim it); always set once stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_address: Option<String>,
    /// A fresh random id per proxy process start. Stored and re-broadcast
    /// as-is: a changed value tells every `gsp-agent` the proxy restarted,
    /// so it re-sets the peer and drops a WireGuard session the restarted
    /// proxy no longer has (see `gsp-agent`'s `proxy_subscribe`). Optional so
    /// proxies and log entries from before it existed still work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
}

/// Upper bound on a `boot_id`'s length — an opaque token, never a payload.
const BOOT_ID_MAX: usize = 64;

impl ProxyRegistration {
    /// The address this registration asks for, if it names one.
    pub fn requested_address(&self) -> Option<IpAddr> {
        self.tunnel_address.as_deref().and_then(|a| a.parse().ok())
    }

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
        if let Some(a) = &self.tunnel_address {
            if a.parse::<IpAddr>().is_err() {
                return Err(format!("tunnel_address {a:?} is not an IP address"));
            }
        }
        if let Some(b) = &self.boot_id {
            let well_formed = !b.is_empty()
                && b.len() <= BOOT_ID_MAX
                && b.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
            if !well_formed {
                return Err(format!(
                    "boot_id must be 1-{BOOT_ID_MAX} ASCII letters, digits or '-'"
                ));
            }
        }
        Ok(())
    }
}

impl Registration for ProxyRegistration {
    const ROLE: Role = Role::Proxy;

    fn name(&self) -> &str {
        &self.name
    }

    fn requested_address(&self) -> Option<IpAddr> {
        ProxyRegistration::requested_address(self)
    }

    fn backends_mut(&mut self) -> Option<&mut Vec<String>> {
        None
    }

    fn set_tunnel_address(&mut self, a: IpAddr) {
        self.tunnel_address = Some(a.to_string());
    }

    fn validate(&self) -> Result<(), String> {
        ProxyRegistration::validate(self)
    }

    fn endpoint_mut(&mut self) -> Option<&mut String> {
        Some(&mut self.endpoint)
    }

    fn register_request(self, now: u64) -> crate::ha::WriteRequest {
        crate::ha::WriteRequest::RegisterProxy { reg: self, now }
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
            tunnel_address: None,
            boot_id: None,
        }
    }

    #[test]
    fn a_boot_id_round_trips_and_is_omitted_when_absent() {
        let json = serde_json::to_string(&valid()).unwrap();
        assert!(!json.contains("boot_id"), "{json}");
        let mut reg = valid();
        reg.boot_id = Some("0123456789abcdef0123456789abcdef".into());
        assert!(reg.validate().is_ok());
        let back: ProxyRegistration =
            serde_json::from_str(&serde_json::to_string(&reg).unwrap()).unwrap();
        assert_eq!(back, reg);
    }

    #[test]
    fn a_malformed_boot_id_fails_validation() {
        let mut reg = valid();
        for bad in ["", "has space", &"a".repeat(65)] {
            reg.boot_id = Some(bad.to_string());
            assert!(reg.validate().is_err(), "{bad:?} must be refused");
        }
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
        let old = r#"{"name":"edge-1","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","endpoint":"203.0.113.9:51820"}"#;
        let reg: ProxyRegistration = serde_json::from_str(old).unwrap();
        assert_eq!(reg.tunnel_address, None);
        assert_eq!(reg.boot_id, None);
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
    fn an_ipv6_endpoint_is_valid() {
        let mut reg = valid();
        reg.endpoint = "[2001:db8::7]:51820".into();
        assert!(reg.validate().is_ok());
        reg.endpoint = "2001:db8::7:51820".into();
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
