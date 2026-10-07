//! `wayhouse` sniffer: OpenVPN client hard-reset packets.
//!
//! An OpenVPN session starts with a *hard reset* from the client: byte 0 is
//! `opcode << 3 | key_id`, with opcode 1 (`P_CONTROL_HARD_RESET_CLIENT_V1`),
//! 7 (`..._V2`, every release since 2.0) or 10 (`..._V3`, `tls-crypt-v2`) and
//! key id 0 for a fresh session. It is followed by an 8-byte session ID, then
//! (depending on `tls-auth` / `tls-crypt`) an HMAC and packet-id fields. The
//! shortest possible reset, without either, is 14 bytes: opcode, session id,
//! an empty ack array length byte and the 4-byte message packet id.
//!
//! Over UDP the datagram is the packet. Over TCP each packet is prefixed with a
//! big-endian `u16` length. The two forms cannot be confused: a TCP length
//! under 2048 starts with a byte of at most `0x07`, never one of the UDP first
//! bytes `0x08`, `0x38` or `0x50`.
//!
//! Nothing past the header is decrypted or checked (there is no hostname), so
//! this is a weak signal of a single byte plus a length window. A recognised
//! reset carries `key: Some("openvpn")`, for a `sniffer` route with an empty
//! `host:` list; list it after the stronger sniffers in a listener's sniffer
//! list. Later packets of a flow are not recognised.

const OP_HARD_RESET_CLIENT_V1: u8 = 1;
const OP_HARD_RESET_CLIENT_V2: u8 = 7;
const OP_HARD_RESET_CLIENT_V3: u8 = 10;
/// Opcode, session id, ack array length, message packet id.
const MIN_PACKET_LEN: usize = 14;
/// A control packet is bounded by the link MTU; anything larger is not a reset.
const MAX_PACKET_LEN: usize = 2047;

fn is_hard_reset_opcode(b0: u8) -> bool {
    matches!(
        b0 >> 3,
        OP_HARD_RESET_CLIENT_V1 | OP_HARD_RESET_CLIENT_V2 | OP_HARD_RESET_CLIENT_V3
    ) && b0 & 0x07 == 0 // key id 0: a fresh session, not a renegotiation
}

/// Recognise an OpenVPN client hard reset in `first`, UDP or TCP framed.
pub fn recognise(first: &[u8]) -> Option<wayhouse_sniffer_abi::Hint<'static>> {
    let udp = first.first().is_some_and(|&b0| is_hard_reset_opcode(b0))
        && (MIN_PACKET_LEN..=MAX_PACKET_LEN).contains(&first.len());
    // TCP: u16 length, then the packet. The peek may hold only a prefix of
    // it (only its first byte is checked), but never more than the declared
    // packet: nothing is sent before the server answers the reset.
    let tcp = match first {
        [hi, lo, opcode, ..] => {
            let declared = usize::from(u16::from_be_bytes([*hi, *lo]));
            (MIN_PACKET_LEN..=MAX_PACKET_LEN).contains(&declared)
                && first.len() - 2 <= declared
                && is_hard_reset_opcode(*opcode)
        }
        _ => false,
    };
    (udp || tcp).then_some(wayhouse_sniffer_abi::Hint {
        key: Some("openvpn"),
        ..Default::default()
    })
}

/// A fixed structural check: no config, `cfg_*` ignored.
///
/// # Safety
/// See `wayhouse_sniffer_abi::input`'s safety note.
#[no_mangle]
pub unsafe extern "C" fn sniff(in_ptr: u32, in_len: u32, _cfg_ptr: u32, _cfg_len: u32) -> i64 {
    let first = wayhouse_sniffer_abi::input(in_ptr, in_len);
    match recognise(first) {
        Some(hint) => wayhouse_sniffer_abi::emit_hint(&hint),
        None => wayhouse_sniffer_abi::NOT_RECOGNISED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset(b0: u8, len: usize) -> Vec<u8> {
        let mut p = vec![0x5au8; len];
        p[0] = b0;
        p
    }

    fn tcp(packet: &[u8]) -> Vec<u8> {
        let mut v = (packet.len() as u16).to_be_bytes().to_vec();
        v.extend_from_slice(packet);
        v
    }

    #[test]
    fn recognises_udp_hard_resets() {
        for b0 in [0x38u8, 0x50, 0x08] {
            let hint = recognise(&reset(b0, 14)).unwrap();
            assert_eq!(hint.key, Some("openvpn"));
            assert!(hint.host.is_none());
            assert!(!hint.reject);
            assert!(recognise(&reset(b0, 100)).is_some());
        }
    }

    #[test]
    fn recognises_tcp_framed_resets() {
        assert!(recognise(&tcp(&reset(0x38, 14))).is_some());
        assert!(recognise(&tcp(&reset(0x50, 300))).is_some());
        // Only part of the packet peeked.
        let full = tcp(&reset(0x38, 300));
        assert!(recognise(&full[..3]).is_some());
        assert!(recognise(&full[..50]).is_some());
    }

    #[test]
    fn rejects_other_opcodes_and_key_ids() {
        assert!(recognise(&reset(0x48, 20)).is_none()); // P_DATA_V2 (9<<3)
        assert!(recognise(&reset(0x20, 20)).is_none()); // P_CONTROL_V1
        assert!(recognise(&reset(0x39, 20)).is_none()); // key id 1: a renegotiation
        assert!(recognise(&reset(0x40, 20)).is_none()); // server reset V2
    }

    #[test]
    fn rejects_bad_lengths() {
        assert!(recognise(&reset(0x38, 13)).is_none());
        assert!(recognise(&reset(0x38, 2048)).is_none());
        let mut over = tcp(&reset(0x38, 14));
        over.push(0); // more bytes than the declared packet
        assert!(recognise(&over).is_none());
        let mut short_decl = vec![0, 13];
        short_decl.extend_from_slice(&reset(0x38, 13));
        assert!(recognise(&short_decl).is_none());
        assert!(recognise(&[0, 0x0e, 0x38]).is_some()); // 14 declared, 1 peeked
    }

    #[test]
    fn truncations_and_junk_never_panic() {
        assert!(recognise(&[]).is_none());
        assert!(recognise(&[0x38]).is_none());
        assert!(recognise(&[0, 14]).is_none()); // length but no packet byte
        let full = tcp(&reset(0x38, 40));
        for n in 0..=full.len() {
            let _ = recognise(&full[..n]);
        }
        assert!(recognise(&[0xffu8; 200]).is_none());
        assert!(recognise(b"GET / HTTP/1.1\r\n\r\n").is_none());
    }
}
