//! The tunnel address this proxy was last assigned, persisted next to its key
//! so a restart can come up while the controller is unreachable (spec:
//! "Persistence and offline behaviour"). The controller keeps an owner's
//! address sticky, so the saved value is still valid unless an operator
//! released it. Mirrored from `gsp-agent::address_store` rather than shared (the two
//! binaries have no common library; same precedent as `proxy_register.rs`).

use std::path::Path;

use anyhow::Context;

use crate::proxy_register::{RegisterError, Registered};

/// `10.60.0.5/16` → `10.60.0.5`.
pub fn ip_of(cidr: &str) -> &str {
    cidr.split_once('/').map(|(ip, _)| ip).unwrap_or(cidr)
}

/// The interface address: the assigned IP with the *network's* prefix, or — in
/// pin-only mode, when the controller reports no network — with the prefix of
/// the operator's pinned `--tunnel-address`.
pub fn interface_cidr(
    assigned_ip: &str,
    network: Option<&str>,
    pinned_cidr: Option<&str>,
) -> anyhow::Result<String> {
    let source = match (network, pinned_cidr) {
        (Some(n), _) => n,
        (None, Some(p)) => p,
        (None, None) => anyhow::bail!(
            "the controller reported no tunnel_network and no --tunnel-address was given whose \
             prefix could be used for the interface"
        ),
    };
    let prefix = source
        .rsplit_once('/')
        .map(|(_, p)| p)
        .with_context(|| format!("{source:?} has no /prefix"))?;
    Ok(format!("{assigned_ip}/{prefix}"))
}

pub fn load(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save(path: &Path, cidr: &str) -> anyhow::Result<()> {
    std::fs::write(path, format!("{cidr}\n"))
        .with_context(|| format!("saving the tunnel address to {}", path.display()))
}

#[derive(Debug)]
pub enum Source {
    Controller,
    /// The controller could not be reached; `cause` is the last transient
    /// error, and `pin_ignored` says so when a pinned `--tunnel-address` differs from the
    /// saved address it lost to.
    Saved {
        cause: String,
        pin_ignored: Option<String>,
    },
}

#[derive(Debug)]
pub struct StartupAddress {
    pub cidr: String,
    pub source: Source,
}

/// Decides the interface address at startup from the registration outcome:
/// the controller's answer wins; a *transient* failure falls back to the saved
/// address (or fails loudly if there is none); a *rejection* (409/422) never
/// falls back — a stale saved value must not mask an address conflict.
pub fn resolve_startup(
    outcome: Result<Registered, RegisterError>,
    pinned_cidr: Option<&str>,
    saved: Option<String>,
) -> anyhow::Result<StartupAddress> {
    match outcome {
        Ok(r) => Ok(StartupAddress {
            cidr: interface_cidr(&r.tunnel_address, r.tunnel_network.as_deref(), pinned_cidr)?,
            source: Source::Controller,
        }),
        Err(e @ RegisterError::Rejected(_)) => {
            Err(anyhow::Error::new(e).context("the controller refused this tunnel registration"))
        }
        Err(e) => match saved {
            Some(cidr) => Ok(StartupAddress {
                source: Source::Saved {
                    cause: format!("{e:#}"),
                    pin_ignored: pinned_cidr.filter(|p| ip_of(p) != ip_of(&cidr)).map(|p| {
                        format!(
                            "--tunnel-address {p} differs from the saved tunnel address {cidr}; \
                                 keeping {cidr} — restart once the controller is reachable to apply the pin"
                        )
                    }),
                },
                cidr,
            }),
            None => Err(anyhow::Error::new(e).context(
                "could not register with the controller and there is no saved tunnel address \
                 to fall back on",
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy_register::{RegisterError, Registered};

    fn registered(ip: &str, net: Option<&str>) -> Registered {
        Registered {
            revision: 1,
            tunnel_address: ip.into(),
            tunnel_network: net.map(str::to_string),
        }
    }

    #[test]
    fn ip_of_strips_the_prefix() {
        assert_eq!(ip_of("10.60.0.5/16"), "10.60.0.5");
        assert_eq!(ip_of("10.60.0.5"), "10.60.0.5");
    }

    #[test]
    fn the_networks_prefix_wins_over_a_pinned_one() {
        assert_eq!(
            interface_cidr("10.60.0.5", Some("10.60.0.0/16"), Some("10.60.0.5/24")).unwrap(),
            "10.60.0.5/16"
        );
    }

    #[test]
    fn pin_only_mode_uses_the_pinned_prefix_and_errors_without_one() {
        assert_eq!(
            interface_cidr("10.60.0.5", None, Some("10.60.0.5/24")).unwrap(),
            "10.60.0.5/24"
        );
        let err = interface_cidr("10.60.0.5", None, None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--tunnel-address"),
            "wrong flag name in: {err}"
        );
    }

    #[test]
    fn a_saved_address_round_trips_and_blank_or_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tunnel-address");
        assert_eq!(load(&path), None);
        save(&path, "10.60.0.5/16").unwrap();
        assert_eq!(load(&path).as_deref(), Some("10.60.0.5/16"));
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(load(&path), None);
    }

    #[test]
    fn startup_uses_the_controllers_answer_when_it_registers() {
        let s = resolve_startup(
            Ok(registered("10.60.0.5", Some("10.60.0.0/16"))),
            None,
            Some("10.60.0.9/16".into()),
        )
        .unwrap();
        assert_eq!(s.cidr, "10.60.0.5/16");
        assert!(matches!(s.source, Source::Controller));
    }

    #[test]
    fn startup_falls_back_to_the_saved_address_when_the_controller_is_unreachable() {
        let s = resolve_startup(
            Err(RegisterError::Transient(anyhow::anyhow!(
                "connection refused"
            ))),
            None,
            Some("10.60.0.5/16".into()),
        )
        .unwrap();
        assert_eq!(s.cidr, "10.60.0.5/16");
        match s.source {
            Source::Saved { cause, pin_ignored } => {
                assert!(cause.contains("connection refused"), "{cause}");
                assert_eq!(pin_ignored, None);
            }
            other => panic!("expected Saved, got {other:?}"),
        }
    }

    #[test]
    fn a_fallback_says_when_it_overrides_a_changed_pin() {
        let s = resolve_startup(
            Err(RegisterError::Transient(anyhow::anyhow!(
                "connection refused"
            ))),
            Some("10.60.0.7/16"),
            Some("10.60.0.5/16".into()),
        )
        .unwrap();
        assert_eq!(s.cidr, "10.60.0.5/16");
        let Source::Saved { pin_ignored, .. } = s.source else {
            panic!("expected Saved");
        };
        let msg = pin_ignored.expect("a changed pin must be reported");
        assert!(
            msg.contains("10.60.0.7") && msg.contains("10.60.0.5"),
            "{msg}"
        );
        assert!(msg.contains("--tunnel-address"), "{msg}");

        // The same IP under another prefix is not a changed pin.
        let s = resolve_startup(
            Err(RegisterError::Transient(anyhow::anyhow!("down"))),
            Some("10.60.0.5/24"),
            Some("10.60.0.5/16".into()),
        )
        .unwrap();
        assert!(matches!(
            s.source,
            Source::Saved {
                pin_ignored: None,
                ..
            }
        ));
    }

    #[test]
    fn startup_fails_without_a_saved_address_when_the_controller_is_unreachable() {
        let err = resolve_startup(
            Err(RegisterError::Transient(anyhow::anyhow!(
                "connection refused"
            ))),
            None,
            None,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("no saved tunnel address"),
            "{err:#}"
        );
    }

    #[test]
    fn startup_never_falls_back_when_the_controller_refuses_the_registration() {
        // A 409 (address held) must not be papered over by a stale saved value.
        let err = resolve_startup(
            Err(RegisterError::Rejected(
                "already held by origin \"x\"".into(),
            )),
            None,
            Some("10.60.0.5/16".into()),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("already held"), "{err:#}");
    }
}
