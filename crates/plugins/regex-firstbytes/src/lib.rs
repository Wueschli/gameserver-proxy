//! `wayhouse` sniffer plugin: a bounded, **runtime-configured** first-bytes matcher.
//!
//! As sketched in `docs/08` Phase 9 ("a generic bounded `regex-firstbytes`,
//! keeps `regex` off the core routing path"). Since the data-plane-completion
//! "per-plugin config" work (ADR 16a) the ABI carries a
//! `settings.sniffers.modules[].config` string, so the pattern is supplied at
//! config time rather than compiled in.
//!
//! Deliberately **not** a full regex engine — `regex` (or even
//! `regex-lite` / `regex-automata`) is real weight to carry into every call's
//! fresh WASM instance, and a proxy sniffer only ever needs to answer "does the
//! prefix look like X". This is an `O(n)`, allocation-free byte matcher over a
//! tiny hand-written pattern language.
//!
//! ## Config grammar
//! ```text
//! config   := [ "key:" NAME "|" ] pattern { "|" pattern }
//! pattern  := [ "@" OFFSET " " ] ( "hex:" HEXDIGITS | "ascii:" TEXT )
//! ```
//! - Whitespace around each `|`-separated segment is trimmed.
//! - `key:` (optional, must be the first segment) sets the recognised hint's
//!   `key`; default `"firstbytes"`.
//! - `@OFFSET ` (decimal, ≤ [`MAX_OFFSET`]) shifts where the literal must
//!   appear; default `0`.
//! - A datagram is recognised if **any** pattern's literal bytes are a prefix
//!   of `first[offset..]`.
//! - Up to [`MAX_PATTERNS`] patterns are considered; a malformed segment makes
//!   the whole config match nothing (no panic, no partial behaviour).
//!
//! Examples: `ascii:GET ` — HTTP-ish. `key:a2s|@0 hex:ffffffff` — Source-engine
//! query magic, tagged `a2s`. `hex:1603|ascii:\x16\x03` — a TLS record start.

/// Cap on `@OFFSET` — a pattern beyond this is treated as unmatched rather than
/// walking an arbitrarily large prefix.
pub const MAX_OFFSET: usize = 512;
/// Cap on how many `|`-separated patterns are evaluated.
pub const MAX_PATTERNS: usize = 32;

/// Recognise `first` against `config` (see the module docs for the grammar).
/// Pure and host-independent so it's unit tested directly; `sniff` below is the
/// thin ABI wrapper the host calls.
pub fn recognise<'a>(first: &[u8], config: &'a [u8]) -> Option<wayhouse_sniffer_abi::Hint<'a>> {
    let config = std::str::from_utf8(config).ok()?;
    let mut segments = config.split('|').map(str::trim);

    // Optional leading `key:NAME`.
    let mut key = "firstbytes";
    let first_seg = segments.clone().next()?;
    if let Some(name) = first_seg.strip_prefix("key:") {
        if name.is_empty() {
            return None;
        }
        key = name;
        segments.next(); // consume it
    }

    let mut matched = false;
    for (i, seg) in segments.enumerate() {
        if i >= MAX_PATTERNS || seg.is_empty() {
            break;
        }
        match pattern_matches(first, seg) {
            Some(true) => {
                matched = true;
                break;
            }
            Some(false) => {}
            None => return None, // malformed pattern ⇒ config matches nothing
        }
    }

    matched.then_some(wayhouse_sniffer_abi::Hint {
        key: Some(key),
        ..Default::default()
    })
}

/// `Some(true)` = the pattern's literal is a prefix of `first[offset..]`,
/// `Some(false)` = well-formed but no match, `None` = malformed pattern.
fn pattern_matches(first: &[u8], seg: &str) -> Option<bool> {
    let (offset, spec) = match seg.strip_prefix('@') {
        Some(rest) => {
            let (digits, spec) = rest.split_once(' ')?;
            let offset: usize = digits.parse().ok()?;
            if offset > MAX_OFFSET {
                return Some(false);
            }
            (offset, spec.trim_start())
        }
        None => (0, seg),
    };
    // Offset past the peeked bytes: a clean miss, not a malformed pattern.
    let Some(hay) = first.get(offset..) else {
        return Some(false);
    };

    if let Some(hex) = spec.strip_prefix("hex:") {
        hex_is_prefix(hay, hex)
    } else if let Some(text) = spec.strip_prefix("ascii:") {
        Some(hay.starts_with(text.as_bytes()))
    } else {
        None
    }
}

/// Whether the bytes encoded by the hex string `hex` are a prefix of `hay`,
/// without allocating. `None` if `hex` is not valid (odd length / non-hex).
fn hex_is_prefix(hay: &[u8], hex: &str) -> Option<bool> {
    let hex = hex.as_bytes();
    if hex.len() % 2 != 0 {
        return None;
    }
    let want = hex.len() / 2;
    for i in 0..want {
        let hi = (hex[2 * i] as char).to_digit(16)?;
        let lo = (hex[2 * i + 1] as char).to_digit(16)?;
        let byte = (hi * 16 + lo) as u8;
        match hay.get(i) {
            Some(&b) if b == byte => {}
            Some(_) => return Some(false),
            None => return Some(false), // hay shorter than the pattern
        }
    }
    Some(true)
}

/// # Safety
/// See `wayhouse_sniffer_abi::input`'s safety note — the pointer/length pairs must be
/// exactly what the host passed to this export.
#[no_mangle]
pub unsafe extern "C" fn sniff(in_ptr: u32, in_len: u32, cfg_ptr: u32, cfg_len: u32) -> i64 {
    let first = wayhouse_sniffer_abi::input(in_ptr, in_len);
    let config = wayhouse_sniffer_abi::config(cfg_ptr, cfg_len);
    match recognise(first, config) {
        Some(hint) => wayhouse_sniffer_abi::emit_hint(&hint),
        None => wayhouse_sniffer_abi::NOT_RECOGNISED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_prefix_matches_and_tags_the_default_key() {
        let hint = recognise(b"GET /health HTTP/1.1\r\n", b"ascii:GET ").unwrap();
        assert_eq!(hint.key, Some("firstbytes"));
        assert!(recognise(b"POST /x HTTP/1.1\r\n", b"ascii:GET ").is_none());
    }

    #[test]
    fn hex_prefix_at_an_offset() {
        // Source-engine A2S: 0xFFFFFFFF then a query byte.
        let pkt = b"\xff\xff\xff\xffTSource Engine Query\0";
        assert!(recognise(pkt, b"hex:ffffffff").unwrap().key == Some("firstbytes"));
        assert!(recognise(pkt, b"@4 hex:54").is_some()); // 'T' at offset 4
        assert!(recognise(pkt, b"@4 hex:55").is_none()); // 'U' — no
    }

    #[test]
    fn key_prefix_sets_the_hint_key() {
        let hint = recognise(b"\xff\xff\xff\xffT", b"key:a2s|hex:ffffffff").unwrap();
        assert_eq!(hint.key, Some("a2s"));
    }

    #[test]
    fn any_alternative_matches() {
        let cfg = b"ascii:GET |ascii:POST |hex:1603";
        assert!(recognise(b"POST / HTTP/1.1", cfg).is_some());
        assert!(recognise(b"\x16\x03\x01hello", cfg).is_some());
        assert!(recognise(b"\xff\xff\xff\xff", cfg).is_none());
    }

    #[test]
    fn empty_or_malformed_config_matches_nothing() {
        assert!(recognise(b"anything", b"").is_none());
        assert!(recognise(b"anything", b"not-a-spec").is_none()); // no hex:/ascii: prefix
        assert!(recognise(b"anything", b"hex:xyz").is_none()); // bad hex
        assert!(recognise(b"anything", b"key:").is_none()); // empty key
        assert!(recognise(b"\xff", b"hex:ff").is_some()); // sanity: still works
    }

    #[test]
    fn offset_past_the_cap_or_the_buffer_is_a_clean_miss() {
        assert!(recognise(b"short", b"@9 hex:00").is_none()); // past the buffer
        assert!(recognise(&[0u8; 1000], b"@600 hex:00").is_none()); // past MAX_OFFSET
    }
}
