//! `--users-file` (phase 12 slice 8) — multi-operator accounts, replacing
//! the single shared `--ui-password` for deployments with more than one
//! human. YAML, e.g.:
//!
//! ```yaml
//! users:
//!   - username: alice
//!     password_hash: "$argon2id$v=19$..."
//!     role: admin
//!   - username: bob
//!     password_hash: "$argon2id$v=19$..."
//!     role: operator
//! ```
//!
//! `--ui-password` is **kept**, not replaced outright, as a legacy
//! single-shared-secret mode (implicitly `admin`) — see `crate::api`'s doc
//! for why this is the one place this codebase's usual "clean break over
//! compat shim" stance doesn't apply. `wayhouse-ui --hash-password` is the
//! intended way an operator populates a `password_hash` — plaintext
//! passwords never belong in a config file at rest.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use serde::Deserialize;

use crate::role::Role;

#[derive(Debug, Clone, Deserialize)]
pub struct UserRecord {
    pub username: String,
    pub password_hash: String,
    pub role: Role,
}

#[derive(Deserialize)]
struct UsersFile {
    users: Vec<UserRecord>,
}

/// Loads `--users-file`, keyed by username for `O(1)` login lookups.
pub fn load(path: &Path) -> anyhow::Result<HashMap<String, UserRecord>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading users file {}: {e}", path.display()))?;
    let parsed: UsersFile = serde_norway::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parsing users file {}: {e}", path.display()))?;
    if parsed.users.is_empty() {
        anyhow::bail!("users file {} declares no users", path.display());
    }
    let mut by_username = HashMap::with_capacity(parsed.users.len());
    for user in parsed.users {
        if by_username.insert(user.username.clone(), user).is_some() {
            anyhow::bail!("users file {} has a duplicate username", path.display());
        }
    }
    Ok(by_username)
}

/// `true` iff `password` matches `hash` (a PHC-format argon2 hash string, as
/// produced by [`hash_password`]). A malformed `hash` is treated as "never
/// matches," not a panic — a users file is operator-authored, and a typo'd
/// hash should lock that one account out, not crash the process.
pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Runs the same Argon2 verification [`verify_password`] does, against a
/// fixed dummy hash, and discards the result. Login calls this for an unknown
/// username so it costs as much as a known one: without it, an unknown user
/// is rejected instantly and a known one only after Argon2, which lets anyone
/// enumerate usernames from response timing.
pub fn verify_against_dummy(password: &str) {
    static DUMMY: LazyLock<String> = LazyLock::new(|| hash_password("wayhouse-ui-dummy-password"));
    let _ = verify_password(&DUMMY, password);
}

/// Hashes `password` for a `password_hash` entry — what `wayhouse-ui
/// --hash-password` prints. `hash_password` generates its own large random
/// salt internally (no separate `SaltString` to plumb through).
pub fn hash_password(password: &str) -> String {
    let hash: PasswordHash = Argon2::default()
        .hash_password(password.as_bytes())
        .expect("argon2 hashing does not fail for a well-formed password");
    hash.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hashed_password_verifies_against_itself_and_rejects_a_wrong_one() {
        let hash = hash_password("correct horse battery staple");
        assert!(verify_password(&hash, "correct horse battery staple"));
        assert!(!verify_password(&hash, "wrong"));
    }

    #[test]
    fn the_dummy_verification_runs_a_real_argon2_check() {
        // Must not panic, and must parse as a real hash so Argon2 actually runs.
        verify_against_dummy("anything");
        assert!(PasswordHash::new(&hash_password("wayhouse-ui-dummy-password")).is_ok());
    }

    #[test]
    fn a_malformed_hash_never_matches_rather_than_panicking() {
        assert!(!verify_password("not a real hash", "anything"));
    }

    #[test]
    fn load_parses_a_well_formed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.yaml");
        let hash = hash_password("secret");
        std::fs::write(
            &path,
            format!(
                "users:\n  - username: alice\n    password_hash: \"{hash}\"\n    role: admin\n"
            ),
        )
        .unwrap();

        let users = load(&path).unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users["alice"].role, Role::Admin);
    }

    #[test]
    fn load_rejects_a_duplicate_username() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.yaml");
        std::fs::write(
            &path,
            "users:\n  \
             - username: alice\n    password_hash: \"x\"\n    role: viewer\n  \
             - username: alice\n    password_hash: \"y\"\n    role: admin\n",
        )
        .unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn load_rejects_an_empty_users_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.yaml");
        std::fs::write(&path, "users: []\n").unwrap();
        assert!(load(&path).is_err());
    }
}
