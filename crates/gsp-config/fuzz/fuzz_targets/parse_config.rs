#![no_main]
//! The whole config parse + validation path (`parse_str`) on arbitrary bytes:
//! matcher / CIDR / byte-spec / country-code parsing, cross-field checks. Must
//! return `Ok`/`Err`, never panic.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = gsp_config::parse_str(text);
    }
});
