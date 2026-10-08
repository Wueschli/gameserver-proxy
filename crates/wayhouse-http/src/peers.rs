//! What protocol minor each peer speaks, so a sender can gate new optional
//! fields on the receiver (#185, the "gating rule" in
//! `docs/superpowers/specs/2026-10-05-component-versioning-design.md`).
//!
//! Both directions:
//! - **Server side.** [`gate`](crate::protocol::gate) records the caller's version
//!   as a [`PeerProtocol`] request extension; a handler or SSE stream asks
//!   [`receiver_supports`] once, on connect.
//! - **Client side.** Every response echoes the server's version. [`PeerVersions`]
//!   remembers the last one per peer ([`PeerVersions::observe`]); until a peer has
//!   answered, [`PeerVersions::peer_supports`] is `false` (send the baseline).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::protocol::{PeerProtocol, ProtocolVersion, HEADER};

/// Does the caller described by `ext` understand protocol minor `minor` (of
/// our major)? A caller that sent no header is treated as baseline.
pub fn receiver_supports(ext: &PeerProtocol, minor: u16) -> bool {
    ext.0.is_some_and(|v| supports(v, minor))
}

fn supports(v: ProtocolVersion, minor: u16) -> bool {
    v.major == ProtocolVersion::current().major && v.minor >= minor
}

/// Last protocol version each peer (keyed by whatever string the caller uses,
/// typically its base URL) answered with. Cheap to clone; clones share state.
#[derive(Debug, Clone, Default)]
pub struct PeerVersions {
    inner: Arc<Mutex<HashMap<String, ProtocolVersion>>>,
}

impl PeerVersions {
    pub fn new() -> Self {
        Self::default()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, ProtocolVersion>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn record(&self, peer: &str, v: ProtocolVersion) {
        self.map().insert(peer.to_owned(), v);
    }

    pub fn get(&self, peer: &str) -> Option<ProtocolVersion> {
        self.map().get(peer).copied()
    }

    /// `true` only once `peer` has answered with the same major and at least
    /// `minor`. An unknown peer is baseline until its first response.
    pub fn peer_supports(&self, peer: &str, minor: u16) -> bool {
        self.get(peer).is_some_and(|v| supports(v, minor))
    }

    pub fn forget(&self, peer: &str) {
        self.map().remove(peer);
    }

    /// Record the version a response carries; a missing or unparsable header
    /// leaves the previous value alone.
    pub fn observe(&self, peer: &str, resp: &reqwest::Response) {
        if let Some(v) = resp
            .headers()
            .get(HEADER)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<ProtocolVersion>().ok())
        {
            self.record(peer, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u16, minor: u16) -> ProtocolVersion {
        ProtocolVersion { major, minor }
    }

    #[test]
    fn unknown_peer_does_not_support_new_minor() {
        let p = PeerVersions::new();
        assert!(!p.peer_supports("a", 1));
        assert!(!p.peer_supports("a", 0), "unknown means baseline, not even minor 0");
    }

    #[test]
    fn records_and_gates_by_minor() {
        let p = PeerVersions::new();
        p.record("a", v(1, 2));
        assert!(p.peer_supports("a", 0) && p.peer_supports("a", 2));
        assert!(!p.peer_supports("a", 3));
        assert!(!p.peer_supports("b", 0), "other peers are unaffected");
        p.forget("a");
        assert_eq!(p.get("a"), None);
    }

    #[test]
    fn older_response_lowers_support() {
        let p = PeerVersions::new();
        p.record("a", v(1, 3));
        assert!(p.peer_supports("a", 3));
        p.record("a", v(1, 1));
        assert!(!p.peer_supports("a", 3), "a downgraded peer gets the baseline again");
    }

    #[test]
    fn other_major_supports_nothing() {
        let p = PeerVersions::new();
        p.record("a", v(2, 9));
        assert!(!p.peer_supports("a", 0));
    }

    #[test]
    fn clones_share_state() {
        let p = PeerVersions::new();
        let q = p.clone();
        p.record("a", v(1, 0));
        assert_eq!(q.get("a"), Some(v(1, 0)));
    }

    #[test]
    fn receiver_supports_reads_the_request_extension() {
        assert!(receiver_supports(&PeerProtocol(Some(v(1, 4))), 4));
        assert!(!receiver_supports(&PeerProtocol(Some(v(1, 4))), 5));
    }

    #[test]
    fn missing_extension_means_baseline() {
        assert!(!receiver_supports(&PeerProtocol(None), 0));
    }

    fn response(header: Option<&str>) -> reqwest::Response {
        let mut b = http::Response::builder();
        if let Some(h) = header {
            b = b.header(HEADER, h);
        }
        reqwest::Response::from(b.body("").unwrap())
    }

    #[test]
    fn observe_reads_the_header() {
        let p = PeerVersions::new();
        p.observe("a", &response(Some("1.5")));
        assert_eq!(p.get("a"), Some(v(1, 5)));
    }

    #[test]
    fn garbage_header_is_ignored() {
        let p = PeerVersions::new();
        p.record("a", v(1, 2));
        p.observe("a", &response(Some("banana")));
        p.observe("a", &response(None));
        assert_eq!(p.get("a"), Some(v(1, 2)));
    }
}
