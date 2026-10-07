//! Verification of a downloaded artifact against its index entry.

use crate::index::VersionEntry;
use sha2::{Digest, Sha256};

/// What an artifact passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    /// A minisign signature was present and verified against the official key.
    pub signed: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("downloaded {got} bytes but the index says {expected}")]
    Size { expected: u64, got: usize },
    #[error("sha256 mismatch: index says {expected}, downloaded file is {got}")]
    Sha256 { expected: String, got: String },
    #[error("signature check failed: {0}")]
    Signature(String),
    #[error("not a valid sniffer module: {0}")]
    Module(String),
}

/// The ABI a module declares in its `wayhouse.abi` custom section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbiDecl {
    pub major: u16,
    pub minor: u16,
}

/// Check size, then sha256, then the signature, then the module itself, in that order.
///
/// A signature that is present is checked whenever a key is given, and a failure is
/// an error, never a quiet downgrade to "unsigned". No signature (or no key to check
/// it with) gives `signed: false`: signatures are optional in the first cut.
pub fn verify_artifact(
    v: &VersionEntry,
    bytes: &[u8],
    signature: Option<&[u8]>,
    official_key: Option<&minisign_verify::PublicKey>,
) -> Result<Verified, VerifyError> {
    if bytes.len() as u64 != v.size {
        return Err(VerifyError::Size {
            expected: v.size,
            got: bytes.len(),
        });
    }
    let got = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if got != v.sha256 {
        return Err(VerifyError::Sha256 {
            expected: v.sha256.clone(),
            got,
        });
    }
    let signed = match (signature, official_key) {
        (Some(sig), Some(key)) => {
            let text = std::str::from_utf8(sig)
                .map_err(|_| VerifyError::Signature("signature is not UTF-8 text".into()))?;
            let sig = minisign_verify::Signature::decode(text)
                .map_err(|e| VerifyError::Signature(e.to_string()))?;
            key.verify(bytes, &sig, false)
                .map_err(|e| VerifyError::Signature(e.to_string()))?;
            true
        }
        _ => false,
    };
    let declared = check_module(bytes)?;
    if format!("{}.{}", declared.major, declared.minor) != v.abi {
        return Err(VerifyError::Module(format!(
            "module declares ABI {}.{} but the index says {}",
            declared.major, declared.minor, v.abi
        )));
    }
    Ok(Verified { signed })
}

/// Read the declared ABI from the module's custom section and reject modules the
/// proxy would refuse anyway: any import, a missing or malformed ABI section, or
/// anything that is not a core module. Parsed only; no guest code runs.
pub fn check_module(bytes: &[u8]) -> Result<AbiDecl, VerifyError> {
    let bad = |e: String| VerifyError::Module(e);
    wasmparser::Validator::new()
        .validate_all(bytes)
        .map_err(|e| bad(e.to_string()))?;
    let mut abi: Option<&[u8]> = None;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        match payload.map_err(|e| bad(e.to_string()))? {
            wasmparser::Payload::Version { encoding, .. }
                if encoding != wasmparser::Encoding::Module =>
            {
                return Err(bad("not a core WebAssembly module".into()));
            }
            wasmparser::Payload::ImportSection(r) if r.count() > 0 => {
                return Err(bad(
                    "module has imports; sniffers must not import anything".into()
                ));
            }
            wasmparser::Payload::CustomSection(c) if c.name() == "wayhouse.abi" => {
                if abi.is_some() {
                    return Err(bad("more than one wayhouse.abi section".into()));
                }
                abi = Some(c.data());
            }
            _ => {}
        }
    }
    match abi {
        Some(&[a, b, c, d]) => Ok(AbiDecl {
            major: u16::from_le_bytes([a, b]),
            minor: u16::from_le_bytes([c, d]),
        }),
        Some(other) => Err(bad(format!(
            "wayhouse.abi section is {} bytes, expected 4",
            other.len()
        ))),
        None => Err(bad(
            "module declares no ABI version (custom section wayhouse.abi)".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Limits;
    use std::io::Cursor;

    fn module(abi_section: bool, import: bool) -> Vec<u8> {
        let imp = if import {
            r#"(import "env" "f" (func))"#
        } else {
            ""
        };
        let abi = if abi_section {
            r#"(@custom "wayhouse.abi" "\00\00\01\00")"#
        } else {
            ""
        };
        wat::parse_str(format!("(module {imp} {abi} (memory 1))")).unwrap()
    }

    fn entry_for(bytes: &[u8]) -> VersionEntry {
        VersionEntry {
            version: "0.1.0".parse().unwrap(),
            abi: "0.1".into(),
            min_proxy: "0.1.0".parse().unwrap(),
            url: "https://example.com/a.wasm".into(),
            sha256: hex(&Sha256::digest(bytes)),
            size: bytes.len() as u64,
            signature_url: None,
            limits: Limits {
                max_memory_bytes: 1,
                call_timeout_ms: 1,
            },
            config: None,
        }
    }

    fn hex(d: &[u8]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A throwaway key pair, the public half decoded for `minisign-verify` and the
    /// minisign-formatted signature of `data`.
    fn sign(data: &[u8]) -> (minisign_verify::PublicKey, Vec<u8>) {
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let sig = minisign::sign(Some(&kp.pk), &kp.sk, Cursor::new(data), None, None).unwrap();
        let pk = minisign_verify::PublicKey::decode(&kp.pk.to_box().unwrap().to_string()).unwrap();
        (pk, sig.to_string().into_bytes())
    }

    #[test]
    fn ok_unsigned() {
        let m = module(true, false);
        assert_eq!(
            verify_artifact(&entry_for(&m), &m, None, None),
            Ok(Verified { signed: false })
        );
    }

    #[test]
    fn ok_signed() {
        let m = module(true, false);
        let (pk, sig) = sign(&m);
        assert_eq!(
            verify_artifact(&entry_for(&m), &m, Some(&sig), Some(&pk)),
            Ok(Verified { signed: true })
        );
    }

    #[test]
    fn bad_sha_is_sha_error_before_signature() {
        let m = module(true, false);
        let (pk, _) = sign(&m);
        let mut e = entry_for(&m);
        e.sha256 = "0".repeat(64);
        // The signature is garbage too; sha256 is checked first.
        let r = verify_artifact(&e, &m, Some(b"garbage"), Some(&pk));
        assert!(matches!(r, Err(VerifyError::Sha256 { .. })), "{r:?}");
    }

    #[test]
    fn flipped_byte_with_valid_sha_still_fails_signature() {
        let m = module(true, false);
        let (pk, sig) = sign(&m);
        // The attacker serves tampered bytes and recomputes the index sha256 for them.
        let mut tampered = m.clone();
        tampered.extend_from_slice(&[0, 1, b'x']); // an extra, valid custom section
        let e = entry_for(&tampered);
        let r = verify_artifact(&e, &tampered, Some(&sig), Some(&pk));
        assert!(matches!(r, Err(VerifyError::Signature(_))), "{r:?}");
    }

    #[test]
    fn signature_present_but_no_key_is_unsigned_ok() {
        let m = module(true, false);
        let (_, sig) = sign(&m);
        assert_eq!(
            verify_artifact(&entry_for(&m), &m, Some(&sig), None),
            Ok(Verified { signed: false })
        );
    }

    #[test]
    fn key_but_no_signature_is_unsigned_ok() {
        let m = module(true, false);
        let (pk, _) = sign(&m);
        assert_eq!(
            verify_artifact(&entry_for(&m), &m, None, Some(&pk)),
            Ok(Verified { signed: false })
        );
    }

    #[test]
    fn signature_by_another_key_fails() {
        let m = module(true, false);
        let (pk, _) = sign(&m);
        let (_, other_sig) = sign(&m);
        let r = verify_artifact(&entry_for(&m), &m, Some(&other_sig), Some(&pk));
        assert!(matches!(r, Err(VerifyError::Signature(_))), "{r:?}");
    }

    #[test]
    fn size_mismatch() {
        let m = module(true, false);
        let mut e = entry_for(&m);
        e.size += 1;
        assert_eq!(
            verify_artifact(&e, &m, None, None),
            Err(VerifyError::Size {
                expected: e.size,
                got: m.len()
            })
        );
    }

    #[test]
    fn module_with_import_rejected() {
        let m = module(true, true);
        let r = verify_artifact(&entry_for(&m), &m, None, None);
        assert!(matches!(r, Err(VerifyError::Module(msg)) if msg.contains("import")));
    }

    #[test]
    fn module_without_abi_section_rejected() {
        let m = module(false, false);
        assert!(
            matches!(check_module(&m), Err(VerifyError::Module(msg)) if msg.contains("wayhouse.abi"))
        );
    }

    #[test]
    fn module_abi_is_read() {
        assert_eq!(
            check_module(&module(true, false)),
            Ok(AbiDecl { major: 0, minor: 1 })
        );
    }

    #[test]
    fn module_abi_must_equal_the_index_abi() {
        let m = module(true, false);
        let mut e = entry_for(&m);
        e.abi = "0.2".into();
        let r = verify_artifact(&e, &m, None, None);
        assert!(matches!(r, Err(VerifyError::Module(msg)) if msg.contains("0.2")));
    }

    #[test]
    fn module_with_an_invalid_function_body_is_rejected() {
        // Parses and carries a valid ABI section, but `i32.add` on an empty stack does
        // not validate, so the proxy's compiler would refuse it.
        let bad = wat::parse_str(
            r#"(module (func (export "f") i32.add drop) (@custom "wayhouse.abi" "\00\00\01\00"))"#,
        )
        .unwrap();
        let r = check_module(&bad);
        assert!(matches!(r, Err(VerifyError::Module(_))), "{r:?}");
    }

    #[test]
    fn garbage_bytes_are_an_error_not_a_panic() {
        assert!(matches!(
            check_module(b"not wasm"),
            Err(VerifyError::Module(_))
        ));
        assert!(matches!(check_module(b""), Err(VerifyError::Module(_))));
    }
}
