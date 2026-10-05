//! `wayhouse` sniffer plugin: RakNet offline-handshake packets.
//!
//! RakNet (Minecraft Bedrock Edition and many other games) opens with
//! "offline" messages that all carry a fixed 16-byte magic,
//! `00 ff ff 00 fe fe fe fe fd fd fd fd 12 34 56 78`:
//!
//! | id | message | magic at |
//! |----|---------|----------|
//! | `0x01` | Unconnected Ping | 9 (after an 8-byte time) |
//! | `0x02` | Unconnected Ping, open connections | 9 |
//! | `0x05` | Open Connection Request 1 | 1 |
//! | `0x07` | Open Connection Request 2 | 1 |
//!
//! A client's first datagram is one of these. The magic is the recognition
//! signal; fields after it are not parsed (a ping's trailing client GUID is
//! not required). A recognised packet carries `key: Some("raknet")`, for a
//! `sniffer` route with an empty `host:` list. Packets of an established
//! connection are not recognised; they ride the session the handshake opened.

const MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];
const ID_UNCONNECTED_PING: u8 = 0x01;
const ID_UNCONNECTED_PING_OPEN_CONNECTIONS: u8 = 0x02;
const ID_OPEN_CONNECTION_REQUEST_1: u8 = 0x05;
const ID_OPEN_CONNECTION_REQUEST_2: u8 = 0x07;

/// Recognise a RakNet offline handshake message in `first`.
pub fn recognise(first: &[u8]) -> Option<wayhouse_sniffer_abi::Hint<'static>> {
    let (&id, rest) = first.split_first()?;
    let magic_at = match id {
        ID_UNCONNECTED_PING | ID_UNCONNECTED_PING_OPEN_CONNECTIONS => 8,
        ID_OPEN_CONNECTION_REQUEST_1 | ID_OPEN_CONNECTION_REQUEST_2 => 0,
        _ => return None,
    };
    if rest.get(magic_at..magic_at + MAGIC.len())? != MAGIC {
        return None;
    }
    Some(wayhouse_sniffer_abi::Hint {
        key: Some("raknet"),
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

    fn ping(id: u8) -> Vec<u8> {
        let mut p = vec![id];
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0x12, 0x34]); // time
        p.extend_from_slice(&MAGIC);
        p.extend_from_slice(&[7; 8]); // client guid
        p
    }

    fn connect(id: u8, tail: usize) -> Vec<u8> {
        let mut p = vec![id];
        p.extend_from_slice(&MAGIC);
        p.extend(std::iter::repeat_n(0, tail));
        p
    }

    #[test]
    fn recognises_unconnected_pings() {
        for id in [0x01, 0x02] {
            let hint = recognise(&ping(id)).unwrap();
            assert_eq!(hint.key, Some("raknet"));
            assert!(hint.host.is_none());
            assert!(!hint.reject);
        }
        assert!(recognise(&ping(0x01)[..25]).is_some()); // GUID not required
    }

    #[test]
    fn recognises_open_connection_requests() {
        assert!(recognise(&connect(0x05, 1 + 1400)).is_some()); // padded to the MTU
        assert!(recognise(&connect(0x05, 0)).is_some());
        assert!(recognise(&connect(0x07, 17)).is_some());
    }

    #[test]
    fn rejects_wrong_ids_and_wrong_magic() {
        assert!(recognise(&connect(0x06, 10)).is_none()); // server reply
        assert!(recognise(&ping(0x1c)).is_none()); // pong
        assert!(recognise(&ping(0x05)).is_none()); // magic at the wrong offset
        assert!(recognise(&connect(0x01, 10)).is_none());
        let mut bad = connect(0x05, 5);
        bad[8] ^= 1;
        assert!(recognise(&bad).is_none());
    }

    #[test]
    fn truncations_and_junk_never_panic() {
        assert!(recognise(&[]).is_none());
        assert!(recognise(&[0x05]).is_none());
        let full = ping(0x01);
        for n in 0..25 {
            assert!(recognise(&full[..n]).is_none(), "len {n}");
        }
        let full = connect(0x05, 4);
        for n in 0..17 {
            assert!(recognise(&full[..n]).is_none(), "len {n}");
        }
        assert!(recognise(&[0xffu8; 100]).is_none());
        assert!(recognise(b"\xff\xff\xff\xffTSource Engine Query\0").is_none());
    }
}
