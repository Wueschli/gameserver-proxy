//! The running WireGuard interface, swappable in place when the controller
//! assigns this origin a different tunnel address (`docs/11` "Address
//! authority"; mirrored in `gsp::live_interface` rather than shared — the two
//! binaries have no common library).
//!
//! Neither backend can *replace* an interface address: `defguard_wireguard_rs`
//! has no remove-address call, the kernel backend's `configure_interface`
//! flushes every address but the boringtun one does not. So a change tears the
//! interface down and brings it back up on the new address with the peers it
//! had, which works the same on both backends. The key, listen port and peer
//! set carry over, so the only visible effect is a short gap while WireGuard
//! re-handshakes.
//!
//! Every user of the interface goes through [`LiveInterface::with`], which
//! holds a read lock; [`LiveInterface::readdress`] holds the write lock for the
//! whole swap, so a peer update can never land on the interface being torn
//! down and get lost.

use std::sync::RwLock;
use std::time::Duration;

use anyhow::Context;
use defguard_wireguard_rs::host::Host;
use defguard_wireguard_rs::net::IpAddrMask;
use defguard_wireguard_rs::peer::Peer;
use defguard_wireguard_rs::WireguardInterfaceApi;

pub type Wg = dyn WireguardInterfaceApi + Send + Sync;

/// Brings up a fresh interface on `address` with `peers` already configured.
pub type BringUp = Box<dyn Fn(IpAddrMask, Vec<Peer>) -> anyhow::Result<Box<Wg>> + Send + Sync>;

struct State {
    /// `None` only after a swap failed *and* the rollback failed too: the
    /// interface is gone and `peers` is what the next attempt restores.
    wg: Option<Box<Wg>>,
    address: IpAddrMask,
    peers: Vec<Peer>,
}

pub struct LiveInterface {
    state: RwLock<State>,
    bring_up: BringUp,
    /// How often a bring-up is tried, [`RETRY_DELAY`] apart. See
    /// [`LiveInterface::bring_up_retrying`].
    attempts: u32,
}

const RETRY_DELAY: Duration = Duration::from_millis(200);

/// The configuration of a peer, without the runtime counters (handshake time,
/// byte counts) the device reports alongside it.
fn carry_peers(host: &Host) -> Vec<Peer> {
    host.peers
        .values()
        .map(|p| {
            let mut peer = Peer::new(p.public_key.clone());
            peer.preshared_key = p.preshared_key.clone();
            peer.endpoint = p.endpoint;
            peer.persistent_keepalive_interval = p.persistent_keepalive_interval;
            peer.allowed_ips = p.allowed_ips.clone();
            peer
        })
        .collect()
}

impl LiveInterface {
    pub fn new(wg: Box<Wg>, address: IpAddrMask, bring_up: BringUp) -> Self {
        Self {
            state: RwLock::new(State {
                wg: Some(wg),
                address,
                peers: Vec::new(),
            }),
            bring_up,
            attempts: 25,
        }
    }

    /// Overrides the number of bring-up attempts (tests use 1 to fail fast).
    #[cfg(test)]
    pub fn attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts;
        self
    }

    /// The userspace backend releases a removed device's tun name and UDP port
    /// from its own threads, a moment after `remove_interface` returns, so the
    /// first bring-up under the same name can fail with "busy"; retry briefly.
    fn bring_up_retrying(&self, address: &IpAddrMask, peers: &[Peer]) -> anyhow::Result<Box<Wg>> {
        let mut attempt = 1;
        loop {
            match (self.bring_up)(address.clone(), peers.to_vec()) {
                Ok(wg) => return Ok(wg),
                Err(e) if attempt >= self.attempts => return Err(e),
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), attempt, "interface bring-up failed; retrying");
                    std::thread::sleep(RETRY_DELAY);
                    attempt += 1;
                }
            }
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` against the interface; `None` when it is down (a failed
    /// readdress whose rollback also failed).
    pub fn with<R>(&self, f: impl FnOnce(&Wg) -> R) -> Option<R> {
        self.read().wg.as_deref().map(f)
    }

    /// The address the interface currently carries.
    pub fn address(&self) -> IpAddrMask {
        self.read().address.clone()
    }

    /// Removes the interface (shutdown).
    pub fn remove(&self) -> anyhow::Result<()> {
        match self.read().wg.as_deref() {
            Some(wg) => Ok(wg.remove_interface()?),
            None => Ok(()),
        }
    }

    /// Moves the interface to `new`, keeping its peers. On failure the old
    /// address is restored; if even that fails the interface stays down and
    /// the next call retries from the peers saved here.
    pub fn readdress(&self, new: IpAddrMask) -> anyhow::Result<()> {
        let mut st = self.write();
        if st.address == new && st.wg.is_some() {
            return Ok(());
        }
        if let Some(old) = st.wg.take() {
            let peers = match old.read_interface_data() {
                Ok(host) => carry_peers(&host),
                Err(e) => {
                    st.wg = Some(old);
                    return Err(anyhow::Error::new(e)
                        .context("reading the current peers before changing the address"));
                }
            };
            if let Err(e) = old.remove_interface() {
                st.wg = Some(old);
                return Err(anyhow::Error::new(e).context("removing the old interface"));
            }
            // The device must be released before its name and port are reused.
            drop(old);
            st.peers = peers;
        }
        match self.bring_up_retrying(&new, &st.peers) {
            Ok(wg) => {
                st.wg = Some(wg);
                st.address = new.clone();
                Ok(())
            }
            Err(e) => {
                let old_address = st.address.clone();
                match self.bring_up_retrying(&old_address, &st.peers) {
                    Ok(wg) => {
                        st.wg = Some(wg);
                        Err(e.context(format!("kept the old address {old_address}")))
                    }
                    Err(rollback) => Err(e.context(format!(
                        "and restoring the old address {old_address} failed too ({rollback:#}); \
                         the interface is down until the next attempt"
                    ))),
                }
            }
        }
        .context(format!("changing the tunnel address to {new}"))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use defguard_wireguard_rs::dns::DnsConfig;
    use defguard_wireguard_rs::error::WireguardInterfaceError;
    use defguard_wireguard_rs::key::Key;
    use defguard_wireguard_rs::InterfaceConfiguration;

    use super::*;

    pub type Log = Arc<Mutex<Vec<String>>>;

    /// A device that records what is done to it.
    pub struct Fake {
        pub name: String,
        pub log: Log,
        pub peers: Vec<Peer>,
        pub fail_remove: bool,
    }

    impl WireguardInterfaceApi for Fake {
        fn create_interface(&mut self) -> Result<(), WireguardInterfaceError> {
            Ok(())
        }
        fn assign_address(&self, _: &IpAddrMask) -> Result<(), WireguardInterfaceError> {
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
                    "stuck".into(),
                ));
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("remove {}", self.name));
            Ok(())
        }
        fn configure_peer(&self, _: &Peer) -> Result<(), WireguardInterfaceError> {
            Ok(())
        }
        fn remove_peer(&self, _: &Key) -> Result<(), WireguardInterfaceError> {
            Ok(())
        }
        fn read_interface_data(&self) -> Result<Host, WireguardInterfaceError> {
            Ok(Host {
                peers: self
                    .peers
                    .iter()
                    .map(|p| (p.public_key.clone(), p.clone()))
                    .collect::<HashMap<_, _>>(),
                ..Host::default()
            })
        }
        fn set_dns(&self, _: &DnsConfig<'_>) -> Result<(), WireguardInterfaceError> {
            Ok(())
        }
    }

    pub fn mask(s: &str) -> IpAddrMask {
        s.parse().unwrap()
    }

    pub fn peer() -> Peer {
        let mut p = Peer::new(Key::generate());
        p.allowed_ips = vec![mask("10.60.0.9/32")];
        p.endpoint = Some("203.0.113.7:51820".parse().unwrap());
        p.persistent_keepalive_interval = Some(25);
        p.tx_bytes = 12345; // runtime counter, must not be carried
        p
    }

    /// A `LiveInterface` on `10.60.0.2/24` holding `peer`, whose `bring_up`
    /// fails for each address in `broken` and otherwise records the call.
    pub fn live(peer: &Peer, broken: &'static [&'static str], log: &Log) -> LiveInterface {
        let log2 = log.clone();
        let bring_up: BringUp = Box::new(move |addr, peers| {
            if broken.contains(&addr.to_string().as_str()) {
                anyhow::bail!("cannot bring up on {addr}");
            }
            log2.lock()
                .unwrap()
                .push(format!("up {addr} peers={}", peers.len()));
            Ok(Box::new(Fake {
                name: addr.to_string(),
                log: log2.clone(),
                peers,
                fail_remove: false,
            }))
        });
        let first = Fake {
            name: "10.60.0.2/24".into(),
            log: log.clone(),
            peers: vec![peer.clone()],
            fail_remove: false,
        };
        LiveInterface::new(Box::new(first), mask("10.60.0.2/24"), bring_up).attempts(1)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn carry_peers_keeps_the_configuration_and_drops_the_counters() {
        let p = peer();
        let mut host = Host::default();
        host.peers.insert(p.public_key.clone(), p.clone());
        let carried = carry_peers(&host);
        assert_eq!(carried.len(), 1);
        assert_eq!(carried[0].public_key, p.public_key);
        assert_eq!(carried[0].allowed_ips, p.allowed_ips);
        assert_eq!(carried[0].endpoint, p.endpoint);
        assert_eq!(carried[0].persistent_keepalive_interval, Some(25));
        assert_eq!(carried[0].tx_bytes, 0);
    }

    #[test]
    fn readdress_removes_the_old_interface_then_brings_up_the_new_one_with_its_peers() {
        let log = Log::default();
        let p = peer();
        let live = live(&p, &[], &log);
        live.readdress(mask("10.60.0.7/24")).unwrap();
        assert_eq!(live.address(), mask("10.60.0.7/24"));
        assert_eq!(
            *log.lock().unwrap(),
            vec!["remove 10.60.0.2/24", "up 10.60.0.7/24 peers=1"]
        );
        // The new device really holds the carried peer.
        let carried = live
            .with(|wg| wg.read_interface_data().unwrap().peers.len())
            .unwrap();
        assert_eq!(carried, 1);
    }

    #[test]
    fn readdress_to_the_current_address_does_nothing() {
        let log = Log::default();
        let live = live(&peer(), &[], &log);
        live.readdress(mask("10.60.0.2/24")).unwrap();
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_bring_up_restores_the_old_address() {
        let log = Log::default();
        let live = live(&peer(), &["10.60.0.7/24"], &log);
        let err = live.readdress(mask("10.60.0.7/24")).unwrap_err();
        assert!(
            format!("{err:#}").contains("kept the old address"),
            "{err:#}"
        );
        assert_eq!(live.address(), mask("10.60.0.2/24"));
        assert_eq!(
            *log.lock().unwrap(),
            vec!["remove 10.60.0.2/24", "up 10.60.0.2/24 peers=1"]
        );
        assert!(live.with(|_| ()).is_some());
    }

    #[test]
    fn if_the_rollback_fails_too_the_next_attempt_restores_from_the_saved_peers() {
        let log = Log::default();
        let live = live(&peer(), &["10.60.0.7/24", "10.60.0.2/24"], &log);
        assert!(live.readdress(mask("10.60.0.7/24")).is_err());
        assert!(live.with(|_| ()).is_none(), "the interface is down");
        // A later attempt on a working address brings it back with the peer.
        live.readdress(mask("10.60.0.8/24")).unwrap();
        assert_eq!(live.address(), mask("10.60.0.8/24"));
        assert_eq!(
            log.lock().unwrap().last().unwrap(),
            "up 10.60.0.8/24 peers=1"
        );
    }

    #[test]
    fn a_device_that_cannot_be_removed_is_left_alone() {
        let log = Log::default();
        let stuck = Fake {
            name: "stuck".into(),
            log: log.clone(),
            peers: vec![],
            fail_remove: true,
        };
        let bring_up: BringUp = Box::new(|_, _| unreachable!("must not bring up a second device"));
        let live = LiveInterface::new(Box::new(stuck), mask("10.60.0.2/24"), bring_up);
        assert!(live.readdress(mask("10.60.0.7/24")).is_err());
        assert_eq!(live.address(), mask("10.60.0.2/24"));
        assert!(live.with(|_| ()).is_some());
    }
}
