//! Which version of a sniffer can be installed on this fleet, and why none can.

use crate::index::{parse_abi, SnifferEntry, VersionEntry};

/// What a sniffer will be installed into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    /// The ABI the host speaks, `major.minor`.
    pub abi: String,
    /// Version of every proxy the sniffer will be installed on; each must satisfy a
    /// version's `min_proxy`. Empty means unknown: the proxy check is skipped (the
    /// aggregator does not report proxy versions until the component-upgrades plan).
    pub proxy_versions: Vec<semver::Version>,
}

/// Why no version of a sniffer is installable. The `Display` texts are what the UI shows.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Incompatible {
    #[error("no versions are listed for this sniffer")]
    NoVersions,
    #[error("built for sniffer ABI {newest} (newest listed), but this proxy speaks ABI {host}")]
    Abi { newest: String, host: String },
    #[error("needs proxy {needs} or newer, but the oldest proxy is {oldest_proxy}")]
    ProxyTooOld {
        needs: semver::Version,
        oldest_proxy: semver::Version,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0:?} is not an ABI version of the form major.minor")]
pub struct AbiParseError(pub String);

/// Whether a sniffer built for `sniffer` runs on a host speaking `host`. While the
/// major is 0 a minor bump may break the ABI, so the two must match exactly (same
/// rule as the proxy's loader); from 1.0 on it is the same major and a sniffer minor
/// no newer than the host's.
pub fn abi_matches(sniffer: &str, host: &str) -> Result<bool, AbiParseError> {
    let parse = |s: &str| parse_abi(s).ok_or_else(|| AbiParseError(s.to_owned()));
    let (s, h) = (parse(sniffer)?, parse(host)?);
    Ok(if h.0 == 0 {
        s == h
    } else {
        s.0 == h.0 && s.1 <= h.1
    })
}

/// The newest version satisfying both the ABI and every proxy's `min_proxy`.
pub fn select<'a>(
    entry: &'a SnifferEntry,
    env: &Environment,
) -> Result<&'a VersionEntry, Incompatible> {
    if entry.versions.is_empty() {
        return Err(Incompatible::NoVersions);
    }
    // A pre-release proxy (`0.1.0-rc.1`) counts as its release for this comparison.
    let oldest_proxy = env
        .proxy_versions
        .iter()
        .min()
        .map(|p| semver::Version::new(p.major, p.minor, p.patch));
    let mut abi_ok: Vec<&VersionEntry> = Vec::new();
    for v in &entry.versions {
        // An unparseable abi string cannot match; the index validator rejects it earlier.
        if abi_matches(&v.abi, &env.abi).unwrap_or(false) {
            abi_ok.push(v);
        }
    }
    if abi_ok.is_empty() {
        return Err(Incompatible::Abi {
            newest: entry.versions[0].abi.clone(),
            host: env.abi.clone(),
        });
    }
    // Versions are sorted newest first, so the first match is the newest.
    for v in &abi_ok {
        match &oldest_proxy {
            Some(p) if &v.min_proxy > p => {}
            _ => return Ok(v),
        }
    }
    let needs = abi_ok
        .iter()
        .map(|v| &v.min_proxy)
        .min()
        .expect("abi_ok is not empty");
    Err(Incompatible::ProxyTooOld {
        needs: needs.clone(),
        oldest_proxy: oldest_proxy.expect("a proxy rejected every version"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Limits;

    fn v(s: &str) -> semver::Version {
        s.parse().unwrap()
    }

    fn ver(version: &str, abi: &str, min_proxy: &str) -> VersionEntry {
        VersionEntry {
            version: v(version),
            abi: abi.into(),
            min_proxy: v(min_proxy),
            url: "https://example.com/a.wasm".into(),
            sha256: "a".repeat(64),
            size: 1,
            signature_url: None,
            limits: Limits {
                max_memory_bytes: 1,
                call_timeout_ms: 1,
            },
            config: None,
        }
    }

    fn entry(versions: Vec<VersionEntry>) -> SnifferEntry {
        SnifferEntry {
            name: "s".into(),
            description: String::new(),
            license: "MIT".into(),
            homepage: None,
            versions,
        }
    }

    fn env(abi: &str, proxies: &[&str]) -> Environment {
        Environment {
            abi: abi.into(),
            proxy_versions: proxies.iter().map(|p| v(p)).collect(),
        }
    }

    #[test]
    fn selects_newest_compatible() {
        let e = entry(vec![
            ver("0.3.0", "0.1", "0.1.0"),
            ver("0.2.0", "0.1", "0.1.0"),
        ]);
        assert_eq!(
            select(&e, &env("0.1", &["0.1.0"])).unwrap().version,
            v("0.3.0")
        );
    }

    #[test]
    fn skips_newer_version_with_other_abi() {
        let e = entry(vec![
            ver("0.3.0", "0.2", "0.1.0"),
            ver("0.2.0", "0.1", "0.1.0"),
        ]);
        assert_eq!(
            select(&e, &env("0.1", &["0.1.0"])).unwrap().version,
            v("0.2.0")
        );
    }

    #[test]
    fn reports_abi_when_nothing_matches() {
        let e = entry(vec![
            ver("0.3.0", "0.2", "0.1.0"),
            ver("0.2.0", "0.2", "0.1.0"),
        ]);
        assert_eq!(
            select(&e, &env("0.1", &["0.1.0"])),
            Err(Incompatible::Abi {
                newest: "0.2".into(),
                host: "0.1".into()
            })
        );
    }

    #[test]
    fn reports_oldest_proxy_when_min_proxy_too_high() {
        let e = entry(vec![
            ver("0.3.0", "0.1", "0.4.0"),
            ver("0.2.0", "0.1", "0.3.0"),
        ]);
        assert_eq!(
            select(&e, &env("0.1", &["0.5.0", "0.2.0"])),
            Err(Incompatible::ProxyTooOld {
                needs: v("0.3.0"),
                oldest_proxy: v("0.2.0")
            })
        );
    }

    #[test]
    fn every_proxy_must_satisfy_min_proxy() {
        let e = entry(vec![
            ver("0.3.0", "0.1", "0.3.0"),
            ver("0.2.0", "0.1", "0.1.0"),
        ]);
        // One proxy is too old for 0.3.0, so the fleet-wide choice is 0.2.0.
        assert_eq!(
            select(&e, &env("0.1", &["0.3.0", "0.1.0"]))
                .unwrap()
                .version,
            v("0.2.0")
        );
    }

    #[test]
    fn unknown_proxy_versions_skip_the_proxy_check() {
        let e = entry(vec![ver("0.3.0", "0.1", "9.9.9")]);
        assert_eq!(select(&e, &env("0.1", &[])).unwrap().version, v("0.3.0"));
    }

    #[test]
    fn no_versions_is_reported() {
        assert_eq!(
            select(&entry(vec![]), &env("0.1", &[])),
            Err(Incompatible::NoVersions)
        );
    }

    #[test]
    fn abi_zero_major_requires_exact_minor() {
        assert_eq!(abi_matches("0.1", "0.1"), Ok(true));
        assert_eq!(abi_matches("0.0", "0.1"), Ok(false));
        assert_eq!(abi_matches("0.2", "0.1"), Ok(false));
    }

    #[test]
    fn abi_major_1_allows_older_minor() {
        assert_eq!(abi_matches("1.0", "1.2"), Ok(true));
        assert_eq!(abi_matches("1.3", "1.2"), Ok(false));
        assert_eq!(abi_matches("2.0", "1.2"), Ok(false));
    }

    #[test]
    fn abi_rejects_garbage_string() {
        for bad in ["", "1", "a.b", "1.2.3", "-1.0", "70000.0"] {
            assert!(abi_matches(bad, "0.1").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn prerelease_proxy_counts_as_its_release() {
        // 0.1.0-rc.1 is semver-older than 0.1.0, but a release candidate of 0.1.0
        // runs the 0.1.0 feature set, so its pre-release tag is ignored.
        let e = entry(vec![ver("0.1.0", "0.1", "0.1.0")]);
        assert!(select(&e, &env("0.1", &["0.1.0-rc.1"])).is_ok());
    }
}
