//! Version skew across the fleet (#185,
//! `docs/superpowers/specs/2026-10-05-component-versioning-design.md`, "Visibility").
//!
//! Every instance reports its product `version` and wire `protocol`. An instance
//! is compared with the newest version in the fleet:
//!
//! - [`Skew::None`]: same version (or it reports none, an older build that
//!   predates the field);
//! - [`Skew::WithinWindow`]: same protocol major and a product minor at most one
//!   behind (the supported N / N-1 window); the UI shows yellow;
//! - [`Skew::OutsideWindow`]: another protocol major, more than one minor behind,
//!   or it refused a request for an incompatible protocol within the last
//!   [`RECENT_MISMATCH_MS`]; the UI shows red.
//!
//! The mismatch rule needs a recent *increase*, not a non-zero counter, so a
//! node that mis-spoke once during an upgrade stops being red after five
//! minutes instead of flapping or staying red until restart.

use serde::Serialize;

/// How long a rise of the mismatch counter keeps a node red.
pub const RECENT_MISMATCH_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Skew {
    None,
    WithinWindow,
    OutsideWindow,
}

/// `major.minor` of a SemVer product version; the patch and any pre-release
/// suffix do not matter for the window.
fn product(v: &str) -> Option<(u64, u64)> {
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor))
}

fn protocol_major(p: &str) -> Option<u64> {
    p.split('.').next()?.parse().ok()
}

/// What `skew` needs to know about one instance.
#[derive(Debug, Clone, Copy)]
pub struct Reported<'a> {
    pub version: &'a str,
    pub protocol: &'a str,
    /// Milliseconds since the mismatch counter last rose, `None` if it never did.
    pub mismatch_rose_ms_ago: Option<u64>,
    /// Still pushing. A stale instance is classified but never sets the
    /// "newest" baseline, so a removed or rolled-back node cannot leave the
    /// rest of the fleet red.
    pub fresh: bool,
}

/// The skew of every entry of `fleet`, in order.
pub fn classify(fleet: &[Reported<'_>]) -> Vec<Skew> {
    let newest = fleet
        .iter()
        .filter(|r| r.fresh)
        .filter_map(|r| product(r.version))
        .max();
    let newest_protocol = fleet
        .iter()
        .filter(|r| r.fresh)
        .filter_map(|r| protocol_major(r.protocol))
        .max();
    fleet
        .iter()
        .map(|r| {
            if r.mismatch_rose_ms_ago
                .is_some_and(|ago| ago < RECENT_MISMATCH_MS)
            {
                return Skew::OutsideWindow;
            }
            if protocol_major(r.protocol).is_some_and(|p| Some(p) != newest_protocol) {
                return Skew::OutsideWindow;
            }
            match (product(r.version), newest) {
                (Some(v), Some(n)) if v == n => Skew::None,
                (Some((vm, vi)), Some((nm, ni))) if vm == nm && ni - vi <= 1 => Skew::WithinWindow,
                (Some(_), Some(_)) => Skew::OutsideWindow,
                _ => Skew::None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r<'a>(version: &'a str, protocol: &'a str) -> Reported<'a> {
        Reported {
            version,
            protocol,
            mismatch_rose_ms_ago: None,
            fresh: true,
        }
    }

    #[test]
    fn a_stale_newer_instance_does_not_set_the_baseline() {
        let mut gone = r("0.4.0", "2.0");
        gone.fresh = false;
        let f = [gone, r("0.2.0", "1.0"), r("0.2.0", "1.0")];
        assert_eq!(classify(&f)[1..], [Skew::None, Skew::None]);
    }

    #[test]
    fn same_version_everywhere_is_no_skew() {
        let f = [r("0.2.0", "1.0"), r("0.2.1", "1.0")];
        assert_eq!(classify(&f), [Skew::None, Skew::None]);
    }

    #[test]
    fn one_minor_behind_is_within_the_window() {
        let f = [r("0.3.0", "1.1"), r("0.2.4", "1.0")];
        assert_eq!(classify(&f), [Skew::None, Skew::WithinWindow]);
    }

    #[test]
    fn two_minors_behind_is_outside_the_window() {
        let f = [r("0.3.0", "1.1"), r("0.1.0", "1.0")];
        assert_eq!(classify(&f), [Skew::None, Skew::OutsideWindow]);
    }

    #[test]
    fn another_protocol_major_is_outside_the_window() {
        let f = [r("0.3.0", "2.0"), r("0.3.0", "1.0")];
        assert_eq!(classify(&f), [Skew::None, Skew::OutsideWindow]);
    }

    #[test]
    fn an_instance_without_version_info_is_not_flagged() {
        let f = [r("0.3.0", "1.0"), r("", "")];
        assert_eq!(classify(&f), [Skew::None, Skew::None]);
    }

    #[test]
    fn prerelease_and_patch_do_not_matter() {
        let f = [r("0.2.0", "1.0"), r("0.2.0-rc.1", "1.0")];
        assert_eq!(classify(&f), [Skew::None, Skew::None]);
    }

    #[test]
    fn a_recent_mismatch_is_red_even_on_the_same_version() {
        let mut bad = r("0.2.0", "1.0");
        bad.mismatch_rose_ms_ago = Some(RECENT_MISMATCH_MS - 1);
        assert_eq!(
            classify(&[r("0.2.0", "1.0"), bad]),
            [Skew::None, Skew::OutsideWindow]
        );
    }

    #[test]
    fn an_old_mismatch_stops_being_red() {
        let mut old = r("0.2.0", "1.0");
        old.mismatch_rose_ms_ago = Some(RECENT_MISMATCH_MS);
        assert_eq!(classify(&[old]), [Skew::None]);
    }
}
