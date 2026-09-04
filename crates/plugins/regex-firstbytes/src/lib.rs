//! `gsp` sniffer plugin **template**: a bounded first-bytes pattern matcher,
//! as sketched in `docs/08` Phase 9 ("a generic bounded `regex-firstbytes`,
//! keeps `regex` off the core routing path").
//!
//! **This is a template, not a generic engine.** The proxy's `sniffer` route
//! matcher and `settings.sniffers` config have no per-plugin parameters today
//! (a plugin is addressed only by name), so there is nowhere to hand this
//! module a runtime regex pattern. What's here demonstrates the *shape* the
//! roadmap describes — a bounded, allocation-light matcher over the peeked
//! bytes with no backend I/O — using a fixed, compiled-in pattern: an HTTP/1.x
//! request line (`GET /... HTTP/1.1\r\n` and friends). Adding real
//! per-instance configurability (e.g. `settings.sniffers.modules[].config`
//! passed to the plugin, or one compiled module per pattern) is future work,
//! noted in `HANDOVER.md`.
//!
//! Deliberately **not** a full regex engine — `regex` (or even
//! `regex-lite`/`regex-automata`) is real weight to carry into every call's
//! fresh WASM instance, and a proxy sniffer only ever needs to answer "does
//! the prefix look like X", not general-purpose matching. A hand-rolled,
//! `O(n)` byte matcher does that with zero allocation until a match is found.

/// HTTP/1.x methods this template recognises at the start of the first bytes.
const METHODS: &[&str] = &[
    "GET ", "POST ", "HEAD ", "PUT ", "DELETE ", "OPTIONS ", "PATCH ",
];

/// Recognise an HTTP/1.x request line: `<METHOD> <target> HTTP/1.<0|1>\r\n`,
/// entirely within the peeked prefix. Pure and host-independent so it's unit
/// tested directly; `sniff` below is the thin ABI wrapper the host calls.
pub fn recognise(first: &[u8]) -> Option<gsp_sniffer_abi::Hint<'static>> {
    let method = METHODS.iter().find(|m| first.starts_with(m.as_bytes()))?;
    let rest = &first[method.len()..];

    // Find the request line's terminating "\r\n" within a sane bound — an
    // unterminated line this long is either not HTTP or an attempt to make
    // us scan forever; either way, bail rather than walk the whole buffer.
    const MAX_LINE: usize = 2048;
    let line_end = rest.iter().take(MAX_LINE).position(|&b| b == b'\r')?;
    if rest.get(line_end + 1) != Some(&b'\n') {
        return None;
    }
    let line = &rest[..line_end];

    // `<target> HTTP/1.<0|1>` — a target with no embedded space, then the
    // literal version token.
    let space = line.iter().position(|&b| b == b' ')?;
    let version = &line[space + 1..];
    if !matches!(version, b"HTTP/1.0" | b"HTTP/1.1") {
        return None;
    }

    Some(gsp_sniffer_abi::Hint {
        key: Some("http"),
        ..Default::default()
    })
}

/// # Safety
/// See `gsp_sniffer_abi::input`'s safety note — `ptr`/`len` must be exactly
/// what the host passed to this export.
#[no_mangle]
pub unsafe extern "C" fn sniff(ptr: u32, len: u32) -> i64 {
    let first = gsp_sniffer_abi::input(ptr, len);
    match recognise(first) {
        Some(hint) => gsp_sniffer_abi::emit_hint(&hint),
        None => gsp_sniffer_abi::NOT_RECOGNISED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_common_methods() {
        for line in [
            "GET /health HTTP/1.1\r\n\r\n",
            "POST /api/v1/thing HTTP/1.1\r\nHost: x\r\n",
            "HEAD / HTTP/1.0\r\n\r\n",
        ] {
            let hint = recognise(line.as_bytes()).unwrap();
            assert_eq!(hint.key, Some("http"));
        }
    }

    #[test]
    fn rejects_non_http_and_malformed_lines() {
        assert!(recognise(b"\xff\xff\xff\xffTSource Engine Query\0").is_none());
        assert!(recognise(b"GET /no-terminator-here").is_none());
        assert!(recognise(b"GET /bad-version HTTP/2\r\n").is_none());
        assert!(recognise(b"").is_none());
    }
}
