//! Automatic tunnel-address lease expiry (`--tunnel-lease-ttl`,
//! `docs/11` "Address authority"): an owner not re-registered for longer than
//! the TTL loses its registration and its address, exactly as if an operator
//! had `DELETE`d it. Subscribers see the tombstone and drop the WireGuard
//! peer; a pool's `tunnel` source clears the origin's backends.
//!
//! A sweep lists the book and, for each owner whose `last_seen` is older than
//! the cutoff, asks its registry to [`RegistryState::expire`]. That check runs
//! again where the write lands (under the registry lock, or when the Raft
//! entry applies), so a re-registration that commits first wins. Under HA only
//! the leader sweeps.
//!
//! Live owners re-register every few minutes, but under HA `last_seen` is only
//! refreshed once it is older than [`TOUCH_AFTER`], so a live owner can look
//! up to that old. [`MIN_TTL`] keeps the TTL clear of it, and [`STARTUP_GRACE`]
//! keeps a controller (or a new leader) that was down for longer than the TTL
//! from expiring everyone before they had a chance to re-register.

use std::time::Duration;

use crate::addresses::{AddressBook, Role};
use crate::peers::api::PeersState;
use crate::proxy_peers::api::ProxyPeersState;
use crate::registry::{Expiry, TOUCH_AFTER};

/// The shortest `--tunnel-lease-ttl` accepted.
pub const MIN_TTL: Duration = Duration::from_secs(2 * TOUCH_AFTER.as_secs());

/// How long after startup (or after gaining leadership) the sweeper waits
/// before its first sweep.
pub const STARTUP_GRACE: Duration = Duration::from_secs(3600);

/// Checks a parsed `--tunnel-lease-ttl`: zero (off) or at least [`MIN_TTL`].
pub fn check_ttl(ttl: Duration) -> Result<(), String> {
    if ttl.is_zero() || ttl >= MIN_TTL {
        return Ok(());
    }
    Err(format!(
        "--tunnel-lease-ttl must be at least {}h (or 0 to never expire): under HA a live \
         owner's last_seen can lag by up to {}h",
        MIN_TTL.as_secs() / 3600,
        TOUCH_AFTER.as_secs() / 3600
    ))
}

/// How often to sweep for `ttl`: a tenth of it, between a minute and an hour.
pub fn sweep_interval(ttl: Duration) -> Duration {
    (ttl / 10).clamp(Duration::from_secs(60), Duration::from_secs(3600))
}

/// One sweep at `now` (unix seconds): expires every owner unseen for longer
/// than `ttl`. Returns how many were released.
pub async fn sweep(
    book: &AddressBook,
    peers: &PeersState,
    proxy_peers: &ProxyPeersState,
    ttl: Duration,
    now: u64,
) -> usize {
    let cutoff = now.saturating_sub(ttl.as_secs());
    let entries = match book.entries() {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "lease sweep: reading the address book failed");
            return 0;
        }
    };
    let mut released = 0;
    for entry in entries.iter().filter(|e| e.assignment.last_seen < cutoff) {
        let result = match entry.role {
            Role::Origin => peers.expire(&entry.name, cutoff).await,
            Role::Proxy => proxy_peers.expire(&entry.name, cutoff).await,
        };
        match result {
            Ok(Expiry::Released(address)) => {
                released += 1;
                tracing::warn!(
                    role = %entry.role,
                    name = %entry.name,
                    ?address,
                    last_seen = entry.assignment.last_seen,
                    "tunnel address lease expired; released it"
                );
            }
            Ok(Expiry::NotExpired | Expiry::Gone) => {}
            Err(e) => {
                tracing::warn!(role = %entry.role, name = %entry.name, error = %e, "lease expiry failed; retrying next sweep");
            }
        }
    }
    released
}

/// Sweeps every [`sweep_interval`] while `is_leader()` holds, starting
/// [`STARTUP_GRACE`] after the process starts and again after each time this
/// node regains leadership. Does nothing when `ttl` is zero.
pub async fn lease_loop(
    book: std::sync::Arc<AddressBook>,
    peers: PeersState,
    proxy_peers: ProxyPeersState,
    ttl: Duration,
    is_leader: impl Fn() -> bool,
) {
    if ttl.is_zero() {
        return;
    }
    let mut tick = tokio::time::interval(sweep_interval(ttl));
    let mut ready_at = tokio::time::Instant::now() + STARTUP_GRACE;
    let mut was_leader = false;
    loop {
        tick.tick().await;
        let leader = is_leader();
        if leader && !was_leader {
            // A fresh leader may be taking over from a gap in registrations.
            ready_at = ready_at.max(tokio::time::Instant::now() + STARTUP_GRACE);
        }
        was_leader = leader;
        if leader && tokio::time::Instant::now() >= ready_at {
            sweep(
                &book,
                &peers,
                &proxy_peers,
                ttl,
                crate::addresses::unix_secs(),
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::addresses::Network;
    use crate::store::Store;

    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const TTL: Duration = Duration::from_secs(7200);

    struct Fixture {
        book: Arc<AddressBook>,
        peers: PeersState,
        proxy_peers: ProxyPeersState,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = |sub: &str| Arc::new(Store::open(&dir.path().join(sub)).unwrap());
        let book = Arc::new(
            AddressBook::open(
                &dir.path().join("addresses"),
                Some(Network::parse("10.60.0.0/24").unwrap()),
            )
            .unwrap(),
        );
        Fixture {
            peers: PeersState::new(store("peers"), None, book.clone()),
            proxy_peers: ProxyPeersState::new(store("proxy-peers"), None, book.clone()),
            book,
            _dir: dir,
        }
    }

    /// Registers `name` at `at` through the book and the registry, like a
    /// non-HA `POST` does.
    fn register_origin(f: &Fixture, name: &str, at: u64) {
        let a = f.book.claim(Role::Origin, name, None, at).unwrap();
        let reg = crate::peers::PeerRegistration {
            name: name.into(),
            pubkey: KEY.into(),
            endpoint: None,
            backends: vec![],
            tunnel_address: Some(a.address.to_string()),
        };
        f.peers.register_applied(&reg, None).unwrap();
    }

    #[test]
    fn ttl_below_the_minimum_is_refused_but_zero_is_off() {
        assert!(check_ttl(Duration::ZERO).is_ok());
        assert!(check_ttl(MIN_TTL).is_ok());
        assert!(check_ttl(MIN_TTL - Duration::from_secs(1)).is_err());
    }

    #[test]
    fn the_sweep_interval_is_a_tenth_of_the_ttl_within_bounds() {
        assert_eq!(
            sweep_interval(Duration::from_secs(7200)),
            Duration::from_secs(720)
        );
        assert_eq!(
            sweep_interval(Duration::from_secs(100)),
            Duration::from_secs(60)
        );
        assert_eq!(
            sweep_interval(Duration::from_secs(86400 * 30)),
            Duration::from_secs(3600)
        );
    }

    #[tokio::test]
    async fn a_sweep_releases_only_owners_unseen_for_longer_than_the_ttl() {
        let f = fixture();
        register_origin(&f, "old", 1_000);
        register_origin(&f, "fresh", 1_000 + TTL.as_secs());
        f.book
            .claim(Role::Proxy, "stale-proxy", None, 1_000)
            .unwrap();

        let now = 1_000 + TTL.as_secs() + 1;
        let released = sweep(&f.book, &f.peers, &f.proxy_peers, TTL, now).await;
        assert_eq!(released, 2);

        // "old" is gone from the registry (with a tombstone) and the book.
        assert_eq!(f.peers.current_for("old").unwrap(), None);
        assert!(f.book.get(Role::Origin, "old").unwrap().is_none());
        let log = f.peers.store.all_revisions().unwrap();
        let tombstone = &log.last().unwrap().1;
        assert_eq!(
            crate::registry::event_payload(3, tombstone)["removed"]["name"],
            "old"
        );
        // The other two survive...
        assert!(f.peers.current_for("fresh").unwrap().is_some());
        // ...except the proxy that was only in the book.
        assert!(f.book.get(Role::Proxy, "stale-proxy").unwrap().is_none());

        // The address is free for the next owner.
        let next = f.book.claim(Role::Origin, "new", None, now).unwrap();
        assert_eq!(next.address.to_string(), "10.60.0.1");
    }

    #[tokio::test]
    async fn a_re_registration_between_the_listing_and_the_write_wins() {
        let f = fixture();
        register_origin(&f, "home", 1_000);
        let cutoff = 1_000 + 1;
        // The sweep listed "home" as unseen, then it re-registered.
        register_origin(&f, "home", 5_000);
        assert_eq!(
            f.peers.expire("home", cutoff).await.unwrap(),
            Expiry::NotExpired
        );
        assert!(f.peers.current_for("home").unwrap().is_some());
        assert!(f.book.get(Role::Origin, "home").unwrap().is_some());
    }

    #[tokio::test]
    async fn expiring_a_name_that_is_already_gone_is_not_an_error() {
        let f = fixture();
        assert_eq!(f.peers.expire("nobody", 10).await.unwrap(), Expiry::Gone);
    }
}
