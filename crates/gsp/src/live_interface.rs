//! The running WireGuard interface and the tunnel address it carries, which
//! the controller can change while the process runs (`docs/11` "Address
//! authority"; mirrored in `gsp::live_interface` rather than shared — the two
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
        }
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
    }

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
        fn configure_peer(&self, _: &Peer) -> Result<(), WireguardInterfaceError> {
            Ok(())
        }
        fn remove_peer(&self, _: &Key) -> Result<(), WireguardInterfaceError> {
            Ok(())
        }
        fn read_interface_data(&self) -> Result<Host, WireguardInterfaceError> {
            Ok(Host::default())
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
        build(fail_assign, fail_delete, false, log)
    }

    /// A [`live`] whose `remove_interface` fails; deleting the link is logged
    /// as `delete link`.
    pub fn live_failing_remove(log: &Log) -> LiveInterface {
        build(&[], &[], true, log)
    }

    fn build(
        fail_assign: &[&str],
        fail_delete: &[&str],
        fail_remove: bool,
        log: &Log,
    ) -> LiveInterface {
        let fail_delete: Vec<String> = fail_delete.iter().map(ToString::to_string).collect();
        let log2 = log.clone();
        let log3 = log.clone();
        LiveInterface::new(
            Arc::new(Fake {
                log: log.clone(),
                fail_assign: fail_assign.iter().map(ToString::to_string).collect(),
                fail_remove,
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
}
