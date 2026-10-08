//! Reading a plugin module's ABI version and capabilities without running it.

use sha2::{Digest, Sha256};
use wayhouse_plugin_abi::{ABI_MAJOR, ABI_MINOR, ABI_SECTION, CAPS_SECTION};

use crate::caps::Capabilities;

/// Largest module the host accepts (as for sniffers).
pub const MAX_MODULE_BYTES: usize = 8 * 1024 * 1024;

/// The ABI version a plugin declares, or the host speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbiVersion {
    pub major: u16,
    pub minor: u16,
}

impl std::fmt::Display for AbiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The ABI this host speaks.
pub const HOST_ABI: AbiVersion = AbiVersion {
    major: ABI_MAJOR,
    minor: ABI_MINOR,
};

/// Static facts about a module, read from its bytes alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfo {
    pub abi: AbiVersion,
    pub caps: Capabilities,
    /// Lower-case hex sha256 of the module bytes; approvals are recorded against it.
    pub sha256: String,
}

/// Why a byte string is not a loadable plugin module.
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("the module is empty")]
    Empty,
    #[error("the module is {len} bytes, over the {max} byte limit")]
    TooLarge { len: usize, max: usize },
    #[error("the module has no `wayhouse.plugin-abi` section; link wayhouse-plugin-abi {HOST_ABI}")]
    AbiMissing,
    #[error("the `wayhouse.plugin-abi` section is malformed ({0} bytes, or declared twice)")]
    AbiMalformed(usize),
    #[error("the plugin declares ABI {plugin}, this host speaks {host}")]
    AbiIncompatible { plugin: AbiVersion, host: AbiVersion },
    #[error("the module has no `wayhouse.plugin-caps` section")]
    CapsMissing,
    #[error("the module declares `wayhouse.plugin-caps` more than once")]
    CapsDuplicate,
    #[error("invalid capability declaration: {0}")]
    CapsInvalid(String),
    #[error("`on_timer` is declared without a `tick_interval_secs`")]
    UndeclaredTickInterval,
    #[error("`tick_interval_secs` is {0}, below the 10 second minimum")]
    TickIntervalTooShort(u64),
    #[error("the module does not compile: {0}")]
    Compile(String),
    #[error("the module imports `{0}`, which is not part of the plugin ABI")]
    UnexpectedImport(String),
    #[error("the module does not export `{0}`")]
    MissingExport(&'static str),
    #[error("the export `{0}` has the wrong type")]
    WrongSignature(&'static str),
    #[error("the module does not instantiate: {0}")]
    Instantiate(String),
}

/// Check size, ABI version and capability declaration of `bytes`. Nothing is compiled
/// or instantiated, so no guest code runs.
pub fn inspect(bytes: &[u8]) -> Result<ModuleInfo, ModuleError> {
    if bytes.is_empty() {
        return Err(ModuleError::Empty);
    }
    if bytes.len() > MAX_MODULE_BYTES {
        return Err(ModuleError::TooLarge {
            len: bytes.len(),
            max: MAX_MODULE_BYTES,
        });
    }
    let mut abi: Option<&[u8]> = None;
    let mut caps: Option<&[u8]> = None;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| ModuleError::Compile(e.to_string()))?;
        if let wasmparser::Payload::CustomSection(c) = payload {
            if c.name() == ABI_SECTION {
                if abi.is_some() {
                    return Err(ModuleError::AbiMalformed(c.data().len()));
                }
                abi = Some(c.data());
            } else if c.name() == CAPS_SECTION {
                if caps.is_some() {
                    return Err(ModuleError::CapsDuplicate);
                }
                caps = Some(c.data());
            }
        }
    }
    let abi = match abi {
        None => return Err(ModuleError::AbiMissing),
        Some(&[a, b, c, d]) => AbiVersion {
            major: u16::from_le_bytes([a, b]),
            minor: u16::from_le_bytes([c, d]),
        },
        Some(other) => return Err(ModuleError::AbiMalformed(other.len())),
    };
    // Exact match while the major is 0: a minor bump may break the ABI.
    if abi != HOST_ABI {
        return Err(ModuleError::AbiIncompatible {
            plugin: abi,
            host: HOST_ABI,
        });
    }
    let caps = Capabilities::parse(caps.ok_or(ModuleError::CapsMissing)?)?;
    let sha256 = Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        });
    Ok(ModuleInfo { abi, caps, sha256 })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A WAT module with the given abi bytes (escaped) and caps JSON sections.
    pub(crate) fn wat_with(abi: Option<&str>, caps: &[&str], body: &str) -> Vec<u8> {
        let mut src = String::from("(module\n");
        if let Some(a) = abi {
            src += &format!("(@custom \"wayhouse.plugin-abi\" \"{a}\")\n");
        }
        for c in caps {
            let esc = c.replace('\\', "\\\\").replace('"', "\\\"");
            src += &format!("(@custom \"wayhouse.plugin-caps\" \"{esc}\")\n");
        }
        src += body;
        src += ")";
        wat::parse_str(src).unwrap()
    }

    pub(crate) const GOOD_ABI: &str = "\\00\\00\\01\\00";
    const GOOD_CAPS: &str = r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true}"#;

    #[test]
    fn accepts_a_valid_module() {
        let m = wat_with(Some(GOOD_ABI), &[GOOD_CAPS], "");
        let info = inspect(&m).unwrap();
        assert_eq!(info.abi, HOST_ABI);
        assert!(info.caps.log && info.caps.triggers.on_timer);
        assert_eq!(info.caps.tick_interval_secs, 30);
        assert_eq!(info.sha256.len(), 64);
    }

    #[test]
    fn rejects_empty_and_oversize() {
        assert!(matches!(inspect(&[]), Err(ModuleError::Empty)));
        let big = vec![0u8; MAX_MODULE_BYTES + 1];
        assert!(matches!(inspect(&big), Err(ModuleError::TooLarge { .. })));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            inspect(b"not wasm at all"),
            Err(ModuleError::Compile(_))
        ));
    }

    #[test]
    fn rejects_missing_or_wrong_abi() {
        let m = wat_with(None, &[GOOD_CAPS], "");
        assert!(matches!(inspect(&m), Err(ModuleError::AbiMissing)));
        let m = wat_with(Some("\\00\\00\\02\\00"), &[GOOD_CAPS], "");
        assert!(matches!(
            inspect(&m),
            Err(ModuleError::AbiIncompatible { .. })
        ));
        let m = wat_with(Some("\\00\\00\\01"), &[GOOD_CAPS], "");
        assert!(matches!(inspect(&m), Err(ModuleError::AbiMalformed(3))));
    }

    #[test]
    fn rejects_missing_duplicate_and_invalid_caps() {
        let m = wat_with(Some(GOOD_ABI), &[], "");
        assert!(matches!(inspect(&m), Err(ModuleError::CapsMissing)));
        let m = wat_with(Some(GOOD_ABI), &[GOOD_CAPS, GOOD_CAPS], "");
        assert!(matches!(inspect(&m), Err(ModuleError::CapsDuplicate)));
        let m = wat_with(Some(GOOD_ABI), &["{not json"], "");
        assert!(matches!(inspect(&m), Err(ModuleError::CapsInvalid(_))));
    }

    #[test]
    fn rejects_unknown_capability_fields() {
        let m = wat_with(Some(GOOD_ABI), &[r#"{"log":true,"filesystem":true}"#], "");
        assert!(matches!(inspect(&m), Err(ModuleError::CapsInvalid(_))));
    }

    #[test]
    fn enforces_the_tick_interval_minimum() {
        let m = wat_with(
            Some(GOOD_ABI),
            &[r#"{"triggers":{"on_timer":true},"tick_interval_secs":9}"#],
            "",
        );
        assert!(matches!(
            inspect(&m),
            Err(ModuleError::TickIntervalTooShort(9))
        ));
        let m = wat_with(Some(GOOD_ABI), &[r#"{"triggers":{"on_timer":true}}"#], "");
        assert!(matches!(
            inspect(&m),
            Err(ModuleError::UndeclaredTickInterval)
        ));
    }

    #[test]
    fn sha256_is_of_the_module_bytes() {
        let m = wat_with(Some(GOOD_ABI), &[GOOD_CAPS], "");
        let expected = Sha256::digest(&m)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(inspect(&m).unwrap().sha256, expected);
    }
}
