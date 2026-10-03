//! The tunnel address this agent was last assigned, persisted next to its key
//! so a restart can come up while the controller is unreachable (spec:
//! "Persistence and offline behaviour"). The controller keeps an owner's
//! address sticky, so the saved value is still valid unless an operator
//! released it. Mirrored in `gsp::tunnel_address` rather than shared (the two
//! binaries have no common library; same precedent as `register.rs`).

use std::path::Path;

use anyhow::Context;

use crate::register::{RegisterError, Registered};

/// `10.60.0.5/16` → `10.60.0.5`.
pub fn ip_of(cidr: &str) -> &str {
    cidr.split_once('/').map(|(ip, _)| ip).unwrap_or(cidr)
}

/// The interface address: the assigned IP with the *network's* prefix, or — in
/// pin-only mode, when the controller reports no network — with the prefix of
/// the operator's pinned `--address`.
pub fn interface_cidr(
    assigned_ip: &str,
    network: Option<&str>,
    pinned_cidr: Option<&str>,
) -> anyhow::Result<String> {
    let source = match (network, pinned_cidr) {
        (Some(n), _) => n,
        (None, Some(p)) => p,
        (None, None) => anyhow::bail!(
            "the controller reported no tunnel_network and no --address was given whose \
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
    /// error, and `pin_ignored` says so when a pinned `--address` differs from the
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
                            "--address {p} differs from the saved tunnel address {cidr}; \
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

    fn unreachable() -> Result<Registered, RegisterError> {
        Err(RegisterError::Transient(anyhow::anyhow!(
            "connection refused"
        )))
    }

    #[test]
    fn a_fallback_carries_the_last_error_and_reports_an_overridden_pin() {
        let s = resolve_startup(unreachable(), None, Some("10.60.0.5/16".into())).unwrap();
        let Source::Saved { cause, pin_ignored } = s.source else {
            panic!("expected Saved");
        };
        assert!(cause.contains("connection refused"), "{cause}");
        assert_eq!(pin_ignored, None);

        let s = resolve_startup(
            unreachable(),
            Some("10.60.0.7/16"),
            Some("10.60.0.5/16".into()),
        )
        .unwrap();
        assert_eq!(s.cidr, "10.60.0.5/16");
        let Source::Saved { pin_ignored, .. } = s.source else {
            panic!("expected Saved");
        };
        let msg = pin_ignored.expect("a changed pin must be reported");
        assert!(msg.contains("--address 10.60.0.7/16"), "{msg}");
    }
}
