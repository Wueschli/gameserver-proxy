//! Native TLS: loading the certificate/key pair and swapping it at runtime.

use std::error::Error as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gsp_http::tls::{load_certified_key, ReloadingCert, TlsError, TlsFiles};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn files(cert: &str, key: &str) -> TlsFiles {
    TlsFiles::new(fixture(cert), fixture(key))
}

/// The source's text appears only via `source()`, never in the message.
fn assert_cause_once(e: &TlsError) {
    if let Some(src) = e.source() {
        assert!(!e.to_string().contains(&src.to_string()), "{e}");
    }
}

#[test]
fn loads_the_fixture_pair() {
    let key = load_certified_key(&files("leaf.pem", "leaf.key")).unwrap();
    assert_eq!(key.cert.len(), 1);
}

#[test]
fn loads_a_sec1_key() {
    load_certified_key(&files("leaf.pem", "leaf.sec1.key")).unwrap();
}

#[test]
fn startup_errors_name_the_file() {
    let missing = load_certified_key(&TlsFiles::new(
        PathBuf::from("/nonexistent/cert.pem"),
        fixture("leaf.key"),
    ))
    .unwrap_err();
    assert!(matches!(missing, TlsError::Read { .. }), "{missing:?}");
    assert!(
        missing
            .to_string()
            .starts_with("--tls-cert /nonexistent/cert.pem: "),
        "{missing}"
    );
    assert_cause_once(&missing);

    let no_cert = load_certified_key(&files("leaf.key", "leaf.key")).unwrap_err();
    assert!(
        matches!(no_cert, TlsError::NoCertificate { .. }),
        "{no_cert:?}"
    );
    assert!(no_cert.to_string().starts_with("--tls-cert "), "{no_cert}");

    let no_key = load_certified_key(&files("leaf.pem", "leaf.pem")).unwrap_err();
    assert!(matches!(no_key, TlsError::NoKey { .. }), "{no_key:?}");
    let want = format!("--tls-key {}: ", fixture("leaf.pem").display());
    assert!(no_key.to_string().starts_with(&want), "{no_key}");

    let mismatch = load_certified_key(&files("leaf.pem", "leaf2.key")).unwrap_err();
    assert!(
        matches!(mismatch, TlsError::KeyMismatch { .. }),
        "{mismatch:?}"
    );
    assert_cause_once(&mismatch);
}

/// A temp dir holding `cert.pem`/`key.pem` copies, so tests can rewrite them.
struct Live {
    dir: tempfile::TempDir,
}

impl Live {
    fn new(cert: &str, key: &str) -> Self {
        let live = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        live.write_cert(&std::fs::read(fixture(cert)).unwrap());
        live.write_key(&std::fs::read(fixture(key)).unwrap());
        live
    }
    fn files(&self) -> TlsFiles {
        TlsFiles::new(
            self.dir.path().join("cert.pem"),
            self.dir.path().join("key.pem"),
        )
    }
    /// Writes, then waits until the mtime has visibly moved on.
    fn write(&self, name: &str, bytes: &[u8]) {
        let path = self.dir.path().join(name);
        let before = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        loop {
            std::fs::write(&path, bytes).unwrap();
            let after = std::fs::metadata(&path).unwrap().modified().unwrap();
            if Some(after) != before {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn write_cert(&self, bytes: &[u8]) {
        self.write("cert.pem", bytes);
    }
    fn write_key(&self, bytes: &[u8]) {
        self.write("key.pem", bytes);
    }
}

fn leaf_der(name: &str) -> Vec<u8> {
    load_certified_key(&files(name, &name.replace(".pem", ".key")))
        .unwrap()
        .cert[0]
        .to_vec()
}

#[test]
fn rotation_swaps_the_served_cert() {
    let live = Live::new("leaf.pem", "leaf.key");
    let cert = ReloadingCert::new(live.files()).unwrap();
    assert!(!cert.reload_if_changed().unwrap());
    live.write_cert(&std::fs::read(fixture("leaf2.pem")).unwrap());
    live.write_key(&std::fs::read(fixture("leaf2.key")).unwrap());
    assert!(cert.reload_if_changed().unwrap());
    assert_eq!(cert.current().cert[0].to_vec(), leaf_der("leaf2.pem"));
}

/// `cp -p` or `touch -r` can replace a file without moving its mtime; the change
/// must still be picked up. The rewrite keeps the length and the inode too, so
/// only the ctime can tell (the .key fixtures are all 241 bytes).
#[test]
fn a_rewrite_that_keeps_the_mtime_is_picked_up() {
    let live = Live::new("leaf.pem", "leaf.key");
    let cert = ReloadingCert::new(live.files()).unwrap();
    let key = live.files().key;
    let before = std::fs::metadata(&key).unwrap();
    // Past one tick of even a coarse (jiffy) timestamp clock.
    std::thread::sleep(Duration::from_millis(50));
    let replacement = std::fs::read(fixture("leaf2.key")).unwrap();
    assert_eq!(
        replacement.len() as u64,
        before.len(),
        "fixtures changed size"
    );
    std::fs::write(&key, replacement).unwrap(); // in place: same inode
    std::fs::File::options()
        .write(true)
        .open(&key)
        .unwrap()
        .set_modified(before.modified().unwrap())
        .unwrap();
    let after = std::fs::metadata(&key).unwrap();
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
    assert_eq!(after.len(), before.len());
    // The new key doesn't match the old certificate: noticing the change means
    // trying the pair and refusing it; Ok(false) would mean it went unseen.
    let seen = cert.reload_if_changed();
    assert!(
        matches!(seen, Err(TlsError::KeyMismatch { .. })),
        "{seen:?}"
    );
}

#[test]
fn half_rotated_pair_keeps_the_old_cert() {
    let live = Live::new("leaf.pem", "leaf.key");
    let cert = ReloadingCert::new(live.files()).unwrap();
    live.write_cert(&std::fs::read(fixture("leaf2.pem")).unwrap());
    let e = cert.reload_if_changed().unwrap_err();
    assert!(matches!(e, TlsError::KeyMismatch { .. }), "{e:?}");
    assert_eq!(cert.current().cert[0].to_vec(), leaf_der("leaf.pem"));
    live.write_key(&std::fs::read(fixture("leaf2.key")).unwrap());
    assert!(cert.reload_if_changed().unwrap());
    assert_eq!(cert.current().cert[0].to_vec(), leaf_der("leaf2.pem"));
}

#[test]
fn garbage_on_reload_keeps_the_old_cert() {
    let live = Live::new("leaf.pem", "leaf.key");
    let cert = ReloadingCert::new(live.files()).unwrap();
    live.write_cert(b"junk");
    let e = cert.reload_if_changed().unwrap_err();
    assert!(matches!(e, TlsError::NoCertificate { .. }), "{e:?}");
    assert_eq!(cert.current().cert[0].to_vec(), leaf_der("leaf.pem"));
}

/// A certificate file cut off mid-chain (a renewal caught half-written) must be
/// rejected, not served as a leaf-only chain.
#[test]
fn a_truncated_chain_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("chain.pem");
    let mut pem = std::fs::read(fixture("leaf.pem")).unwrap();
    let ca = std::fs::read(fixture("ca.pem")).unwrap();
    pem.extend_from_slice(&ca[..ca.len() / 2]);
    std::fs::write(&cert, pem).unwrap();
    let e = load_certified_key(&TlsFiles::new(cert, fixture("leaf.key"))).unwrap_err();
    assert!(matches!(e, TlsError::BadCertificate { .. }), "{e:?}");
    assert!(e.to_string().starts_with("--tls-cert "), "{e}");
    assert_cause_once(&e);
}

/// `TlsArgs` built in code (not through clap's `requires`) with only one file must
/// not silently serve plain HTTP.
#[test]
fn half_a_tls_args_pair_is_an_error() {
    use gsp_http::tls::TlsArgs;
    let only_cert = TlsArgs {
        tls_cert: Some(fixture("leaf.pem")),
        tls_key: None,
        ..TlsArgs::default()
    };
    assert!(matches!(only_cert.load(), Err(TlsError::Incomplete)));
    let only_key = TlsArgs {
        tls_cert: None,
        tls_key: Some(fixture("leaf.key")),
        ..TlsArgs::default()
    };
    assert!(matches!(only_key.load(), Err(TlsError::Incomplete)));
    assert!(TlsArgs::default().load().unwrap().is_none());
    let both = TlsArgs {
        tls_cert: Some(fixture("leaf.pem")),
        tls_key: Some(fixture("leaf.key")),
        ..TlsArgs::default()
    };
    assert!(both.load().unwrap().is_some());
}

/// A pair configured somewhere other than `--tls-cert`/`--tls-key` (e.g. `gsp`'s
/// `settings.admin.tls`) names that setting in every error, not the flags.
#[test]
fn renamed_files_name_their_setting() {
    let named = |cert: PathBuf, key: PathBuf| {
        TlsFiles::new(cert, key).named("settings.admin.tls.cert", "settings.admin.tls.key")
    };
    let cases = [
        named(PathBuf::from("/nonexistent/cert.pem"), fixture("leaf.key")),
        named(fixture("leaf.pem"), PathBuf::from("/nonexistent/key.pem")),
        named(fixture("leaf.key"), fixture("leaf.key")),
        named(fixture("leaf.pem"), fixture("leaf.pem")),
        named(fixture("leaf.pem"), fixture("leaf2.key")),
    ];
    for files in cases {
        let text = load_certified_key(&files).unwrap_err().to_string();
        assert!(text.contains("settings.admin.tls."), "{text}");
        assert!(!text.contains("--tls"), "{text}");
    }
    // The default names stay the flags.
    let text = load_certified_key(&TlsFiles::new(
        PathBuf::from("/nonexistent/cert.pem"),
        fixture("leaf.key"),
    ))
    .unwrap_err()
    .to_string();
    assert!(
        text.starts_with("--tls-cert /nonexistent/cert.pem"),
        "{text}"
    );
}

#[test]
fn loads_a_pkcs1_rsa_key() {
    load_certified_key(&files("leaf-rsa.pem", "leaf-rsa.pkcs1.key")).unwrap();
}
