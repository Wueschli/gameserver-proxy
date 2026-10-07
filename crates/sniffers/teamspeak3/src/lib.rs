//! `wayhouse` sniffer: the TeamSpeak 3 voice server's init packet.
//!
//! A TeamSpeak 3 client opens its UDP handshake with an *init* packet whose
//! 8-byte MAC field is the literal `TS3INIT1`:
//!
//! ```text
//! 0..8   "TS3INIT1"
//! 8..10  packet id (u16, big endian)
//! 10..12 client id (u16)
//! 12     packet type / flags, 0x88 for init
//! 13..17 client version
//! 17     init step, 0 for the client's first packet
//! 18..   timestamp, random bytes, reserved zeros (34 bytes in all)
//! ```
//!
//! Only step 0 is recognised, which is what a client sends first; later init
//! steps follow the session it opened. Nothing in it names a host, so a
//! recognised packet carries `key: Some("teamspeak3")`, for a `sniffer`
//! route with an empty `host:` list. The layout follows TeamSpeak's own
//! `ts3init` netfilter module's test client.

const MAGIC: &[u8; 8] = b"TS3INIT1";
const FLAGS_INIT: u8 = 0x88;
const FLAGS_AT: usize = 12;
const STEP_AT: usize = 17;
/// Through the init step byte.
const MIN_LEN: usize = STEP_AT + 1;

/// Recognise a TeamSpeak 3 init step 0 packet in `first`.
pub fn recognise(first: &[u8]) -> Option<wayhouse_sniffer_abi::Hint<'static>> {
    if first.len() < MIN_LEN
        || first[..MAGIC.len()] != *MAGIC
        || first[FLAGS_AT] != FLAGS_INIT
        || first[STEP_AT] != 0
    {
        return None;
    }
    Some(wayhouse_sniffer_abi::Hint {
        key: Some("teamspeak3"),
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

    fn init() -> Vec<u8> {
        let mut p = b"TS3INIT1".to_vec();
        p.extend_from_slice(&[0, 101, 0, 0, 0x88]);
        p.extend_from_slice(&[6, 0x3b, 0xec, 0xe9]); // version
        p.push(0); // step
        p.extend_from_slice(&[0; 16]); // timestamp, random, reserved
        p
    }

    #[test]
    fn recognises_init_step_0() {
        let p = init();
        assert_eq!(p.len(), 34);
        let hint = recognise(&p).unwrap();
        assert_eq!(hint.key, Some("teamspeak3"));
        assert!(hint.host.is_none());
        assert!(!hint.reject);
        assert!(recognise(&p[..18]).is_some());
    }

    #[test]
    fn rejects_other_steps_flags_and_prefixes() {
        let mut p = init();
        p[17] = 2;
        assert!(recognise(&p).is_none());
        let mut p = init();
        p[12] = 0x08;
        assert!(recognise(&p).is_none());
        let mut p = init();
        p[7] = b'2';
        assert!(recognise(&p).is_none());
    }

    #[test]
    fn truncations_and_junk_never_panic() {
        let full = init();
        for n in 0..18 {
            assert!(recognise(&full[..n]).is_none(), "len {n}");
        }
        assert!(recognise(&[]).is_none());
        assert!(recognise(&[0u8; 64]).is_none());
        assert!(recognise(b"\xff\xff\xff\xffTSource Engine Query\0").is_none());
    }
}
