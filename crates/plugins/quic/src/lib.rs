//! `gsp` sniffer plugin: QUIC Initial packets.
//!
//! A client's first datagram on a new QUIC connection is an Initial packet in
//! a *long header*: byte 0 has the header-form and fixed bits set
//! (`0b11xx_xxxx`), the packet type sits in bits 4-5, then a 4-byte version
//! and a length-prefixed destination connection ID (8 to 20 bytes in a client
//! Initial, RFC 9000 §7.2 and §17.2). Only the version-independent
//! prefix is parsed; the payload is encrypted and never inspected, so there is
//! no hostname to extract.
//!
//! A recognised Initial carries `key: Some("quic")`, for a `sniffer` route
//! matched with an empty `host:` list.
//!
//! Recognised versions: v1 (RFC 9000), v2 (RFC 9369) and the IETF drafts
//! `0xff000000..=0xff000022`. Their Initial type bits differ (v1 `0b00`, v2
//! `0b01`). Retry, 0-RTT, Handshake and short-header packets are not first
//! packets from a client, so they are not recognised.

mod initial;

const V1: u32 = 0x0000_0001;
const V2: u32 = 0x6b33_43cf;
const DRAFT_FIRST: u32 = 0xff00_0000;
const DRAFT_LAST: u32 = 0xff00_0022;
const MAX_CID_LEN: u8 = 20;
/// A client must pick a destination connection ID of at least 8 bytes for its
/// first Initial (RFC 9000 §7.2).
const MIN_INITIAL_DCID_LEN: u8 = 8;

/// Recognise a QUIC Initial in `first`. Pure, so it is unit tested natively;
/// `sniff` below is the ABI wrapper the host calls.
pub fn recognise(first: &[u8]) -> Option<gsp_sniffer_abi::Hint<'static>> {
    let (&b0, rest) = first.split_first()?;
    if b0 & 0xc0 != 0xc0 {
        return None; // not a long header with the fixed bit set
    }
    let version = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
    let initial_type = match version {
        V1 | DRAFT_FIRST..=DRAFT_LAST => 0b00,
        V2 => 0b01,
        _ => return None,
    };
    if (b0 >> 4) & 0b11 != initial_type {
        return None;
    }
    let dcid_len = *rest.get(4)?;
    if !(MIN_INITIAL_DCID_LEN..=MAX_CID_LEN).contains(&dcid_len) {
        return None;
    }
    // The DCID, the SCID length byte and (a possibly empty) SCID must follow.
    let after_dcid = 5 + usize::from(dcid_len);
    let scid_len = *rest.get(after_dcid)?;
    if scid_len > MAX_CID_LEN || rest.len() < after_dcid + 1 + usize::from(scid_len) {
        return None;
    }
    // The Initial's keys derive from public inputs, so try to read the SNI.
    // Failure (an older draft, a ClientHello split across packets with the
    // SNI in the later one, a forged packet) is not a reason to drop the
    // recognition: the key-only hint stands.
    let host = initial::extract_sni(first).map(|h| &*Box::leak(h.into_boxed_str()));
    Some(gsp_sniffer_abi::Hint {
        host,
        key: Some("quic"),
        ..Default::default()
    })
}

/// QUIC recognition is a fixed structural check: no config, `cfg_*` ignored.
///
/// # Safety
/// See `gsp_sniffer_abi::input`'s safety note.
#[no_mangle]
pub unsafe extern "C" fn sniff(in_ptr: u32, in_len: u32, _cfg_ptr: u32, _cfg_len: u32) -> i64 {
    let first = gsp_sniffer_abi::input(in_ptr, in_len);
    match recognise(first) {
        Some(hint) => gsp_sniffer_abi::emit_hint(&hint),
        None => gsp_sniffer_abi::NOT_RECOGNISED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A long-header packet: first byte, version, DCID, SCID, then filler.
    fn pkt(b0: u8, version: u32, dcid: &[u8], scid: &[u8]) -> Vec<u8> {
        let mut p = vec![b0];
        p.extend_from_slice(&version.to_be_bytes());
        p.push(dcid.len() as u8);
        p.extend_from_slice(dcid);
        p.push(scid.len() as u8);
        p.extend_from_slice(scid);
        p.extend_from_slice(&[0u8; 32]);
        p
    }

    #[test]
    fn recognises_v1_initial() {
        let hint = recognise(&pkt(0xc3, V1, &[1; 8], &[2; 4])).unwrap();
        assert_eq!(hint.key, Some("quic"));
        assert!(hint.host.is_none());
        assert!(!hint.reject);
    }

    #[test]
    fn a_real_initial_carries_the_sni_as_host() {
        let p: String = include_str!("../testdata/v1_mixed_case.hex").trim().into();
        let bytes: Vec<u8> = (0..p.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&p[i..i + 2], 16).unwrap())
            .collect();
        let hint = recognise(&bytes).unwrap();
        assert_eq!(hint.key, Some("quic"));
        assert_eq!(hint.host, Some("play.example.net"));
    }

    #[test]
    fn recognises_v2_initial_and_drafts() {
        assert!(recognise(&pkt(0xd3, V2, &[1; 8], &[])).is_some()); // v2 Initial = 0b01
        assert!(recognise(&pkt(0xc0, 0xff00_001d, &[1; 8], &[])).is_some()); // draft-29
        assert!(recognise(&pkt(0xc0, V1, &[1; 8], &[])).is_some()); // empty SCID is legal
    }

    #[test]
    fn rejects_wrong_packet_type_for_the_version() {
        assert!(recognise(&pkt(0xd0, V1, &[1; 8], &[])).is_none()); // v1 0-RTT
        assert!(recognise(&pkt(0xc0, V2, &[1; 8], &[])).is_none()); // v2 type 0b00 is Retry
        assert!(recognise(&pkt(0xe0, V1, &[1; 8], &[])).is_none()); // v1 Handshake
    }

    #[test]
    fn rejects_unknown_versions_short_headers_and_bad_lengths() {
        assert!(recognise(&pkt(0xc0, 0, &[1; 8], &[])).is_none()); // version negotiation
        assert!(recognise(&pkt(0xc0, 0x1a2a_3a4a, &[1; 8], &[])).is_none()); // greased
        assert!(recognise(&pkt(0x40, V1, &[1; 8], &[])).is_none()); // short header
        assert!(recognise(&pkt(0x80, V1, &[1; 8], &[])).is_none()); // fixed bit clear
        assert!(recognise(&pkt(0xc0, V1, &[1; 21], &[])).is_none()); // DCID too long
        assert!(recognise(&pkt(0xc0, V1, &[1; 7], &[])).is_none()); // DCID too short
        assert!(recognise(&pkt(0xc0, V1, &[], &[])).is_none());
        assert!(recognise(&pkt(0xc0, V1, &[1; 8], &[2; 21])).is_none()); // SCID too long
    }

    #[test]
    fn rejects_truncated_input() {
        let full = pkt(0xc0, V1, &[1; 8], &[2; 4]);
        for cut in 0..(1 + 4 + 1 + 8 + 1 + 4) {
            assert!(recognise(&full[..cut]).is_none(), "cut at {cut}");
        }
        assert!(recognise(&full[..1 + 4 + 1 + 8 + 1 + 4]).is_some());
        assert!(recognise(b"GET / HTTP/1.1\r\n").is_none());
    }
}
