//! WireGuard key decoding.

/// Decode a standard-alphabet base64 string that must be exactly 32 bytes —
/// the shape of a WireGuard key (44 chars with one trailing `=`). `pub` (not
/// just used by this crate's own `validate()`) so `gsp-controller`'s phase 14
/// backend-peers registry can apply the exact same pubkey check without
/// duplicating it — `gsp-controller` already depends on `gsp-config` for
/// `parse_str`.
pub fn base64_decode_32(s: &str) -> Option<[u8; 32]> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.decode(s).ok()?.try_into().ok()
}
