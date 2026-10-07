//! `wayhouse` sniffer: WireGuard handshake initiations.
//!
//! A WireGuard peer opens a session by sending a *handshake initiation*: a
//! UDP datagram of exactly 148 bytes whose first four bytes are the message
//! type `1` followed by three reserved zero bytes (WireGuard whitepaper §5.4.2).
//! Everything after the header is encrypted, so there is no hostname to
//! extract. A recognised initiation carries `key: Some("wireguard")`, for a
//! `sniffer` route matched with an empty `host:` list.
//!
//! Only initiations are recognised: transport data (type 4) and cookie replies
//! (type 3) belong to an already established flow, and a responder's reply
//! (type 2) is never a client's first packet.

const MSG_HANDSHAKE_INITIATION: [u8; 4] = [1, 0, 0, 0];
const HANDSHAKE_INITIATION_LEN: usize = 148;

/// Recognise a WireGuard handshake initiation in `first`.
pub fn recognise(first: &[u8]) -> Option<wayhouse_sniffer_abi::Hint<'static>> {
    if first.len() != HANDSHAKE_INITIATION_LEN || first[..4] != MSG_HANDSHAKE_INITIATION {
        return None;
    }
    Some(wayhouse_sniffer_abi::Hint {
        key: Some("wireguard"),
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

    fn msg(ty: u8, len: usize) -> Vec<u8> {
        let mut p = vec![0u8; len];
        p[0] = ty;
        p
    }

    #[test]
    fn recognises_a_handshake_initiation() {
        let hint = recognise(&msg(1, 148)).unwrap();
        assert_eq!(hint.key, Some("wireguard"));
        assert!(hint.host.is_none());
        assert!(!hint.reject);
    }

    #[test]
    fn rejects_other_message_types() {
        assert!(recognise(&msg(2, 92)).is_none()); // handshake response
        assert!(recognise(&msg(3, 64)).is_none()); // cookie reply
        assert!(recognise(&msg(4, 148)).is_none()); // transport data
    }

    #[test]
    fn rejects_wrong_length_nonzero_reserved_and_junk() {
        assert!(recognise(&msg(1, 147)).is_none());
        assert!(recognise(&msg(1, 149)).is_none());
        assert!(recognise(&[]).is_none());
        let mut p = msg(1, 148);
        p[2] = 1; // reserved bytes must be zero
        assert!(recognise(&p).is_none());
        assert!(recognise(&[0xffu8; 148]).is_none());
    }
}
