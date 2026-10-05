//! The running WireGuard interface and the tunnel address it carries, which
//! the controller can change while the process runs (`docs/11` "Address
//! authority"; mirrored in `wayhouse::live_interface` rather than shared — the two
//! binaries have no common library).
//!
//! A change touches only the address: the device, its key, listen port and
//! peers stay, so established sessions survive. `defguard_wireguard_rs` can add
//! an address but not remove one — `configure_interface` flushes on the kernel
//! backend only, and `remove_interface` cannot be relied on (it fails in
//! `clear_dns` where no resolver tool exists) — so the old address is deleted
//! with a netlink call of our own ([`crate::netlink_addr`]), the same on both
//! backends, and the new one assigned after it. Deleting first matters for
//! IPv4: deleting a primary address also deletes the secondaries sharing its
//! subnet, so an address added first would be swept away with it.

use std::io;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use defguard_wireguard_rs::net::IpAddrMask;
use defguard_wireguard_rs::peer::Peer;
use defguard_wireguard_rs::WireguardInterfaceApi;

pub type Wg = dyn WireguardInterfaceApi + Send + Sync;

/// Removes an address from the interface.
pub type DeleteAddress = Box<dyn Fn(&IpAddrMask) -> io::Result<()> + Send + Sync>;

/// Deletes the interface's link, address and all.
pub type DeleteLink = Box<dyn Fn() -> io::Result<()> + Send + Sync>;

struct State {
    address: IpAddrMask,
    /// Both the new address and the restore of the old one failed in a
    /// [`LiveInterface::readdress`], so the link may carry no address; the
    /// next call repairs it even when asked for the address it records.
    dirty: bool,
}

pub struct LiveInterface {
    wg: Arc<Wg>,
    state: Mutex<State>,
    delete: DeleteAddress,
    delete_link: DeleteLink,
    /// Peers [`LiveInterface::renew_peers`] removed and could not add back,
    /// for [`LiveInterface::repair_peers`].
    lost: Mutex<Vec<Peer>>,
    /// Held across every edit of the device's peer set: a renewal or repair
    /// works from a snapshot, so a subscription event must not land in the
    /// middle of one and then be overtaken by it.
    peer_edit: Mutex<()>,
}

impl LiveInterface {
    pub fn new(
        wg: Arc<Wg>,
        address: IpAddrMask,
        delete: DeleteAddress,
        delete_link: DeleteLink,
    ) -> Self {
        Self {
            wg,
            state: Mutex::new(State {
                address,
                dirty: false,
            }),
            delete,
            delete_link,
            lost: Mutex::new(Vec::new()),
            peer_edit: Mutex::new(()),
        }
    }

    /// Runs `f`, an edit of the device's peers, so that it never interleaves
    /// with [`LiveInterface::renew_peers`] or [`LiveInterface::repair_peers`].
    /// Not reentrant: `f` must not call either of them.
    pub fn edit_peers<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self
            .peer_edit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f()
    }

    /// The interface itself, for the peer subscriptions.
    pub fn api(&self) -> Arc<Wg> {
        self.wg.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The address the interface currently carries.
    pub fn address(&self) -> IpAddrMask {
        self.lock().address.clone()
    }

    /// Whether the interface needs [`LiveInterface::readdress`] to carry `new`.
    pub fn needs_readdress(&self, new: &IpAddrMask) -> bool {
        let st = self.lock();
        st.address != *new || st.dirty
    }

    /// Removes the interface (shutdown). When the library cannot (on the
    /// kernel backend it fails in `clear_dns` before it deletes the link), the
    /// link is deleted over netlink instead, so neither it nor its address
    /// outlives the process.
    pub fn remove(&self) -> anyhow::Result<()> {
        match self.wg.remove_interface() {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "removing the interface failed; deleting the link instead");
                (self.delete_link)().context("deleting the interface's link")
            }
        }
    }

    fn lost(&self) -> std::sync::MutexGuard<'_, Vec<Peer>> {
        self.lost
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Re-creates every peer as the device reports it and returns the tunnel
    /// addresses of those that came back, to be kicked. The other side drops
    /// and re-adds its peer for us when our address changes, which discards
    /// the session this side still believes in; its packets are then ignored
    /// until a keepalive times out (longer than a proxy waits for a backend).
    /// Re-creating the peer resets the session, so a new handshake starts at
    /// once.
    ///
    /// Best effort per peer: one that cannot be added back (retried once) is
    /// remembered for [`LiveInterface::repair_peers`] and does not stop the
    /// others.
    pub fn renew_peers(&self) -> anyhow::Result<Vec<std::net::IpAddr>> {
        self.edit_peers(|| self.renew_peers_locked())
    }

    fn renew_peers_locked(&self) -> anyhow::Result<Vec<std::net::IpAddr>> {
        let host = self.wg.read_interface_data()?;
        let mut targets = Vec::new();
        for peer in host.peers.values() {
            if let Err(e) = self.wg.remove_peer(&peer.public_key) {
                tracing::debug!(error = %e, "removing a peer to renew its session");
            }
            let added = self.wg.configure_peer(peer).or_else(|first| {
                tracing::debug!(error = %first, "re-adding a peer failed; retrying once");
                self.wg.configure_peer(peer)
            });
            match added {
                Ok(()) => targets.extend(peer.allowed_ips.iter().map(|a| a.address)),
                Err(e) => {
                    tracing::warn!(error = %e, peer = %peer.public_key, "could not re-add a peer after the address change; will retry");
                    let mut lost = self.lost();
                    lost.retain(|p| p.public_key != peer.public_key);
                    lost.push(peer.clone());
                }
            }
        }
        Ok(targets)
    }

    /// Drops `key` from the peers awaiting repair: the registration it came
    /// from was removed or replaced, so re-adding it would resurrect a proxy
    /// that no longer exists.
    pub fn forget_peer(&self, key: &defguard_wireguard_rs::key::Key) {
        self.lost().retain(|p| &p.public_key != key);
    }

    /// Adds back the peers [`LiveInterface::renew_peers`] lost and returns
    /// their tunnel addresses, to be kicked. A peer that has reappeared on the
    /// device since (the subscription re-added it from a newer registration)
    /// is left as it is.
    pub fn repair_peers(&self) -> Vec<std::net::IpAddr> {
        self.edit_peers(|| self.repair_peers_locked())
    }

    fn repair_peers_locked(&self) -> Vec<std::net::IpAddr> {
        let mut lost = self.lost();
        if lost.is_empty() {
            return Vec::new();
        }
        let present = match self.wg.read_interface_data() {
            Ok(host) => host.peers,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the interface to repair lost peers");
                return Vec::new();
            }
        };
        let mut targets = Vec::new();
        lost.retain(|peer| {
            if present.contains_key(&peer.public_key) {
                return false;
            }
            match self.wg.configure_peer(peer) {
                Ok(()) => {
                    targets.extend(peer.allowed_ips.iter().map(|a| a.address));
                    false
                }
                Err(e) => {
                    tracing::warn!(error = %e, peer = %peer.public_key, "could not re-add a lost peer; will retry");
                    true
                }
            }
        });
        targets
    }

    /// Moves the interface to `new`, keeping its peers. When the old address
    /// cannot be deleted nothing has changed; when the new one cannot be
    /// assigned the old one is put back if it can be, and either way the
    /// recorded address stays the old one, so the next call tries again.
    pub fn readdress(&self, new: &IpAddrMask) -> anyhow::Result<()> {
        let mut st = self.lock();
        if st.address == *new && !st.dirty {
            return Ok(());
        }
        (self.delete)(&st.address)
            .with_context(|| format!("removing the address {}", st.address))?;
        if let Err(e) = self.wg.assign_address(new) {
            st.dirty = match self.wg.assign_address(&st.address) {
                Ok(()) => false,
                Err(restore) => {
                    tracing::error!(error = %restore, address = %st.address, "could not put the old tunnel address back");
                    true
                }
            };
            return Err(anyhow::Error::new(e).context(format!("assigning the address {new}")));
        }
        st.address = new.clone();
        st.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Mutex;

    use defguard_wireguard_rs::dns::DnsConfig;
    use defguard_wireguard_rs::error::WireguardInterfaceError;
    use defguard_wireguard_rs::host::Host;
    use defguard_wireguard_rs::key::Key;
    use defguard_wireguard_rs::peer::Peer;
    use defguard_wireguard_rs::InterfaceConfiguration;

    use super::*;

    pub type Log = Arc<Mutex<Vec<String>>>;

    /// A device that records the addresses assigned to it.
    pub struct Fake {
        pub log: Log,
        pub fail_assign: Vec<String>,
        /// Whether `remove_interface` fails (as it does without a resolver tool).
        pub fail_remove: bool,
        /// The peers the device holds; `remove_peer` and `configure_peer` edit it.
        pub peers: Mutex<Vec<Peer>>,
        /// Remaining failures of `configure_peer` per peer key.
        pub fail_configure: Mutex<std::collections::HashMap<String, usize>>,
        /// Run once, right after the next `read_interface_data` took its
        /// snapshot: lets a test deliver an event while a renewal is in flight.
        pub after_read: Hook,
    }

    pub type Hook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

    impl WireguardInterfaceApi for Fake {
        fn create_interface(&mut self) -> Result<(), WireguardInterfaceError> {
            Ok(())
        }
        fn assign_address(&self, address: &IpAddrMask) -> Result<(), WireguardInterfaceError> {
            if self.fail_assign.contains(&address.to_string()) {
                return Err(WireguardInterfaceError::PeerConfigurationError(
                    "refused".into(),
                ));
            }
            self.log.lock().unwrap().push(format!("assign {address}"));
            Ok(())
        }
        fn configure_peer_routing(&self, _: &[Peer]) -> Result<(), WireguardInterfaceError> {
            Ok(())
        }
        fn configure_interface(
            &self,
            _: &InterfaceConfiguration,
        ) -> Result<(), WireguardInterfaceError> {
            Ok(())
        }
        fn remove_interface(&self) -> Result<(), WireguardInterfaceError> {
            if self.fail_remove {
                return Err(WireguardInterfaceError::PeerConfigurationError(
                    "Command returned error status".into(),
                ));
            }
            self.log.lock().unwrap().push("remove".into());
            Ok(())
        }
        fn configure_peer(&self, peer: &Peer) -> Result<(), WireguardInterfaceError> {
            if let Some(n) = self
                .fail_configure
                .lock()
                .unwrap()
                .get_mut(&peer.public_key.to_string())
                .filter(|n| **n > 0)
            {
                *n -= 1;
                return Err(WireguardInterfaceError::PeerConfigurationError(
                    "refused".into(),
                ));
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("configure peer {}", peer.public_key));
            let mut peers = self.peers.lock().unwrap();
            peers.retain(|p| p.public_key != peer.public_key);
            peers.push(peer.clone());
            Ok(())
        }
        fn remove_peer(&self, key: &Key) -> Result<(), WireguardInterfaceError> {
            self.log.lock().unwrap().push(format!("remove peer {key}"));
            self.peers.lock().unwrap().retain(|p| &p.public_key != key);
            Ok(())
        }
        fn read_interface_data(&self) -> Result<Host, WireguardInterfaceError> {
            let mut host = Host::default();
            for p in self.peers.lock().unwrap().iter() {
                host.peers.insert(p.public_key.clone(), p.clone());
            }
            let hook = self.after_read.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
            Ok(host)
        }
        fn set_dns(&self, _: &DnsConfig<'_>) -> Result<(), WireguardInterfaceError> {
            Ok(())
        }
    }

    pub fn mask(s: &str) -> IpAddrMask {
        s.parse().unwrap()
    }

    /// A `LiveInterface` on `10.60.0.2/24`. Assigning an address in
    /// `fail_assign` fails, deleting one in `fail_delete` fails; both are
    /// logged otherwise.
    pub fn live(fail_assign: &[&str], fail_delete: &[&str], log: &Log) -> LiveInterface {
        live_with_peers(fail_assign, fail_delete, &[], log)
    }

    /// [`live`] on a device that reports `peers`.
    pub fn live_with_peers(
        fail_assign: &[&str],
        fail_delete: &[&str],
        peers: &[Peer],
        log: &Log,
    ) -> LiveInterface {
        build(
            fail_assign,
            fail_delete,
            peers,
            &[],
            false,
            log,
            Hook::default(),
        )
    }

    /// [`live_with_peers`] where `configure_peer` fails the given number of
    /// times for each listed key before it succeeds.
    pub fn live_with_flaky_peers(
        peers: &[Peer],
        failures: &[(&Key, usize)],
        log: &Log,
    ) -> LiveInterface {
        build(&[], &[], peers, failures, false, log, Hook::default())
    }

    /// [`live_with_peers`] with `hook` run after the first read of the device.
    pub fn live_with_read_hook(peers: &[Peer], hook: &Hook, log: &Log) -> LiveInterface {
        build(&[], &[], peers, &[], false, log, hook.clone())
    }

    /// A [`live`] whose `remove_interface` fails; deleting the link is logged
    /// as `delete link`.
    pub fn live_failing_remove(log: &Log) -> LiveInterface {
        build(&[], &[], &[], &[], true, log, Hook::default())
    }

    fn build(
        fail_assign: &[&str],
        fail_delete: &[&str],
        peers: &[Peer],
        failures: &[(&Key, usize)],
        fail_remove: bool,
        log: &Log,
        after_read: Hook,
    ) -> LiveInterface {
        let fail_delete: Vec<String> = fail_delete.iter().map(ToString::to_string).collect();
        let log2 = log.clone();
        let log3 = log.clone();
        LiveInterface::new(
            Arc::new(Fake {
                log: log.clone(),
                fail_assign: fail_assign.iter().map(ToString::to_string).collect(),
                fail_remove,
                peers: Mutex::new(peers.to_vec()),
                fail_configure: Mutex::new(
                    failures.iter().map(|(k, n)| (k.to_string(), *n)).collect(),
                ),
                after_read,
            }),
            mask("10.60.0.2/24"),
            Box::new(move |a| {
                if fail_delete.contains(&a.to_string()) {
                    return Err(io::Error::other("refused"));
                }
                log2.lock().unwrap().push(format!("delete {a}"));
                Ok(())
            }),
            Box::new(move || {
                log3.lock().unwrap().push("delete link".into());
                Ok(())
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;

    #[test]
    fn readdress_deletes_the_old_address_then_assigns_the_new_one() {
        let log = Log::default();
        let live = live(&[], &[], &log);
        live.readdress(&mask("10.60.0.7/24")).unwrap();
        assert_eq!(live.address(), mask("10.60.0.7/24"));
        assert_eq!(
            *log.lock().unwrap(),
            vec!["delete 10.60.0.2/24", "assign 10.60.0.7/24"]
        );
    }

    #[test]
    fn readdress_to_the_current_address_does_nothing() {
        let log = Log::default();
        let live = live(&[], &[], &log);
        assert!(!live.needs_readdress(&mask("10.60.0.2/24")));
        live.readdress(&mask("10.60.0.2/24")).unwrap();
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_delete_changes_nothing_and_is_retried() {
        let log = Log::default();
        let live = live(&[], &["10.60.0.2/24"], &log);
        assert!(live.readdress(&mask("10.60.0.7/24")).is_err());
        assert_eq!(live.address(), mask("10.60.0.2/24"));
        assert!(live.needs_readdress(&mask("10.60.0.7/24")));
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_assign_puts_the_old_address_back_and_keeps_it_recorded() {
        let log = Log::default();
        let live = live(&["10.60.0.7/24"], &[], &log);
        let err = live.readdress(&mask("10.60.0.7/24")).unwrap_err();
        assert!(
            format!("{err:#}").contains("assigning the address"),
            "{err:#}"
        );
        assert_eq!(live.address(), mask("10.60.0.2/24"));
        assert_eq!(
            *log.lock().unwrap(),
            vec!["delete 10.60.0.2/24", "assign 10.60.0.2/24"]
        );
    }

    #[test]
    fn if_the_restore_fails_too_the_next_call_repairs_even_the_recorded_address() {
        let log = Log::default();
        let live = live(&["10.60.0.7/24", "10.60.0.2/24"], &[], &log);
        assert!(live.readdress(&mask("10.60.0.7/24")).is_err());
        assert!(live.needs_readdress(&mask("10.60.0.2/24")));
        let before = log.lock().unwrap().len();
        assert!(live.readdress(&mask("10.60.0.2/24")).is_err());
        assert_eq!(
            log.lock().unwrap().len(),
            before + 1,
            "the recorded address is deleted and assigned again, not skipped"
        );
    }

    #[test]
    fn renew_peers_recreates_each_peer_and_returns_its_address() {
        use defguard_wireguard_rs::key::Key;
        use defguard_wireguard_rs::peer::Peer;
        let key = Key::new([7; 32]);
        let mut peer = Peer::new(key.clone());
        peer.allowed_ips.push(mask("fd00::2/128"));
        let log = Log::default();
        let live = live_with_peers(&[], &[], &[peer], &log);
        let targets = live.renew_peers().unwrap();
        assert_eq!(
            targets,
            vec!["fd00::2".parse::<std::net::IpAddr>().unwrap()]
        );
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                format!("remove peer {key}"),
                format!("configure peer {key}")
            ]
        );
    }

    fn peer(n: u8, ip: &str) -> defguard_wireguard_rs::peer::Peer {
        let mut p =
            defguard_wireguard_rs::peer::Peer::new(defguard_wireguard_rs::key::Key::new([n; 32]));
        p.allowed_ips.push(mask(ip));
        p
    }

    #[test]
    fn remove_uses_the_library_when_it_works() {
        let log = Log::default();
        live(&[], &[], &log).remove().unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["remove"]);
    }

    #[test]
    fn remove_deletes_the_link_itself_when_the_library_fails() {
        let log = Log::default();
        live_failing_remove(&log).remove().unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["delete link"]);
    }

    #[test]
    fn a_failed_readd_is_retried_once_and_the_other_peers_are_still_renewed() {
        let (a, b) = (peer(1, "fd00::1/128"), peer(2, "fd00::2/128"));
        let log = Log::default();
        let live = live_with_flaky_peers(&[a.clone(), b.clone()], &[(&a.public_key, 1)], &log);
        let mut targets = live.renew_peers().unwrap();
        targets.sort();
        assert_eq!(targets.len(), 2, "both peers renewed: {targets:?}");
        let log = log.lock().unwrap();
        assert!(log.contains(&format!("configure peer {}", a.public_key)));
        assert!(log.contains(&format!("configure peer {}", b.public_key)));
    }

    #[test]
    fn a_peer_that_cannot_be_re_added_is_repaired_on_a_later_pass() {
        let (a, b) = (peer(1, "fd00::1/128"), peer(2, "fd00::2/128"));
        let log = Log::default();
        let live = live_with_flaky_peers(&[a.clone(), b.clone()], &[(&a.public_key, 2)], &log);
        let targets = live.renew_peers().unwrap();
        assert_eq!(
            targets,
            vec!["fd00::2".parse::<std::net::IpAddr>().unwrap()],
            "only the peer that came back is kicked"
        );
        let device = live.api().read_interface_data().unwrap();
        assert!(!device.peers.contains_key(&a.public_key), "a is lost");
        assert!(
            device.peers.contains_key(&b.public_key),
            "b was not skipped"
        );

        let repaired = live.repair_peers();
        assert_eq!(
            repaired,
            vec!["fd00::1".parse::<std::net::IpAddr>().unwrap()]
        );
        assert!(live
            .api()
            .read_interface_data()
            .unwrap()
            .peers
            .contains_key(&a.public_key));
        assert!(live.repair_peers().is_empty(), "nothing is left to repair");
    }

    #[test]
    fn a_forgotten_peer_is_not_repaired() {
        let a = peer(1, "fd00::1/128");
        let log = Log::default();
        let live = live_with_flaky_peers(std::slice::from_ref(&a), &[(&a.public_key, 2)], &log);
        live.renew_peers().unwrap();
        live.forget_peer(&a.public_key);
        assert!(live.repair_peers().is_empty());
        assert!(!live
            .api()
            .read_interface_data()
            .unwrap()
            .peers
            .contains_key(&a.public_key));
    }

    #[test]
    fn repair_does_not_overwrite_a_peer_that_was_re_added_meanwhile() {
        let a = peer(1, "fd00::1/128");
        let log = Log::default();
        let live = live_with_flaky_peers(std::slice::from_ref(&a), &[(&a.public_key, 2)], &log);
        live.renew_peers().unwrap();
        let newer = peer(1, "fd00::99/128");
        live.api().configure_peer(&newer).unwrap();
        assert!(live.repair_peers().is_empty());
        let device = live.api().read_interface_data().unwrap();
        assert_eq!(
            device.peers[&a.public_key].allowed_ips, newer.allowed_ips,
            "the newer configuration stays"
        );
    }
}
