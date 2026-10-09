//! Plugin secrets at rest (design:
//! `docs/superpowers/specs/2026-10-08-plugin-secret-storage-design.md`).
//!
//! One cluster keyring, provisioned identically to every controller out of band
//! (`--plugin-secret-key-file`, or `WAYHOUSE_PLUGIN_SECRET_KEY`), encrypts each secret with
//! XChaCha20-Poly1305 and a random 192-bit nonce. The associated data binds a ciphertext
//! to `install id || 0 || slot || 0 || key id`, so one copied to another slot or install
//! fails to open. The log, snapshots and the database hold only the [`Sealed`] form; the
//! keyring never leaves the process. The file is a keyring (one base64 key per line, the
//! last line active) polled for changes, so a rotation needs no restart.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wayhouse_plugin_host::secret::MIN_SECRET_BYTES;
use wayhouse_plugin_host::SecretValue;
use zeroize::Zeroizing;

/// The environment variable that carries a keyring when no file is given.
pub const KEY_ENV: &str = "WAYHOUSE_PLUGIN_SECRET_KEY";
/// Largest secret value, in bytes.
pub const MAX_SECRET_BYTES: usize = 4096;
/// How often the key file is checked for a change.
const RELOAD_EVERY: Duration = Duration::from_secs(5);

/// A secret as stored and replicated: ciphertext only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sealed {
    pub key_id: String,
    /// Base64 of the 24-byte nonce.
    pub nonce: String,
    /// Base64 of the ciphertext with its tag.
    pub ciphertext: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyringError {
    #[error("the key file is readable by group or others; chmod 600 it")]
    Permissions,
    #[error("could not read the key file: {0}")]
    Read(String),
    #[error("line {0} is not a base64 key of exactly 32 bytes (generate one with `openssl rand -base64 32`)")]
    BadKey(usize),
    #[error("the keyring holds no key")]
    Empty,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SealError {
    #[error("no secret key is configured")]
    NoKey,
    #[error("a secret must be {MIN_SECRET_BYTES} to {MAX_SECRET_BYTES} bytes")]
    Size,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OpenError {
    /// The keyring lacks the key this secret was sealed with.
    #[error("key {0} is missing")]
    KeyMissing(String),
    #[error("the secret does not authenticate (wrong key, or moved to another slot)")]
    Auth,
    #[error("the stored secret is malformed")]
    Malformed,
}

struct Key {
    id: String,
    bytes: Zeroizing<[u8; 32]>,
}

/// The keys this node holds; the last is active.
#[derive(Default)]
pub struct Keyring {
    keys: Vec<Key>,
}

fn key_id(key: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(b"wayhouse-plugin-secret-key-id");
    h.update(key);
    h.finalize()[..8].iter().fold(String::new(), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    })
}

fn aad(install: &str, slot: &str, key_id: &str) -> Vec<u8> {
    let mut a = Vec::new();
    for part in [install, slot, key_id] {
        a.extend_from_slice(part.as_bytes());
        a.push(0);
    }
    a
}

impl std::fmt::Debug for Keyring {
    /// Key ids only, never key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyring")
            .field("keys", &self.key_ids())
            .finish()
    }
}

impl Keyring {
    /// Parses a keyring: one base64 key per line; blank lines and `#` comments are skipped.
    pub fn parse(text: &str) -> Result<Self, KeyringError> {
        let mut keys = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let raw = Zeroizing::new(
                STANDARD
                    .decode(line)
                    .map_err(|_| KeyringError::BadKey(i + 1))?,
            );
            let bytes: [u8; 32] = raw
                .as_slice()
                .try_into()
                .map_err(|_| KeyringError::BadKey(i + 1))?;
            let id = key_id(&bytes);
            if !keys.iter().any(|k: &Key| k.id == id) {
                keys.push(Key {
                    id,
                    bytes: Zeroizing::new(bytes),
                });
            }
        }
        if keys.is_empty() {
            return Err(KeyringError::Empty);
        }
        Ok(Self { keys })
    }

    /// Reads a key file, refusing one other users can read.
    pub fn from_file(path: &Path) -> Result<Self, KeyringError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(path).map_err(|e| KeyringError::Read(e.to_string()))?;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(KeyringError::Permissions);
            }
        }
        let text = Zeroizing::new(
            std::fs::read_to_string(path).map_err(|e| KeyringError::Read(e.to_string()))?,
        );
        Self::parse(&text)
    }

    /// The keyring from `WAYHOUSE_PLUGIN_SECRET_KEY` (one or more keys, whitespace
    /// separated), if set.
    pub fn from_env() -> Result<Option<Self>, KeyringError> {
        match std::env::var(KEY_ENV) {
            Ok(v) if !v.trim().is_empty() => {
                let v = Zeroizing::new(v);
                Self::parse(&v.split_whitespace().collect::<Vec<_>>().join("\n")).map(Some)
            }
            _ => Ok(None),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Ids of every key held, oldest first.
    pub fn key_ids(&self) -> Vec<String> {
        self.keys.iter().map(|k| k.id.clone()).collect()
    }

    pub fn active_id(&self) -> Option<&str> {
        self.keys.last().map(|k| k.id.as_str())
    }

    fn cipher(key: &Key) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&*key.bytes).into())
    }

    /// Encrypts `plaintext` for `(install, slot)` under the active key.
    pub fn seal(&self, install: &str, slot: &str, plaintext: &[u8]) -> Result<Sealed, SealError> {
        if !(MIN_SECRET_BYTES..=MAX_SECRET_BYTES).contains(&plaintext.len()) {
            return Err(SealError::Size);
        }
        let key = self.keys.last().ok_or(SealError::NoKey)?;
        let nonce: [u8; 24] = rand::random();
        let ciphertext = Self::cipher(key)
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: plaintext,
                    aad: &aad(install, slot, &key.id),
                },
            )
            .map_err(|_| SealError::Size)?;
        Ok(Sealed {
            key_id: key.id.clone(),
            nonce: STANDARD.encode(nonce),
            ciphertext: STANDARD.encode(ciphertext),
        })
    }

    /// Decrypts a secret sealed for `(install, slot)`.
    pub fn open(
        &self,
        install: &str,
        slot: &str,
        sealed: &Sealed,
    ) -> Result<SecretValue, OpenError> {
        let key = self
            .keys
            .iter()
            .find(|k| k.id == sealed.key_id)
            .ok_or_else(|| OpenError::KeyMissing(sealed.key_id.clone()))?;
        let nonce: [u8; 24] = STANDARD
            .decode(&sealed.nonce)
            .ok()
            .and_then(|n| n.try_into().ok())
            .ok_or(OpenError::Malformed)?;
        let ciphertext = STANDARD
            .decode(&sealed.ciphertext)
            .map_err(|_| OpenError::Malformed)?;
        let plain = Self::cipher(key)
            .decrypt(
                (&nonce).into(),
                Payload {
                    msg: &ciphertext,
                    aad: &aad(install, slot, &key.id),
                },
            )
            .map_err(|_| OpenError::Auth)?;
        Ok(SecretValue::new(plain))
    }
}

/// The node's keyring, swapped whole when the key file changes.
#[derive(Clone, Default)]
pub struct KeyringHandle(Arc<RwLock<Arc<Keyring>>>);

impl KeyringHandle {
    pub fn new(keyring: Keyring) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(keyring))))
    }

    pub fn current(&self) -> Arc<Keyring> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set(&self, keyring: Keyring) {
        *self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(keyring);
    }
}

/// Loads the keyring from `file` or the environment. `None` when neither is configured.
pub fn load(file: Option<&Path>) -> Result<Option<KeyringHandle>, KeyringError> {
    match file {
        Some(path) => Keyring::from_file(path).map(|k| Some(KeyringHandle::new(k))),
        None => Ok(Keyring::from_env()?.map(KeyringHandle::new)),
    }
}

/// What identifies a version of the key file.
fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Polls `path`; a changed file that parses replaces the keyring, one that does not is
/// logged and ignored (the old keys stay).
pub fn spawn_reload(handle: KeyringHandle, path: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut seen = stamp(&path);
        let mut every = tokio::time::interval(RELOAD_EVERY);
        loop {
            every.tick().await;
            reload_if_changed(&handle, &path, &mut seen);
        }
    })
}

pub fn reload_if_changed(
    handle: &KeyringHandle,
    path: &Path,
    seen: &mut Option<(SystemTime, u64)>,
) {
    let now = stamp(path);
    if now == *seen {
        return;
    }
    *seen = now;
    match Keyring::from_file(path) {
        Ok(k) => {
            tracing::info!(keys = ?k.key_ids(), "reloaded the plugin secret keyring");
            handle.set(k);
        }
        Err(e) => {
            tracing::warn!(error = %e, "the plugin secret key file changed but did not load; keeping the old keys")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> String {
        STANDARD.encode([n; 32])
    }

    fn ring(keys: &[u8]) -> Keyring {
        Keyring::parse(&keys.iter().map(|n| key(*n)).collect::<Vec<_>>().join("\n")).unwrap()
    }

    #[test]
    fn a_sealed_secret_opens_only_where_it_was_sealed() {
        let r = ring(&[1]);
        let sealed = r.seal("inst", "TOKEN", b"hunter2-hunter2").unwrap();
        assert_eq!(
            r.open("inst", "TOKEN", &sealed).unwrap().expose(),
            b"hunter2-hunter2"
        );
        // Moved to another slot or install: the associated data no longer matches.
        assert_eq!(
            r.open("inst", "OTHER", &sealed).unwrap_err(),
            OpenError::Auth
        );
        assert_eq!(
            r.open("other", "TOKEN", &sealed).unwrap_err(),
            OpenError::Auth
        );
        // And the ciphertext is not the plaintext.
        assert!(!sealed.ciphertext.contains("hunter"));
        assert!(!STANDARD
            .decode(&sealed.ciphertext)
            .unwrap()
            .windows(7)
            .any(|w| w == b"hunter2"));
    }

    #[test]
    fn two_seals_of_the_same_secret_differ() {
        let r = ring(&[1]);
        let a = r.seal("i", "T", b"same-secret-value").unwrap();
        let b = r.seal("i", "T", b"same-secret-value").unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn the_last_key_is_active_and_older_ones_still_open() {
        let old = ring(&[1]);
        let sealed_old = old.seal("i", "T", b"old-secret-value").unwrap();
        let both = ring(&[1, 2]);
        assert_eq!(both.active_id().unwrap(), key_id(&[2; 32]));
        assert_eq!(
            both.open("i", "T", &sealed_old).unwrap().expose(),
            b"old-secret-value"
        );
        assert_eq!(
            both.seal("i", "T", b"new-secret-value").unwrap().key_id,
            key_id(&[2; 32])
        );
        // Retiring early: a secret under a removed key is missing its key, not corrupt.
        let only_new = ring(&[2]);
        assert_eq!(
            only_new.open("i", "T", &sealed_old).unwrap_err(),
            OpenError::KeyMissing(key_id(&[1; 32]))
        );
    }

    #[test]
    fn no_key_and_bad_sizes_are_refused() {
        assert_eq!(
            Keyring::default()
                .seal("i", "T", b"long-enough-value")
                .unwrap_err(),
            SealError::NoKey
        );
        let r = ring(&[1]);
        assert_eq!(r.seal("i", "T", b"short").unwrap_err(), SealError::Size);
        assert_eq!(
            r.seal("i", "T", &vec![b'x'; MAX_SECRET_BYTES + 1])
                .unwrap_err(),
            SealError::Size
        );
    }

    #[test]
    fn the_keyring_text_must_be_32_byte_base64_keys() {
        assert_eq!(Keyring::parse("").unwrap_err(), KeyringError::Empty);
        assert_eq!(
            Keyring::parse("# only a comment\n").unwrap_err(),
            KeyringError::Empty
        );
        assert_eq!(
            Keyring::parse("not base64!!").unwrap_err(),
            KeyringError::BadKey(1)
        );
        assert_eq!(
            Keyring::parse(&format!("{}\n{}", key(1), STANDARD.encode([0u8; 16]))).unwrap_err(),
            KeyringError::BadKey(2)
        );
        let r = Keyring::parse(&format!("# old\n{}\n\n{}\n{}\n", key(1), key(2), key(2))).unwrap();
        assert_eq!(r.key_ids().len(), 2, "a repeated key counts once");
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys");
        std::fs::write(&path, key(1)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            Keyring::from_file(&path).unwrap_err(),
            KeyringError::Permissions
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(Keyring::from_file(&path).unwrap().key_ids().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_changed_key_file_is_picked_up_and_a_broken_one_is_ignored() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys");
        let write = |text: &str| {
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        write(&key(1));
        let handle = load(Some(&path)).unwrap().unwrap();
        let mut seen = stamp(&path);
        assert_eq!(handle.current().key_ids().len(), 1);
        write(&format!("{}\n{}", key(1), key(2)));
        reload_if_changed(&handle, &path, &mut seen);
        assert_eq!(handle.current().key_ids().len(), 2);
        write("garbage");
        reload_if_changed(&handle, &path, &mut seen);
        assert_eq!(handle.current().key_ids().len(), 2, "the old keys stay");
    }
}
