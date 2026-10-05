//! Startup checks for how a fleet binary's API is exposed and how strong its
//! shared secrets are (docs/security-review-2026-10.md O1, O7).
//!
//! Every fleet API is open when no token is configured, which is safe on the
//! default loopback bind but not on a routable address. These helpers let each
//! `main` refuse such a start up front. They return a plain message so callers
//! can wrap it in whatever error type they use.

use std::net::SocketAddr;

/// Shortest accepted shared secret (bearer token, HA token, gossip PSK), in bytes.
pub const MIN_SECRET_LEN: usize = 16;

/// Reject a shared secret shorter than [`MIN_SECRET_LEN`] bytes. `name` is the
/// flag or setting it came from, used in the message.
pub fn check_secret(name: &str, secret: &str) -> Result<(), String> {
    if secret.len() < MIN_SECRET_LEN {
        return Err(format!(
            "{name} is too short ({} bytes, minimum {MIN_SECRET_LEN}); generate one with `openssl rand -hex 32`",
            secret.len()
        ));
    }
    Ok(())
}

/// [`check_secret`] for an optional secret; `None` passes.
pub fn check_optional_secret(name: &str, secret: Option<&str>) -> Result<(), String> {
    secret.map_or(Ok(()), |s| check_secret(name, s))
}

/// Refuse an unauthenticated API on a non-loopback address.
///
/// `protected` is whether a token/password is configured. `allow_insecure` is
/// the operator's explicit `--insecure-no-auth` opt-out, for deployments where
/// the network boundary is the only control; it only logs a warning.
pub fn check_exposure(
    component: &str,
    listen: SocketAddr,
    protected: bool,
    allow_insecure: bool,
) -> Result<(), String> {
    if protected || listen.ip().is_loopback() {
        return Ok(());
    }
    if allow_insecure {
        tracing::warn!(
            %listen, component,
            "API is reachable beyond loopback with no authentication (--insecure-no-auth)"
        );
        return Ok(());
    }
    Err(format!(
        "{component} would listen on {listen} (not loopback) with no authentication; \
         set a token/password, bind a loopback address, or pass --insecure-no-auth \
         if the network boundary is your only control"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn short_secret_rejected_boundary() {
        assert!(check_secret("--auth-token", &"a".repeat(15)).is_err());
        assert!(check_secret("--auth-token", &"a".repeat(16)).is_ok());
        let msg = check_secret("--ha-token", "short").unwrap_err();
        assert!(msg.contains("--ha-token") && msg.contains("16"));
    }

    #[test]
    fn optional_secret_none_passes() {
        assert!(check_optional_secret("x", None).is_ok());
        assert!(check_optional_secret("x", Some("short")).is_err());
    }

    #[test]
    fn exposure_loopback_open_is_fine() {
        assert!(check_exposure("c", addr("127.0.0.1:1"), false, false).is_ok());
        assert!(check_exposure("c", addr("[::1]:1"), false, false).is_ok());
    }

    #[test]
    fn exposure_non_loopback_open_refused() {
        for a in ["0.0.0.0:1", "10.0.0.5:1", "[::]:1"] {
            let e = check_exposure("wayhouse-controller", addr(a), false, false).unwrap_err();
            assert!(
                e.contains("wayhouse-controller") && e.contains("--insecure-no-auth"),
                "{e}"
            );
        }
    }

    #[test]
    fn exposure_non_loopback_with_token_or_optout_ok() {
        assert!(check_exposure("c", addr("0.0.0.0:1"), true, false).is_ok());
        assert!(check_exposure("c", addr("0.0.0.0:1"), false, true).is_ok());
    }
}
