//! Phase 14 tunnel address authority (`docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`):
//! `gsp-controller` allocates the tunnel-internal addresses that origins and
//! proxies used to pick (and self-report) by hand, and enforces that no two
//! peers ever hold the same one.
//!
//! One [`AddressBook`] is shared by the backend-peers and proxy-peers
//! registries, because origins and proxies share a single tunnel network.
//! Names are unique per [`Role`]; **addresses are unique globally**. Every
//! claim or release is one short critical section plus one `sled`
//! transaction across both trees, so a crash can never leave `by_owner` and
//! `by_address` disagreeing. A lock-based allocator is only correct with a
//! single writer, which is why `--tunnel-network` is refused together with
//! `--ha-peers` (see the spec's Future work).
//!
//! Time is passed in (`now`, unix seconds) rather than read here, so
//! staleness is testable without sleeping.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sled::transaction::Transactional;

/// Which registry an owner registered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Origin,
    Proxy,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Origin => "origin",
            Role::Proxy => "proxy",
        }
    }

    fn parse(s: &str) -> Option<Role> {
        match s {
            "origin" => Some(Role::Origin),
            "proxy" => Some(Role::Proxy),
            _ => None,
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The IPv4 network tunnel addresses are allocated from (`--tunnel-network`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    network: u32,
    prefix: u8,
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

impl Network {
    /// Parses `a.b.c.d/prefix`. IPv4 only; the prefix must leave at least two
    /// usable host addresses (`/30` or shorter). Host bits are masked off.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (addr, prefix) = s
            .split_once('/')
            .ok_or_else(|| format!("{s:?} is not in ip/prefix form (e.g. 10.60.0.0/16)"))?;
        if addr.contains(':') {
            return Err("IPv6 tunnel networks are not supported yet".into());
        }
        let ip: Ipv4Addr = addr
            .parse()
            .map_err(|e| format!("{addr:?} is not an IPv4 address: {e}"))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| format!("{prefix:?} is not a prefix length"))?;
        if prefix > 30 {
            return Err(format!(
                "prefix /{prefix} is too long: the network needs at least two usable host \
                 addresses (use /30 or shorter)"
            ));
        }
        Ok(Network {
            network: u32::from(ip) & mask(prefix),
            prefix,
        })
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    fn broadcast(&self) -> u32 {
        self.network | !mask(self.prefix)
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & mask(self.prefix) == self.network
    }

    /// Inside the network and neither its network nor broadcast address.
    pub fn is_host(&self, ip: Ipv4Addr) -> bool {
        self.contains(ip) && u32::from(ip) != self.network && u32::from(ip) != self.broadcast()
    }

    /// How many host addresses the network holds.
    pub fn capacity(&self) -> u64 {
        (1u64 << (32 - u32::from(self.prefix))) - 2
    }

    /// The `n`th host address, `n` in `1..=capacity()`.
    fn host(&self, n: u64) -> Ipv4Addr {
        Ipv4Addr::from(self.network + n as u32)
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", Ipv4Addr::from(self.network), self.prefix)
    }
}

/// One owner's address and when it was first/last seen (unix seconds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub address: Ipv4Addr,
    pub first_seen: u64,
    pub last_seen: u64,
}

/// An owner and its assignment, as listed by [`AddressBook::entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub role: Role,
    pub name: String,
    pub assignment: Assignment,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClaimError {
    #[error("address {address} is already held by {role} {name:?}")]
    Held {
        address: Ipv4Addr,
        role: Role,
        name: String,
    },
    #[error(
        "{role} {name:?} already has tunnel address {have}; release it first \
         (DELETE its registration) to use {requested}"
    )]
    OwnerHasDifferent {
        role: Role,
        name: String,
        have: Ipv4Addr,
        requested: Ipv4Addr,
    },
    #[error("address {address} is outside the tunnel network {network}")]
    OutsideNetwork { address: Ipv4Addr, network: Network },
    #[error("address {0} is not a usable tunnel host address")]
    NotHost(Ipv4Addr),
    #[error(
        "this controller has no --tunnel-network, so it cannot allocate an address: \
         request one explicitly"
    )]
    NoNetwork,
    #[error("tunnel network {network} is exhausted ({allocated}/{capacity} addresses allocated)")]
    Exhausted {
        network: Network,
        allocated: usize,
        capacity: u64,
    },
    #[error("tunnel address storage error: {0}")]
    Storage(String),
}

fn storage<E: std::fmt::Display>(e: E) -> ClaimError {
    ClaimError::Storage(e.to_string())
}

fn owner_key(role: Role, name: &str) -> Vec<u8> {
    format!("{role}/{name}").into_bytes()
}

fn parse_owner_key(key: &[u8]) -> Option<(Role, String)> {
    let s = std::str::from_utf8(key).ok()?;
    let (role, name) = s.split_once('/')?;
    Some((Role::parse(role)?, name.to_string()))
}

pub struct AddressBook {
    network: Option<Network>,
    by_owner: sled::Tree,
    by_address: sled::Tree,
    write: Mutex<()>,
    // Held so the database stays open for the trees' lifetime.
    _db: sled::Db,
}

impl AddressBook {
    /// Opens (creating if missing) the book under `dir`. `network` of `None`
    /// is pin-only mode: uniqueness is enforced but nothing is allocated.
    pub fn open(dir: &Path, network: Option<Network>) -> Result<Self, sled::Error> {
        let db = sled::open(dir)?;
        let by_owner = db.open_tree("by_owner")?;
        let by_address = db.open_tree("by_address")?;
        Ok(AddressBook {
            network,
            by_owner,
            by_address,
            write: Mutex::new(()),
            _db: db,
        })
    }

    pub fn network(&self) -> Option<Network> {
        self.network
    }

    /// Registers `(role, name)`'s address and returns it, applying the
    /// allocation and pinning rules of the spec: an existing owner keeps its
    /// address (idempotent re-registration), a new owner without a request
    /// gets the lowest free host address, a requested address is granted only
    /// if free and inside the network, and an owner can never silently switch
    /// to a different address.
    pub fn claim(
        &self,
        role: Role,
        name: &str,
        requested: Option<Ipv4Addr>,
        now: u64,
    ) -> Result<Assignment, ClaimError> {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let okey = owner_key(role, name);
        let existing = self.read_owner(&okey)?;

        let address = match (&existing, requested) {
            (Some(a), None) => a.address,
            (Some(a), Some(req)) if a.address == req => a.address,
            (Some(a), Some(req)) => {
                return Err(ClaimError::OwnerHasDifferent {
                    role,
                    name: name.to_string(),
                    have: a.address,
                    requested: req,
                })
            }
            (None, Some(req)) => {
                self.check_pin(req)?;
                req
            }
            (None, None) => self.allocate()?,
        };

        let assignment = Assignment {
            address,
            first_seen: existing.map(|a| a.first_seen).unwrap_or(now),
            last_seen: now,
        };
        let value = serde_json::to_vec(&assignment).map_err(storage)?;
        let addr_key = address.octets();
        (&self.by_owner, &self.by_address)
            .transaction(|(owner, addr)| {
                owner.insert(okey.as_slice(), value.as_slice())?;
                addr.insert(&addr_key[..], okey.as_slice())?;
                Ok::<(), sled::transaction::ConflictableTransactionError<()>>(())
            })
            .map_err(|e| ClaimError::Storage(format!("{e:?}")))?;
        Ok(assignment)
    }

    /// Frees `(role, name)`'s address. `Ok(None)` if it held none.
    pub fn release(&self, role: Role, name: &str) -> Result<Option<Ipv4Addr>, ClaimError> {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let okey = owner_key(role, name);
        let Some(existing) = self.read_owner(&okey)? else {
            return Ok(None);
        };
        let addr_key = existing.address.octets();
        (&self.by_owner, &self.by_address)
            .transaction(|(owner, addr)| {
                owner.remove(okey.as_slice())?;
                addr.remove(&addr_key[..])?;
                Ok::<(), sled::transaction::ConflictableTransactionError<()>>(())
            })
            .map_err(|e| ClaimError::Storage(format!("{e:?}")))?;
        Ok(Some(existing.address))
    }

    pub fn get(&self, role: Role, name: &str) -> Result<Option<Assignment>, ClaimError> {
        self.read_owner(&owner_key(role, name))
    }

    /// Every owner, ordered by key (role then name).
    pub fn entries(&self) -> Result<Vec<Entry>, ClaimError> {
        let mut out = Vec::new();
        for item in self.by_owner.iter() {
            let (key, value) = item.map_err(storage)?;
            let Some((role, name)) = parse_owner_key(&key) else {
                continue;
            };
            let assignment = serde_json::from_slice(&value).map_err(storage)?;
            out.push(Entry {
                role,
                name,
                assignment,
            });
        }
        Ok(out)
    }

    pub fn allocated(&self) -> usize {
        self.by_owner.len()
    }

    fn read_owner(&self, okey: &[u8]) -> Result<Option<Assignment>, ClaimError> {
        match self.by_owner.get(okey).map_err(storage)? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(storage)?)),
            None => Ok(None),
        }
    }

    fn check_pin(&self, req: Ipv4Addr) -> Result<(), ClaimError> {
        match self.network {
            Some(net) => {
                if !net.contains(req) {
                    return Err(ClaimError::OutsideNetwork {
                        address: req,
                        network: net,
                    });
                }
                if !net.is_host(req) {
                    return Err(ClaimError::NotHost(req));
                }
            }
            None => {
                if req.is_unspecified()
                    || req.is_broadcast()
                    || req.is_multicast()
                    || req.is_loopback()
                {
                    return Err(ClaimError::NotHost(req));
                }
            }
        }
        if let Some(holder) = self.by_address.get(req.octets()).map_err(storage)? {
            if let Some((role, name)) = parse_owner_key(&holder) {
                return Err(ClaimError::Held {
                    address: req,
                    role,
                    name,
                });
            }
        }
        Ok(())
    }

    fn allocate(&self) -> Result<Ipv4Addr, ClaimError> {
        let net = self.network.ok_or(ClaimError::NoNetwork)?;
        for n in 1..=net.capacity() {
            let ip = net.host(n);
            if !self.by_address.contains_key(ip.octets()).map_err(storage)? {
                return Ok(ip);
            }
        }
        Err(ClaimError::Exhausted {
            network: net,
            allocated: self.by_owner.len(),
            capacity: net.capacity(),
        })
    }
}

/// True when `assignment` has not been seen for longer than `after`. A zero
/// `after` disables staleness.
pub fn is_stale(assignment: &Assignment, now: u64, after: Duration) -> bool {
    !after.is_zero() && now.saturating_sub(assignment.last_seen) > after.as_secs()
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolves a registration's `backends` against the registrant's own tunnel
/// address: `:port` means "my address plus this port", `host:port` is kept,
/// and every host must equal `own` (so a peer can only ever front its own
/// address and `AllowedIPs` can never overlap).
pub fn expand_backends(backends: &[String], own: Ipv4Addr) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(backends.len());
    for b in backends {
        let (host, port) = if let Some(port) = b.strip_prefix(':') {
            let port: u16 = port
                .parse()
                .map_err(|_| format!("backend {b:?} has an invalid port"))?;
            (own, port)
        } else {
            let sa: SocketAddr = b
                .parse()
                .map_err(|_| format!("backend {b:?} is not host:port or :port"))?;
            match sa.ip() {
                IpAddr::V4(ip) => (ip, sa.port()),
                IpAddr::V6(_) => return Err(format!("backend {b:?}: IPv6 is not supported yet")),
            }
        };
        if host != own {
            return Err(format!(
                "backend {b:?} is not on this registrant's tunnel address {own}: backends must \
                 use the registrant's own address (or the :port shorthand)"
            ));
        }
        out.push(format!("{host}:{port}"));
    }
    Ok(out)
}

/// Parses `0`, or a number with a unit: `30s`, `10m`, `12h`, `14d`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("{s:?} is not a duration like 30s, 10m, 12h or 14d"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return Err(format!("{s:?} is not a duration like 30s, 10m, 12h or 14d")),
    };
    Ok(Duration::from_secs(secs))
}

pub mod api;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn book(network: Option<&str>) -> (AddressBook, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let net = network.map(|n| Network::parse(n).unwrap());
        (AddressBook::open(dir.path(), net).unwrap(), dir)
    }

    // sled releases its file lock on a background thread after a drop, so a
    // reopen right away can fail briefly (see HANDOVER "Known flakes").
    fn reopen(dir: &Path, network: Option<&str>) -> AddressBook {
        let net = network.map(|n| Network::parse(n).unwrap());
        for _ in 0..50 {
            if let Ok(b) = AddressBook::open(dir, net) {
                return b;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("could not reopen the address book");
    }

    #[test]
    fn network_parse_accepts_ipv4_and_masks_host_bits() {
        let n = Network::parse("10.60.0.7/16").unwrap();
        assert_eq!(n.to_string(), "10.60.0.0/16");
        assert_eq!(n.prefix(), 16);
        assert_eq!(n.capacity(), 65534);
    }

    #[test]
    fn network_parse_rejects_ipv6_bad_input_and_tiny_networks() {
        assert!(Network::parse("fd00::/64").unwrap_err().contains("IPv6"));
        assert!(Network::parse("10.60.0.0").is_err());
        assert!(Network::parse("10.60.0.0/x").is_err());
        assert!(Network::parse("10.60.0.0/31").is_err());
        assert!(Network::parse("10.60.0.0/32").is_err());
        assert!(Network::parse("10.60.0.0/30").is_ok());
    }

    #[test]
    fn network_host_excludes_network_and_broadcast() {
        let n = Network::parse("10.60.0.0/24").unwrap();
        assert!(!n.is_host(ip("10.60.0.0")));
        assert!(n.is_host(ip("10.60.0.1")));
        assert!(n.is_host(ip("10.60.0.254")));
        assert!(!n.is_host(ip("10.60.0.255")));
        assert!(!n.is_host(ip("10.61.0.1")));
    }

    #[test]
    fn allocation_hands_out_the_lowest_free_address_and_is_sticky() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        let a = b.claim(Role::Origin, "o1", None, 100).unwrap();
        let p = b.claim(Role::Proxy, "p1", None, 100).unwrap();
        assert_eq!(a.address, ip("10.60.0.1"));
        assert_eq!(p.address, ip("10.60.0.2"));
        // Re-registering returns the same address and bumps last_seen only.
        let again = b.claim(Role::Origin, "o1", None, 250).unwrap();
        assert_eq!(again.address, ip("10.60.0.1"));
        assert_eq!(again.first_seen, 100);
        assert_eq!(again.last_seen, 250);
    }

    #[test]
    fn the_same_name_in_two_roles_gets_two_addresses() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        let a = b.claim(Role::Origin, "x", None, 1).unwrap();
        let p = b.claim(Role::Proxy, "x", None, 1).unwrap();
        assert_ne!(a.address, p.address);
    }

    #[test]
    fn a_pinned_free_address_is_granted() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        let a = b
            .claim(Role::Origin, "o1", Some(ip("10.60.0.9")), 1)
            .unwrap();
        assert_eq!(a.address, ip("10.60.0.9"));
        // The allocator then skips it.
        let next = b.claim(Role::Origin, "o2", None, 1).unwrap();
        assert_eq!(next.address, ip("10.60.0.1"));
    }

    #[test]
    fn a_pin_held_by_another_owner_is_a_conflict_naming_the_holder() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim(Role::Origin, "o1", Some(ip("10.60.0.9")), 1)
            .unwrap();
        let err = b
            .claim(Role::Proxy, "p1", Some(ip("10.60.0.9")), 1)
            .unwrap_err();
        assert_eq!(
            err,
            ClaimError::Held {
                address: ip("10.60.0.9"),
                role: Role::Origin,
                name: "o1".into()
            }
        );
        assert!(err.to_string().contains("origin \"o1\""));
    }

    #[test]
    fn a_pin_outside_the_network_or_on_a_reserved_address_is_rejected() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        assert!(matches!(
            b.claim(Role::Origin, "o", Some(ip("10.61.0.1")), 1),
            Err(ClaimError::OutsideNetwork { .. })
        ));
        assert_eq!(
            b.claim(Role::Origin, "o", Some(ip("10.60.0.0")), 1),
            Err(ClaimError::NotHost(ip("10.60.0.0")))
        );
        assert_eq!(
            b.claim(Role::Origin, "o", Some(ip("10.60.0.255")), 1),
            Err(ClaimError::NotHost(ip("10.60.0.255")))
        );
    }

    #[test]
    fn an_owner_cannot_switch_addresses_without_releasing() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim(Role::Origin, "o1", None, 1).unwrap();
        let err = b
            .claim(Role::Origin, "o1", Some(ip("10.60.0.50")), 2)
            .unwrap_err();
        assert!(matches!(err, ClaimError::OwnerHasDifferent { .. }));
        // Asking for the address it already has is fine.
        assert!(b
            .claim(Role::Origin, "o1", Some(ip("10.60.0.1")), 3)
            .is_ok());
    }

    #[test]
    fn release_frees_the_address_for_the_next_owner() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim(Role::Origin, "o1", None, 1).unwrap();
        b.claim(Role::Origin, "o2", None, 1).unwrap();
        assert_eq!(
            b.release(Role::Origin, "o1").unwrap(),
            Some(ip("10.60.0.1"))
        );
        assert_eq!(b.release(Role::Origin, "o1").unwrap(), None);
        assert!(b.get(Role::Origin, "o1").unwrap().is_none());
        let n = b.claim(Role::Proxy, "p", None, 1).unwrap();
        assert_eq!(
            n.address,
            ip("10.60.0.1"),
            "the lowest free address is reused"
        );
    }

    #[test]
    fn an_exhausted_network_is_reported_with_counts() {
        let (b, _d) = book(Some("10.60.0.0/30")); // two hosts: .1 and .2
        b.claim(Role::Origin, "a", None, 1).unwrap();
        b.claim(Role::Origin, "b", None, 1).unwrap();
        match b.claim(Role::Origin, "c", None, 1).unwrap_err() {
            ClaimError::Exhausted {
                allocated,
                capacity,
                ..
            } => assert_eq!((allocated, capacity), (2, 2)),
            other => panic!("expected Exhausted, got {other:?}"),
        }
    }

    #[test]
    fn pin_only_mode_enforces_uniqueness_but_never_allocates() {
        let (b, _d) = book(None);
        assert_eq!(
            b.claim(Role::Origin, "o", None, 1),
            Err(ClaimError::NoNetwork)
        );
        b.claim(Role::Origin, "o", Some(ip("10.60.0.2")), 1)
            .unwrap();
        assert!(matches!(
            b.claim(Role::Proxy, "p", Some(ip("10.60.0.2")), 1),
            Err(ClaimError::Held { .. })
        ));
        assert_eq!(
            b.claim(Role::Proxy, "p", Some(ip("127.0.0.1")), 1),
            Err(ClaimError::NotHost(ip("127.0.0.1")))
        );
    }

    #[test]
    fn assignments_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let b = AddressBook::open(dir.path(), Network::parse("10.60.0.0/24").ok()).unwrap();
            b.claim(Role::Origin, "o1", None, 7).unwrap();
        }
        let b = reopen(dir.path(), Some("10.60.0.0/24"));
        let a = b.get(Role::Origin, "o1").unwrap().unwrap();
        assert_eq!(a.address, ip("10.60.0.1"));
        assert_eq!(a.first_seen, 7);
        // And the reverse index survived too: the address is still taken.
        assert!(matches!(
            b.claim(Role::Proxy, "p", Some(ip("10.60.0.1")), 8),
            Err(ClaimError::Held { .. })
        ));
    }

    #[test]
    fn concurrent_registrations_never_share_an_address() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        let b = Arc::new(b);
        let handles: Vec<_> = (0..64)
            .map(|i| {
                let b = b.clone();
                std::thread::spawn(move || {
                    b.claim(Role::Origin, &format!("o{i}"), None, 1)
                        .unwrap()
                        .address
                })
            })
            .collect();
        let mut got: Vec<Ipv4Addr> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        got.sort();
        got.dedup();
        assert_eq!(got.len(), 64, "every owner must get a distinct address");
        assert_eq!(b.allocated(), 64);
    }

    #[test]
    fn entries_lists_every_owner_with_its_role() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim(Role::Origin, "o1", None, 1).unwrap();
        b.claim(Role::Proxy, "p1", None, 1).unwrap();
        let entries = b.entries().unwrap();
        let got: Vec<(Role, &str)> = entries.iter().map(|e| (e.role, e.name.as_str())).collect();
        assert_eq!(got, vec![(Role::Origin, "o1"), (Role::Proxy, "p1")]);
    }

    #[test]
    fn staleness_uses_the_injected_clock_and_zero_disables_it() {
        let a = Assignment {
            address: ip("10.60.0.1"),
            first_seen: 0,
            last_seen: 1000,
        };
        let day = Duration::from_secs(86400);
        assert!(!is_stale(&a, 1000 + 86400, day), "exactly at the limit");
        assert!(is_stale(&a, 1000 + 86401, day));
        assert!(!is_stale(&a, u64::MAX, Duration::ZERO), "0 disables");
    }

    #[test]
    fn expand_backends_resolves_the_port_shorthand_and_keeps_own_addresses() {
        let own = ip("10.60.0.5");
        let got = expand_backends(&[":25565".into(), "10.60.0.5:25566".into()], own).unwrap();
        assert_eq!(got, vec!["10.60.0.5:25565", "10.60.0.5:25566"]);
        assert!(expand_backends(&[], own).unwrap().is_empty());
    }

    #[test]
    fn expand_backends_rejects_other_hosts_and_malformed_entries() {
        let own = ip("10.60.0.5");
        let err = expand_backends(&["10.60.0.6:1".into()], own).unwrap_err();
        assert!(err.contains("registrant's own address"), "{err}");
        assert!(expand_backends(&[":notaport".into()], own).is_err());
        assert!(expand_backends(&["nonsense".into()], own).is_err());
        assert!(expand_backends(&["[::1]:1".into()], own).is_err());
    }

    #[test]
    fn parse_duration_understands_units_and_zero() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("12h").unwrap(), Duration::from_secs(43200));
        assert_eq!(
            parse_duration("14d").unwrap(),
            Duration::from_secs(14 * 86400)
        );
        assert!(parse_duration("14").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("14w").is_err());
    }
}
