#![no_main]
//! `wayhouse_config::extract_sni` parses an attacker-controlled TLS ClientHello off
//! the TCP peek buffer. It must never panic and must terminate on any input.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = wayhouse_config::extract_sni(data);
});
