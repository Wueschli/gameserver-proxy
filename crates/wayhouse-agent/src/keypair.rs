//! Persists this agent's WireGuard private key across restarts — an origin's
//! registered identity (`backend_sources[].pubkey` pins against it, see
//! `docs/11-backend-transport.md`) must be stable, not regenerated on every
//! boot.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use defguard_wireguard_rs::key::Key;

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Loads the private key at `path` (base64, `Key`'s own `Display` format),
/// or generates and persists a new one if the file doesn't exist yet.
pub fn load_or_generate(path: &Path) -> anyhow::Result<Key> {
    if path.exists() {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading private key from {}", path.display()))?;
        return Key::try_from(text.trim()).map_err(|e| {
            anyhow::anyhow!(
                "{} does not hold a valid WireGuard key: {e}",
                path.display()
            )
        });
    }

    let key = Key::generate();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating private key file {}", path.display()))?;
    file.write_all(key.to_string().as_bytes())
        .with_context(|| format!("writing private key to {}", path.display()))?;
    drop(file);
    restrict_permissions(path)?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_and_persists_a_key_on_first_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.key");
        assert!(!path.exists());

        let key = load_or_generate(&path).unwrap();
        assert!(path.exists());
        assert_eq!(key.to_string().len(), 44); // base64 of 32 bytes
    }

    #[test]
    fn a_second_load_returns_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.key");

        let first = load_or_generate(&path).unwrap();
        let second = load_or_generate(&path).unwrap();
        assert_eq!(first.to_string(), second.to_string());
    }

    #[test]
    fn a_corrupt_key_file_is_a_clear_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.key");
        fs::write(&path, "not a key").unwrap();
        assert!(load_or_generate(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_persisted_key_file_is_not_world_or_group_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.key");
        load_or_generate(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
