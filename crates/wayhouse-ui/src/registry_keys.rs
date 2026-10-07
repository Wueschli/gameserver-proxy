//! The public key the official sniffer registry signs with.
//!
//! `None` until the maintainer provides it (a manual step: generate the key pair
//! offline, paste the public half here). While it is `None`, signatures are not
//! checked and installs proceed unsigned, which the first cut allows; the API and
//! the page say "unsigned".

/// Base64 minisign public key (the second line of a `.pub` file).
pub const OFFICIAL_PUBKEY: Option<&str> = None;

/// The parsed official key, if one is configured and valid.
pub fn official_key() -> Option<minisign_verify::PublicKey> {
    OFFICIAL_PUBKEY.and_then(|k| minisign_verify::PublicKey::from_base64(k).ok())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_configured_key_must_parse() {
        if let Some(k) = super::OFFICIAL_PUBKEY {
            assert!(minisign_verify::PublicKey::from_base64(k).is_ok());
        }
    }
}
