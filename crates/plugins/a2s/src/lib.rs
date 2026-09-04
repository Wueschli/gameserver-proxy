//! `gsp` sniffer plugin: Source-engine A2S query packets.
//!
//! A2S ("Any 2 Source", the Steam query protocol used by Source/GoldSrc
//! servers — CS:GO, TF2, Garry's Mod, Rust, …) queries are UDP datagrams
//! whose first four bytes are the "simple" header `0xFFFFFFFF`, followed by
//! one query-type byte. Recognising it needs no state and no config: it's a
//! fixed 5-byte magic check.
//!
//! There is no hostname to route on (A2S has none), so a recognised query
//! carries `key: Some("a2s")` — useful for a `sniffer` route matched with an
//! empty `host:` list (match on *any* recognition) to steer query traffic
//! onto a dedicated pool, separate from real game traffic on the same port.

/// A2S query type bytes (Valve's documented "Simple packet" values).
const A2S_INFO: u8 = b'T';
const A2S_PLAYER: u8 = b'U';
const A2S_RULES: u8 = b'V';
const A2S_GETCHALLENGE: u8 = b'W';

/// Recognise an A2S query in `first`. Pure and host-independent so it's unit
/// tested directly (no wasm runtime needed); `sniff` below is the thin ABI
/// wrapper the host actually calls.
pub fn recognise(first: &[u8]) -> Option<gsp_sniffer_abi::Hint<'static>> {
    let &[0xff, 0xff, 0xff, 0xff, kind, ..] = first else {
        return None;
    };
    if !matches!(kind, A2S_INFO | A2S_PLAYER | A2S_RULES | A2S_GETCHALLENGE) {
        return None;
    }
    Some(gsp_sniffer_abi::Hint {
        key: Some("a2s"),
        ..Default::default()
    })
}

/// A2S recognition is a fixed magic check — this plugin takes no config, so the
/// `cfg_*` params are ignored.
///
/// # Safety
/// See `gsp_sniffer_abi::input`'s safety note — the pointer/length pairs must be
/// exactly what the host passed to this export.
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

    #[test]
    fn recognises_each_query_type() {
        for kind in [A2S_INFO, A2S_PLAYER, A2S_RULES, A2S_GETCHALLENGE] {
            let mut pkt = vec![0xff, 0xff, 0xff, 0xff, kind];
            pkt.extend_from_slice(b"Source Engine Query\0");
            let hint = recognise(&pkt).unwrap();
            assert_eq!(hint.key, Some("a2s"));
            assert!(hint.host.is_none());
            assert!(!hint.reject);
        }
    }

    #[test]
    fn rejects_wrong_magic_short_input_and_unknown_query_type() {
        assert!(recognise(&[0xff, 0xff, 0xff, 0xfe, b'T']).is_none()); // bad magic
        assert!(recognise(&[0xff, 0xff, 0xff, 0xff]).is_none()); // too short
        assert!(recognise(&[0xff, 0xff, 0xff, 0xff, b'Z']).is_none()); // unknown kind
        assert!(recognise(b"not an a2s packet at all").is_none());
    }
}
