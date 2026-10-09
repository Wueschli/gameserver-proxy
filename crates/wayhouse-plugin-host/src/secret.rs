//! Secret values as the host handles them (design:
//! `docs/superpowers/specs/2026-10-08-plugin-secret-storage-design.md`).
//!
//! A [`SecretValue`] lives only for the host call that expands it: it is zeroed on drop, never
//! `Debug`-printed, never passed to the guest. The [`Scrubber`] removes every form a
//! destination could echo back (raw, base64, percent-encoded) from what the guest sees.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use zeroize::Zeroizing;

/// The shortest secret the store accepts: scrubbing a shorter value would mangle ordinary
/// text and protect nothing.
pub const MIN_SECRET_BYTES: usize = 8;

/// What replaces a secret in anything the guest or a log can see.
pub const REDACTED: &str = "[redacted]";

/// A secret's plaintext, zeroed when dropped.
pub struct SecretValue(Zeroizing<Vec<u8>>);

impl SecretValue {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The plaintext. Call only where it is written into a request.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl std::fmt::Display for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where the host reads a plugin's secrets from: `Ok(None)` when the slot is not set,
/// `Err` with a reason when it is set but cannot be read (the key is missing).
pub trait SecretSource: Send + Sync {
    fn get(&self, slot: &str) -> Result<Option<SecretValue>, String>;
}

/// A source with no secrets at all.
pub struct NoSecrets;

impl SecretSource for NoSecrets {
    fn get(&self, _slot: &str) -> Result<Option<SecretValue>, String> {
        Ok(None)
    }
}

/// Replaces every form of the secrets it was built from.
#[derive(Default)]
pub struct Scrubber {
    patterns: Vec<Zeroizing<Vec<u8>>>,
}

impl Scrubber {
    /// Adds `secret` and its base64 (standard and URL-safe, padded or not) and
    /// percent-encoded forms. A secret under [`MIN_SECRET_BYTES`] is skipped.
    pub fn add(&mut self, secret: &SecretValue) {
        let raw = secret.expose();
        if raw.len() < MIN_SECRET_BYTES {
            return;
        }
        let mut forms: Vec<Vec<u8>> = vec![raw.to_vec()];
        for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
            forms.push(engine.encode(raw).into_bytes());
        }
        forms.push(
            percent_encoding::percent_encode(raw, percent_encoding::NON_ALPHANUMERIC)
                .to_string()
                .into_bytes(),
        );
        for form in forms {
            if !self.patterns.iter().any(|p| p.as_slice() == form) {
                self.patterns.push(Zeroizing::new(form));
            }
        }
        // Longest first, so a form that contains another is replaced whole.
        self.patterns.sort_by_key(|p| std::cmp::Reverse(p.len()));
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn scrub_bytes(&self, data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        for pat in &self.patterns {
            out = replace_all(&out, pat, REDACTED.as_bytes());
        }
        out
    }

    pub fn scrub_str(&self, text: &str) -> String {
        String::from_utf8_lossy(&self.scrub_bytes(text.as_bytes())).into_owned()
    }
}

fn replace_all(hay: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    if needle.is_empty() || hay.len() < needle.len() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(s: &str) -> SecretValue {
        SecretValue::new(s.as_bytes().to_vec())
    }

    #[test]
    fn a_secret_never_prints() {
        assert_eq!(format!("{:?}", secret("hunter2hunter2")), "<redacted>");
        assert_eq!(format!("{}", secret("hunter2hunter2")), "<redacted>");
    }

    #[test]
    fn every_echoed_form_is_scrubbed() {
        let mut s = Scrubber::default();
        let raw = "tok/en+value=1234";
        s.add(&secret(raw));
        let b64 = STANDARD.encode(raw);
        let b64url = URL_SAFE_NO_PAD.encode(raw);
        let pct =
            percent_encoding::percent_encode(raw.as_bytes(), percent_encoding::NON_ALPHANUMERIC)
                .to_string();
        for form in [raw, &b64, &b64url, &pct] {
            let out = s.scrub_str(&format!("echo: {form}; again {form}"));
            assert_eq!(out, "echo: [redacted]; again [redacted]", "{form}");
        }
        assert_eq!(s.scrub_str("nothing to see"), "nothing to see");
    }

    #[test]
    fn a_short_secret_is_not_scrubbed() {
        let mut s = Scrubber::default();
        s.add(&secret("abc"));
        assert!(s.is_empty());
        assert_eq!(s.scrub_str("abc abc"), "abc abc");
    }

    #[test]
    fn scrubbing_binary_bodies_keeps_other_bytes() {
        let mut s = Scrubber::default();
        s.add(&secret("sekret-value-9"));
        let mut body = vec![0u8, 255, 1];
        body.extend_from_slice(b"sekret-value-9");
        body.push(7);
        let mut want = vec![0u8, 255, 1];
        want.extend_from_slice(REDACTED.as_bytes());
        want.push(7);
        assert_eq!(s.scrub_bytes(&body), want);
    }
}
