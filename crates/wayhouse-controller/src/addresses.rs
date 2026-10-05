//! Phase 14 tunnel address authority (`docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`):
//! `wayhouse-controller` allocates the tunnel-internal addresses that origins and
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

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
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

/// The address family of a tunnel [`Network`]. One family per controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    fn bits(self) -> u32 {
        match self {
            Family::V4 => 32,
            Family::V6 => 128,
        }
    }
}

/// The network tunnel addresses are allocated from (`--tunnel-network`),
/// IPv4 or IPv6. IPv4 is held in the low 32 bits of `network`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    family: Family,
    network: u128,
    prefix: u8,
}

/// The most entries the book holds, for either family. Allocation scans the
/// network linearly under the book's mutex, and the counts come from sled's
/// O(n) `Tree::len()`, so the pool is capped as an entry count (an IPv4
/// `/16`'s worth) rather than by address space.
pub const MAX_ENTRIES: u64 = 65_534;

/// The shortest IPv4 `--tunnel-network` prefix accepted (65 534 hosts).
pub const MIN_PREFIX_V4: u8 = 16;
/// The shortest IPv6 prefix: the pool is a single subnet.
pub const MIN_PREFIX_V6: u8 = 64;
/// The longest IPv6 prefix: at least 254 hosts.
pub const MAX_PREFIX_V6: u8 = 120;

fn to_u128(ip: IpAddr) -> (Family, u128) {
    match ip {
        IpAddr::V4(a) => (Family::V4, u128::from(u32::from(a))),
        IpAddr::V6(a) => (Family::V6, u128::from(a)),
    }
}

/// An IPv4-mapped (`::ffff:a.b.c.d`) or IPv4-compatible (`::a.b.c.d`) IPv6
/// address: IPv4 spelled as IPv6, which would make one host two addresses.
pub fn is_ipv4_in_ipv6(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(_) => false,
        IpAddr::V6(a) => {
            let s = a.segments();
            s[..5] == [0; 5] && (s[5] == 0xffff || s[5] == 0) && !(s[5] == 0 && s[6] == 0)
        }
    }
}

impl Network {
    /// Parses `ip/prefix`. IPv4 prefixes run from `/16` ([`MIN_PREFIX_V4`])
    /// to `/30`; IPv6 prefixes from `/64` to `/120`. Host bits are masked off.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (addr, prefix) = s.split_once('/').ok_or_else(|| {
            format!("{s:?} is not in ip/prefix form (e.g. fd49:89c1:4b5e:60::/64 or 10.60.0.0/16)")
        })?;
        let ip: IpAddr = addr
            .parse()
            .map_err(|e| format!("{addr:?} is not an IP address: {e}"))?;
        if is_ipv4_in_ipv6(ip) {
            return Err(format!(
                "{addr:?} is an IPv4 address written as IPv6: use the IPv4 form"
            ));
        }
        if let IpAddr::V6(v6) = ip {
            let seg = v6.segments();
            // ::/80 holds loopback, unspecified and the IPv4-in-IPv6 ranges;
            // multicast and link-local are not unicast pools either.
            if seg[..5] == [0; 5] || v6.is_multicast() || seg[0] & 0xffc0 == 0xfe80 {
                return Err(format!(
                    "{addr:?} is not a usable unicast network: use a ULA such as \
                     fd49:89c1:4b5e:60::/64"
                ));
            }
        }
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| format!("{prefix:?} is not a prefix length"))?;
        let (family, bits) = to_u128(ip);
        match family {
            Family::V4 => {
                if prefix < MIN_PREFIX_V4 {
                    return Err(format!(
                        "prefix /{prefix} is too short: an IPv4 tunnel network can be at most a \
                         /{MIN_PREFIX_V4} ({MAX_ENTRIES} host addresses)"
                    ));
                }
                if prefix > 30 {
                    return Err(format!(
                        "prefix /{prefix} is too long: the network needs at least two usable \
                         host addresses (use /30 or shorter)"
                    ));
                }
            }
            Family::V6 => {
                if prefix < MIN_PREFIX_V6 {
                    return Err(format!(
                        "prefix /{prefix} is too short: the IPv6 tunnel pool is a single subnet, \
                         at most a /{MIN_PREFIX_V6}"
                    ));
                }
                if prefix > MAX_PREFIX_V6 {
                    return Err(format!(
                        "prefix /{prefix} is too long: an IPv6 tunnel network must be \
                         /{MAX_PREFIX_V6} or shorter"
                    ));
                }
            }
        }
        let mut n = Network {
            family,
            network: 0,
            prefix,
        };
        n.network = bits & n.mask();
        Ok(n)
    }

    pub fn family(&self) -> Family {
        self.family
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    fn host_bits(&self) -> u32 {
        self.family.bits() - u32::from(self.prefix)
    }

    /// The prefix mask over the family's width (host bits are always 2..=64,
    /// so no shift overflows).
    fn mask(&self) -> u128 {
        let width = u128::MAX >> (128 - self.family.bits());
        (width >> self.host_bits()) << self.host_bits()
    }

    fn all_ones(&self) -> u128 {
        self.network | ((1u128 << self.host_bits()) - 1)
    }

    /// True for an address of this network's family inside the prefix.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let (family, bits) = to_u128(ip);
        family == self.family && bits & self.mask() == self.network
    }

    /// Inside the network and neither its network nor its all-ones
    /// (broadcast) address.
    pub fn is_host(&self, ip: IpAddr) -> bool {
        let bits = to_u128(ip).1;
        self.contains(ip) && bits != self.network && bits != self.all_ones()
    }

    /// How many host addresses the network holds.
    pub fn host_count(&self) -> u128 {
        (1u128 << self.host_bits()) - 2
    }

    /// How many addresses the book will hold: `min(host_count, MAX_ENTRIES)`.
    pub fn capacity(&self) -> u64 {
        self.host_count().min(u128::from(MAX_ENTRIES)) as u64
    }

    /// The `n`th host address, `n` in `1..=host_count()`.
    fn host(&self, n: u64) -> IpAddr {
        self.addr(self.network + u128::from(n))
    }

    fn addr(&self, bits: u128) -> IpAddr {
        match self.family {
            Family::V4 => IpAddr::V4(Ipv4Addr::from(bits as u32)),
            Family::V6 => IpAddr::V6(Ipv6Addr::from(bits)),
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr(self.network), self.prefix)
    }
}

/// A network serializes as its `ip/prefix` string (the `u128` inside is an
/// implementation detail JSON should not carry).
impl Serialize for Network {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Network {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Network::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// One owner's address and when it was first/last seen (unix seconds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub address: IpAddr,
    pub first_seen: u64,
    pub last_seen: u64,
}

/// An owner and its assignment, as listed by [`AddressBook::entries`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub role: Role,
    pub name: String,
    pub assignment: Assignment,
}

/// Why a claim was refused. Every variant is a deterministic function of the
/// book's contents and the claim's arguments, so replicas applying the same
/// log agree on it; it is serialized into the book's `last_outcome`. A storage
/// failure is never a `Rejection`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum Rejection {
    #[error("address {address} is already held by {role} {name:?}")]
    Held {
        address: IpAddr,
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
        have: IpAddr,
        requested: IpAddr,
    },
    #[error("address {address} is outside the tunnel network {network}")]
    OutsideNetwork { address: IpAddr, network: Network },
    #[error("address {0} is not a usable tunnel host address")]
    NotHost(IpAddr),
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
    #[error("the address book is full ({allocated} entries): DELETE unused registrations first")]
    Full { allocated: usize },
    /// A registration's backends are not on the claimed address (the message
    /// is `expand_backends`'s).
    #[error("{0}")]
    BackendHost(String),
    /// A replicated `SetTunnelNetwork` entry carried a network that does not
    /// parse (the message is `Network::parse`'s).
    #[error("invalid tunnel network: {0}")]
    InvalidNetwork(String),
    /// The replicated registries have not been initialized yet.
    #[error("cluster is initializing its registries")]
    NotInitialized,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClaimError {
    #[error(transparent)]
    Rejected(Rejection),
    #[error("tunnel address storage error: {0}")]
    Storage(String),
}

/// The book's storage (sled or serialization) failed. Distinct from a
/// [`Rejection`]: it says nothing about the claim and must not be recorded as
/// its outcome.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("tunnel address storage error: {0}")]
pub struct StorageFailure(pub String);

impl From<StorageFailure> for ClaimError {
    fn from(f: StorageFailure) -> Self {
        ClaimError::Storage(f.0)
    }
}

impl From<Rejection> for ClaimError {
    fn from(r: Rejection) -> Self {
        ClaimError::Rejected(r)
    }
}

/// What one indexed claim did. `Granted` and `Rejected` are recorded as the
/// book's `last_outcome`; `AlreadyApplied` (the step's index is not beyond the
/// book's `applied_index`) is never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Granted(Assignment),
    Rejected(Rejection),
    AlreadyApplied,
}

fn storage<E: std::fmt::Display>(e: E) -> ClaimError {
    ClaimError::Storage(e.to_string())
}

fn failure<E: std::fmt::Display>(e: E) -> StorageFailure {
    StorageFailure(e.to_string())
}

/// The `by_address` key: the address's raw octets, 4 bytes for IPv4 and 16
/// for IPv6, so keys of the two families never collide and databases
/// written before IPv6 support open unchanged.
fn addr_key(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(a) => a.octets().to_vec(),
        IpAddr::V6(a) => a.octets().to_vec(),
    }
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
    /// `applied_index` and `last_outcome`, written in the same transaction as
    /// an indexed step's other writes.
    meta: sled::Tree,
    write: Mutex<()>,
    /// `--tunnel-readdress`: move owners outside the network on their next claim.
    readdress: bool,
    // Held so the database stays open for the trees' lifetime.
    db: sled::Db,
}

const APPLIED_INDEX: &[u8] = b"applied_index";
const LAST_OUTCOME: &[u8] = b"last_outcome";

/// `last_outcome` as stored: the outcome and the index of the claim it is for
/// (`applied_index` also advances on release and touch, so it can be newer).
#[derive(Serialize, Deserialize)]
struct StoredOutcome {
    index: u64,
    outcome: Outcome,
}

/// The whole book as a Raft snapshot carries it ([`AddressBook::snapshot`],
/// [`AddressBook::replace`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookSnapshot {
    pub entries: Vec<Entry>,
    pub applied_index: Option<u64>,
    pub last_outcome: Option<(u64, Outcome)>,
}

/// One write of a book transaction.
enum Op {
    OwnerPut(Vec<u8>, Vec<u8>),
    OwnerRemove(Vec<u8>),
    AddrPut(Vec<u8>, Vec<u8>),
    AddrRemove(Vec<u8>),
}

/// Why a transaction aborted instead of committing.
enum Abort {
    /// The book had already applied this index.
    Skip,
    Corrupt(String),
}

/// What [`AddressBook::commit`] did.
#[derive(Debug, PartialEq, Eq)]
enum Committed {
    Done,
    AlreadyApplied,
}

fn decode_index(bytes: &[u8]) -> Result<u64, String> {
    let raw: [u8; 8] = bytes
        .try_into()
        .map_err(|_| format!("applied_index is {} bytes, not 8", bytes.len()))?;
    Ok(u64::from_be_bytes(raw))
}

impl AddressBook {
    /// Opens (creating if missing) the book under `dir`. `network` of `None`
    /// is pin-only mode: uniqueness is enforced but nothing is allocated.
    pub fn open(dir: &Path, network: Option<Network>) -> Result<Self, sled::Error> {
        let db = sled::open(dir)?;
        let by_owner = db.open_tree("by_owner")?;
        let by_address = db.open_tree("by_address")?;
        let meta = db.open_tree("meta")?;
        Ok(AddressBook {
            network,
            by_owner,
            by_address,
            meta,
            write: Mutex::new(()),
            readdress: false,
            db,
        })
    }

    pub fn network(&self) -> Option<Network> {
        self.network
    }

    /// Turns on `--tunnel-readdress`: an existing owner whose address is not
    /// a host of the configured network gets a new one on its next claim.
    pub fn with_readdress(mut self, on: bool) -> Self {
        self.readdress = on;
        self
    }

    /// Entries whose address is not a host of the configured network (other
    /// family, outside the prefix, or now its network/all-ones address).
    /// Always empty in pin-only mode.
    pub fn outside_network(&self) -> Result<Vec<Entry>, ClaimError> {
        let Some(net) = self.network else {
            return Ok(Vec::new());
        };
        Ok(self
            .entries()?
            .into_iter()
            .filter(|e| !net.is_host(e.assignment.address))
            .collect())
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
        requested: Option<IpAddr>,
        now: u64,
    ) -> Result<Assignment, ClaimError> {
        match self.claim_core(
            role,
            name,
            requested,
            now,
            self.network,
            self.readdress,
            None,
        )? {
            Outcome::Granted(a) => Ok(a),
            Outcome::Rejected(r) => Err(ClaimError::Rejected(r)),
            Outcome::AlreadyApplied => Err(ClaimError::Storage(
                "a claim without a Raft index was skipped as already applied".into(),
            )),
        }
    }

    /// The deterministic claim a replicated log applies at `index`. It reads
    /// nothing node-local: the caller gives `now` and `network`, and
    /// `--tunnel-readdress` is not honoured. A step whose `index` is not
    /// beyond [`applied_index`](Self::applied_index) is skipped entirely
    /// (`AlreadyApplied`); otherwise the index and the outcome are recorded
    /// with the claim's writes, and a rejection writes only those.
    pub fn claim_at(
        &self,
        role: Role,
        name: &str,
        requested: Option<IpAddr>,
        now: u64,
        network: Option<Network>,
        index: u64,
    ) -> Result<Outcome, StorageFailure> {
        self.claim_core(role, name, requested, now, network, false, Some(index))
    }

    /// The one claim implementation. `index` of `None` is the non-replicated
    /// path: nothing is written to `meta`.
    #[allow(clippy::too_many_arguments)] // one core shared by claim and claim_at
    fn claim_core(
        &self,
        role: Role,
        name: &str,
        requested: Option<IpAddr>,
        now: u64,
        network: Option<Network>,
        readdress: bool,
        index: Option<u64>,
    ) -> Result<Outcome, StorageFailure> {
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(index) = index {
            if self
                .applied_index()?
                .is_some_and(|applied| applied >= index)
            {
                return Ok(Outcome::AlreadyApplied);
            }
        }
        let okey = owner_key(role, name);
        let decided = self.decide(&okey, role, name, requested, now, network, readdress);
        let (outcome, ops, readdressed_from) = match decided {
            Ok((assignment, ops, from)) => (Outcome::Granted(assignment), ops, from),
            Err(ClaimError::Rejected(r)) => (Outcome::Rejected(r), Vec::new(), None),
            Err(ClaimError::Storage(e)) => return Err(StorageFailure(e)),
        };
        let recorded = index
            .map(|index| {
                serde_json::to_vec(&StoredOutcome {
                    index,
                    outcome: outcome.clone(),
                })
                .map_err(failure)
                .map(|bytes| (index, bytes))
            })
            .transpose()?;
        let meta = recorded.as_ref().map(|(i, b)| (*i, Some(b.as_slice())));
        match self.commit(&ops, meta)? {
            Committed::Done => {}
            Committed::AlreadyApplied => return Ok(Outcome::AlreadyApplied),
        }
        if let (Outcome::Granted(a), Some(old)) = (&outcome, readdressed_from) {
            tracing::info!(%role, name, %old, new = %a.address, "re-addressed into the new tunnel network");
        }
        Ok(outcome)
    }

    /// Evaluates a claim against the book: the assignment, the writes that
    /// record it and the address it was moved off by `--tunnel-readdress`, or
    /// why it is refused. Writes nothing.
    #[allow(clippy::too_many_arguments)] // mirrors claim_core
    fn decide(
        &self,
        okey: &[u8],
        role: Role,
        name: &str,
        requested: Option<IpAddr>,
        now: u64,
        network: Option<Network>,
        readdress: bool,
    ) -> Result<(Assignment, Vec<Op>, Option<IpAddr>), ClaimError> {
        let mut existing = self.read_owner(okey)?;
        // `--tunnel-readdress`: an owner left outside a changed network is
        // claimed as a new owner, and its old address freed in the same
        // transaction.
        let mut old_key = None;
        if let (Some(a), Some(net)) = (&existing, network) {
            if readdress && !net.is_host(a.address) {
                old_key = Some(a.address);
                existing = None;
            }
        }

        let address = match (&existing, requested) {
            (Some(a), None) => a.address,
            (Some(a), Some(req)) if a.address == req => a.address,
            (Some(a), Some(req)) => {
                return Err(Rejection::OwnerHasDifferent {
                    role,
                    name: name.to_string(),
                    have: a.address,
                    requested: req,
                }
                .into())
            }
            (None, Some(req)) => {
                if old_key.is_none() {
                    self.check_room(network)?;
                }
                self.check_pin(network, req)?;
                req
            }
            (None, None) => {
                if old_key.is_none() {
                    self.check_room(network)?;
                }
                self.allocate(network)?
            }
        };

        let assignment = Assignment {
            address,
            first_seen: existing.map(|a| a.first_seen).unwrap_or(now),
            last_seen: now,
        };
        let value = serde_json::to_vec(&assignment).map_err(storage)?;
        let mut ops = Vec::new();
        if let Some(old) = old_key {
            ops.push(Op::AddrRemove(addr_key(old)));
        }
        ops.push(Op::OwnerPut(okey.to_vec(), value));
        ops.push(Op::AddrPut(addr_key(address), okey.to_vec()));
        Ok((assignment, ops, old_key))
    }

    /// Frees `(role, name)`'s address. `Ok(None)` if it held none.
    pub fn release(&self, role: Role, name: &str) -> Result<Option<IpAddr>, ClaimError> {
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(self.release_locked(role, name, None)?.flatten())
    }

    /// [`release`](Self::release) as the replicated log applies it at `index`.
    /// `None` means the step was already applied; otherwise the freed address
    /// (itself `None` if the owner held none). `applied_index` advances either
    /// way; `last_outcome` is left alone (it records claims only).
    pub fn release_at(
        &self,
        role: Role,
        name: &str,
        index: u64,
    ) -> Result<Option<Option<IpAddr>>, StorageFailure> {
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        self.release_locked(role, name, Some(index))
    }

    fn release_locked(
        &self,
        role: Role,
        name: &str,
        index: Option<u64>,
    ) -> Result<Option<Option<IpAddr>>, StorageFailure> {
        if self.already_applied(index)? {
            return Ok(None);
        }
        let okey = owner_key(role, name);
        let existing = self.read_owner(&okey)?;
        let ops: Vec<Op> = existing
            .iter()
            .flat_map(|a| {
                [
                    Op::OwnerRemove(okey.clone()),
                    Op::AddrRemove(addr_key(a.address)),
                ]
            })
            .collect();
        match self.commit(&ops, index.map(|i| (i, None)))? {
            Committed::Done => Ok(Some(existing.map(|a| a.address))),
            Committed::AlreadyApplied => Ok(None),
        }
    }

    /// Refreshes `last_seen` of `(role, name)` at `index` and nothing else
    /// (the address and `first_seen` stay). `None` means the step was already
    /// applied; `Some(false)` that the owner is unknown, which still advances
    /// `applied_index`. `last_outcome` is left alone (it records claims only).
    pub fn touch_at(
        &self,
        role: Role,
        name: &str,
        now: u64,
        index: u64,
    ) -> Result<Option<bool>, StorageFailure> {
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        if self.already_applied(Some(index))? {
            return Ok(None);
        }
        let okey = owner_key(role, name);
        let touched = self.read_owner(&okey)?;
        let mut ops = Vec::new();
        if let Some(mut a) = touched.clone() {
            a.last_seen = now;
            ops.push(Op::OwnerPut(okey, serde_json::to_vec(&a).map_err(failure)?));
        }
        match self.commit(&ops, Some((index, None)))? {
            Committed::Done => Ok(Some(touched.is_some())),
            Committed::AlreadyApplied => Ok(None),
        }
    }

    /// The highest index this book has applied; `None` for a book that has
    /// only ever served the non-replicated path.
    pub fn applied_index(&self) -> Result<Option<u64>, StorageFailure> {
        self.meta
            .get(APPLIED_INDEX)
            .map_err(failure)?
            .map(|b| decode_index(&b).map_err(StorageFailure))
            .transpose()
    }

    /// The most recent claim's index and outcome (`Granted` or `Rejected`).
    pub fn last_outcome(&self) -> Result<Option<(u64, Outcome)>, StorageFailure> {
        self.meta
            .get(LAST_OUTCOME)
            .map_err(failure)?
            .map(|b| {
                serde_json::from_slice::<StoredOutcome>(&b)
                    .map(|s| (s.index, s.outcome))
                    .map_err(failure)
            })
            .transpose()
    }

    /// Records `rejection` as the claim outcome at `index` without evaluating
    /// anything — a register entry refused before the book was consulted (the
    /// cluster has no recorded network yet). Writes `applied_index` and
    /// `last_outcome` only; `AlreadyApplied` when `index` was already applied.
    pub fn reject_at(&self, rejection: Rejection, index: u64) -> Result<Outcome, StorageFailure> {
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        let outcome = Outcome::Rejected(rejection);
        let bytes = serde_json::to_vec(&StoredOutcome {
            index,
            outcome: outcome.clone(),
        })
        .map_err(failure)?;
        match self.commit(&[], Some((index, Some(bytes.as_slice()))))? {
            Committed::Done => Ok(outcome),
            Committed::AlreadyApplied => Ok(Outcome::AlreadyApplied),
        }
    }

    /// Advances `applied_index` to `index` and writes nothing else — a
    /// release or touch entry refused before the book was consulted.
    /// `false` when `index` was already applied.
    pub fn mark_applied(&self, index: u64) -> Result<bool, StorageFailure> {
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(self.commit(&[], Some((index, None)))? == Committed::Done)
    }

    /// An owned copy of the whole book (every entry, `applied_index` and
    /// `last_outcome`), for a Raft snapshot.
    pub fn snapshot(&self) -> Result<BookSnapshot, StorageFailure> {
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(BookSnapshot {
            entries: self.entries().map_err(|e| StorageFailure(e.to_string()))?,
            applied_index: self.applied_index()?,
            last_outcome: self.last_outcome()?,
        })
    }

    /// Replaces the whole book by `snapshot` in one transaction across its
    /// three trees — a Raft snapshot install.
    pub fn replace(&self, snapshot: &BookSnapshot) -> Result<(), StorageFailure> {
        use sled::transaction::{ConflictableTransactionError, TransactionError};
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        // `sled` transactions cannot iterate: collect the stale keys first
        // (the guard keeps every other writer out meanwhile).
        let keys = |tree: &sled::Tree| {
            tree.iter()
                .keys()
                .collect::<Result<Vec<_>, _>>()
                .map_err(failure)
        };
        let (stale_owners, stale_addresses) = (keys(&self.by_owner)?, keys(&self.by_address)?);
        let mut owners = Vec::with_capacity(snapshot.entries.len());
        for e in &snapshot.entries {
            let okey = owner_key(e.role, &e.name);
            let value = serde_json::to_vec(&e.assignment).map_err(failure)?;
            owners.push((okey, value, addr_key(e.assignment.address)));
        }
        let outcome = snapshot
            .last_outcome
            .as_ref()
            .map(|(index, outcome)| {
                serde_json::to_vec(&StoredOutcome {
                    index: *index,
                    outcome: outcome.clone(),
                })
            })
            .transpose()
            .map_err(failure)?;
        let result =
            (&self.by_owner, &self.by_address, &self.meta).transaction(|(owner, addr, meta)| {
                for k in &stale_owners {
                    owner.remove(k)?;
                }
                for k in &stale_addresses {
                    addr.remove(k)?;
                }
                for (okey, value, akey) in &owners {
                    owner.insert(okey.as_slice(), value.as_slice())?;
                    addr.insert(akey.as_slice(), okey.as_slice())?;
                }
                match snapshot.applied_index {
                    Some(i) => meta.insert(APPLIED_INDEX, &i.to_be_bytes()[..])?,
                    None => meta.remove(APPLIED_INDEX)?,
                };
                match &outcome {
                    Some(bytes) => meta.insert(LAST_OUTCOME, bytes.as_slice())?,
                    None => meta.remove(LAST_OUTCOME)?,
                };
                Ok::<_, ConflictableTransactionError<std::convert::Infallible>>(())
            });
        match result {
            Ok(()) => {}
            Err(TransactionError::Storage(e)) => return Err(failure(format!("{e:?}"))),
            Err(TransactionError::Abort(never)) => match never {},
        }
        self.db.flush().map_err(failure)?;
        Ok(())
    }

    /// The `meta` tree, for tests that corrupt it to provoke a storage
    /// failure.
    #[cfg(test)]
    pub(crate) fn meta_tree(&self) -> &sled::Tree {
        &self.meta
    }

    fn already_applied(&self, index: Option<u64>) -> Result<bool, StorageFailure> {
        Ok(match index {
            Some(index) => self
                .applied_index()?
                .is_some_and(|applied| applied >= index),
            None => false,
        })
    }

    /// Applies `ops` in one transaction across the three trees, together with
    /// `meta` (`applied_index`, and `last_outcome` when given). The applied
    /// index is checked again inside the transaction, so a step can never be
    /// committed twice. With no `meta` nothing is written to the `meta` tree.
    fn commit(
        &self,
        ops: &[Op],
        meta: Option<(u64, Option<&[u8]>)>,
    ) -> Result<Committed, StorageFailure> {
        use sled::transaction::{ConflictableTransactionError, TransactionError};
        // Nothing to write (a non-indexed no-op): no transaction, no flush.
        if ops.is_empty() && meta.is_none() {
            return Ok(Committed::Done);
        }
        let result =
            (&self.by_owner, &self.by_address, &self.meta).transaction(|(owner, addr, metat)| {
                if let Some((index, outcome)) = meta {
                    if let Some(bytes) = metat.get(APPLIED_INDEX)? {
                        let applied = decode_index(&bytes)
                            .map_err(|e| ConflictableTransactionError::Abort(Abort::Corrupt(e)))?;
                        if applied >= index {
                            return Err(ConflictableTransactionError::Abort(Abort::Skip));
                        }
                    }
                    metat.insert(APPLIED_INDEX, &index.to_be_bytes()[..])?;
                    if let Some(outcome) = outcome {
                        metat.insert(LAST_OUTCOME, outcome)?;
                    }
                }
                for op in ops {
                    match op {
                        Op::OwnerPut(k, v) => owner.insert(k.as_slice(), v.as_slice())?,
                        Op::OwnerRemove(k) => owner.remove(k.as_slice())?,
                        Op::AddrPut(k, v) => addr.insert(k.as_slice(), v.as_slice())?,
                        Op::AddrRemove(k) => addr.remove(k.as_slice())?,
                    };
                }
                Ok(())
            });
        match result {
            Ok(()) => {}
            Err(TransactionError::Abort(Abort::Skip)) => return Ok(Committed::AlreadyApplied),
            Err(TransactionError::Abort(Abort::Corrupt(e))) => return Err(StorageFailure(e)),
            Err(TransactionError::Storage(e)) => return Err(failure(format!("{e:?}"))),
        }
        self.db.flush().map_err(failure)?;
        Ok(Committed::Done)
    }

    pub fn get(&self, role: Role, name: &str) -> Result<Option<Assignment>, ClaimError> {
        Ok(self.read_owner(&owner_key(role, name))?)
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

    fn read_owner(&self, okey: &[u8]) -> Result<Option<Assignment>, StorageFailure> {
        match self.by_owner.get(okey).map_err(failure)? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(failure)?)),
            None => Ok(None),
        }
    }

    fn check_pin(&self, network: Option<Network>, req: IpAddr) -> Result<(), ClaimError> {
        match network {
            Some(net) => {
                if !net.contains(req) {
                    return Err(Rejection::OutsideNetwork {
                        address: req,
                        network: net,
                    }
                    .into());
                }
                if !net.is_host(req) {
                    return Err(Rejection::NotHost(req).into());
                }
            }
            None => {
                let unusable = match req {
                    IpAddr::V4(a) => a.is_broadcast(),
                    // Link-local, fe80::/10.
                    IpAddr::V6(a) => a.segments()[0] & 0xffc0 == 0xfe80,
                };
                if unusable || req.is_unspecified() || req.is_multicast() || req.is_loopback() {
                    return Err(Rejection::NotHost(req).into());
                }
            }
        }
        if is_ipv4_in_ipv6(req) {
            return Err(Rejection::NotHost(req).into());
        }
        if let Some(holder) = self.by_address.get(addr_key(req)).map_err(storage)? {
            // An entry we cannot read still means "taken": fail closed rather
            // than hand the address to a second owner.
            let Some((role, name)) = parse_owner_key(&holder) else {
                return Err(storage(format!(
                    "address {req} is held by an unreadable owner entry"
                )));
            };
            return Err(Rejection::Held {
                address: req,
                role,
                name,
            }
            .into());
        }
        Ok(())
    }

    fn allocate(&self, network: Option<Network>) -> Result<IpAddr, ClaimError> {
        let net = network.ok_or(Rejection::NoNetwork)?;
        // Fewer than `capacity` entries means one of the first `capacity`
        // hosts is free, so the scan never has to go further.
        for n in 1..=net.capacity() {
            let ip = net.host(n);
            if !self
                .by_address
                .contains_key(addr_key(ip))
                .map_err(storage)?
            {
                return Ok(ip);
            }
        }
        Err(Rejection::Exhausted {
            network: net,
            allocated: self.by_owner.len(),
            capacity: net.capacity(),
        }
        .into())
    }

    /// A new owner is refused once the book holds `capacity` entries
    /// ([`MAX_ENTRIES`] in pin-only mode), pins included.
    fn check_room(&self, network: Option<Network>) -> Result<(), ClaimError> {
        let capacity = network.map_or(MAX_ENTRIES, |n| n.capacity());
        let allocated = self.by_owner.len();
        if allocated as u64 >= capacity {
            return Err(match network {
                Some(network) => Rejection::Exhausted {
                    network,
                    allocated,
                    capacity,
                },
                None => Rejection::Full { allocated },
            }
            .into());
        }
        Ok(())
    }
}

/// True when `assignment` has not been seen for longer than `after`. A zero
/// `after` disables staleness.
pub fn is_stale(assignment: &Assignment, now: u64, after: Duration) -> bool {
    !after.is_zero() && now.saturating_sub(assignment.last_seen) > after.as_secs()
}

pub fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolves a registration's `backends` against the registrant's own tunnel
/// address: `:port` means "my address plus this port", `host:port` is kept,
/// and every host must equal `own` (so a peer can only ever front its own
/// address and `AllowedIPs` can never overlap).
pub fn expand_backends(backends: &[String], own: IpAddr) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(backends.len());
    for b in backends {
        let sa = if let Some(port) = b.strip_prefix(':') {
            let port: u16 = port
                .parse()
                .map_err(|_| format!("backend {b:?} has an invalid port"))?;
            SocketAddr::new(own, port)
        } else {
            b.parse::<SocketAddr>()
                .map_err(|_| format!("backend {b:?} is not host:port or :port"))?
        };
        // Parsed addresses, so `fd49:0::5` and `fd49::5` are the same host.
        if sa.ip() != own {
            return Err(format!(
                "backend {b:?} is not on this registrant's tunnel address {own}: backends must \
                 use the registrant's own address (or the :port shorthand)"
            ));
        }
        // `SocketAddr`'s Display brackets IPv6: `[fd49::5]:25565`.
        out.push(sa.to_string());
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
    let per_unit: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(format!("{s:?} is not a duration like 30s, 10m, 12h or 14d")),
    };
    let secs = n
        .checked_mul(per_unit)
        .ok_or_else(|| format!("{s:?} is too large"))?;
    Ok(Duration::from_secs(secs))
}

/// The startup check for a changed `--tunnel-network`: `Err` naming the
/// network, the count and up to ten `role name address` entries when stored
/// addresses fall outside it and `readdress` is off; otherwise the entries
/// that will be re-addressed (empty when nothing changed).
pub fn check_network_change(book: &AddressBook, readdress: bool) -> Result<Vec<Entry>, String> {
    let outside = book.outside_network().map_err(|e| e.to_string())?;
    if outside.is_empty() || readdress {
        return Ok(outside);
    }
    let network = book.network().map(|n| n.to_string()).unwrap_or_default();
    Err(format!(
        "--tunnel-network {network} does not contain {} stored tunnel address(es); restore the \
         previous --tunnel-network, or pass --tunnel-readdress to move these peers to the new \
         network at their next registration\n{}",
        outside.len(),
        summarize(&outside)
    ))
}

/// Up to ten `role name address` lines, plus a count of the rest.
pub fn summarize(entries: &[Entry]) -> String {
    let mut lines: Vec<String> = entries
        .iter()
        .take(10)
        .map(|e| format!("  {} {} {}", e.role, e.name, e.assignment.address))
        .collect();
    if entries.len() > 10 {
        lines.push(format!("  … and {} more", entries.len() - 10));
    }
    lines.join("\n")
}

/// Validates the controller's `--tunnel-*` flags together: parses the network
/// and the stale threshold, and refuses `--tunnel-readdress` without a network
/// or together with HA (re-addressing is one writer's decision; the replicated
/// registries have no such writer). A network together with HA is fine: the
/// cluster records one network and every node's flag is checked against it.
pub fn resolve_flags(
    network: Option<&str>,
    stale_after: &str,
    ha_enabled: bool,
    readdress: bool,
) -> Result<(Option<Network>, Duration), String> {
    if readdress && ha_enabled {
        return Err(
            "--tunnel-readdress cannot be combined with --ha-peers or --ha-join: \
             re-addressing must run as one writer's decision, and the cluster records its \
             tunnel network once"
                .into(),
        );
    }
    if readdress && network.is_none() {
        return Err("--tunnel-readdress needs a --tunnel-network to re-address into".into());
    }
    let stale = parse_duration(stale_after).map_err(|e| format!("--tunnel-stale-after: {e}"))?;
    let net = match network {
        Some(n) => Some(Network::parse(n).map_err(|e| format!("--tunnel-network: {e}"))?),
        None => None,
    };
    Ok((net, stale))
}

pub mod api;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ip(s: &str) -> IpAddr {
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
    fn network_parse_caps_the_network_at_a_slash_16() {
        assert_eq!(Network::parse("10.60.0.0/16").unwrap().capacity(), 65534);
        let err = Network::parse("10.0.0.0/15").unwrap_err();
        assert!(err.contains("/16"), "{err}");
        assert!(Network::parse("10.0.0.0/8").is_err());
        assert!(Network::parse("0.0.0.0/0").is_err());
    }

    #[test]
    fn network_parse_rejects_bad_input_and_tiny_networks() {
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
            ClaimError::Rejected(Rejection::Held {
                address: ip("10.60.0.9"),
                role: Role::Origin,
                name: "o1".into()
            })
        );
        assert!(err.to_string().contains("origin \"o1\""));
    }

    #[test]
    fn a_pin_whose_holder_key_is_unreadable_is_not_treated_as_free() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        // A corrupt holder entry must fail closed, never hand the address out.
        b.by_address
            .insert(
                addr_key(ip("10.60.0.9")),
                b"\xff\xfe-not-an-owner-key".to_vec(),
            )
            .unwrap();
        let err = b
            .claim(Role::Origin, "o1", Some(ip("10.60.0.9")), 1)
            .unwrap_err();
        assert!(matches!(err, ClaimError::Storage(_)), "{err:?}");
        assert!(b.get(Role::Origin, "o1").unwrap().is_none());
    }

    #[test]
    fn a_pin_outside_the_network_or_on_a_reserved_address_is_rejected() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        assert!(matches!(
            b.claim(Role::Origin, "o", Some(ip("10.61.0.1")), 1),
            Err(ClaimError::Rejected(Rejection::OutsideNetwork { .. }))
        ));
        assert_eq!(
            b.claim(Role::Origin, "o", Some(ip("10.60.0.0")), 1),
            Err(ClaimError::Rejected(Rejection::NotHost(ip("10.60.0.0"))))
        );
        assert_eq!(
            b.claim(Role::Origin, "o", Some(ip("10.60.0.255")), 1),
            Err(ClaimError::Rejected(Rejection::NotHost(ip("10.60.0.255"))))
        );
    }

    #[test]
    fn an_owner_cannot_switch_addresses_without_releasing() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim(Role::Origin, "o1", None, 1).unwrap();
        let err = b
            .claim(Role::Origin, "o1", Some(ip("10.60.0.50")), 2)
            .unwrap_err();
        assert!(matches!(
            err,
            ClaimError::Rejected(Rejection::OwnerHasDifferent { .. })
        ));
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
            ClaimError::Rejected(Rejection::Exhausted {
                allocated,
                capacity,
                ..
            }) => assert_eq!((allocated, capacity), (2, 2)),
            other => panic!("expected Exhausted, got {other:?}"),
        }
    }

    #[test]
    fn pin_only_mode_enforces_uniqueness_but_never_allocates() {
        let (b, _d) = book(None);
        assert_eq!(
            b.claim(Role::Origin, "o", None, 1),
            Err(ClaimError::Rejected(Rejection::NoNetwork))
        );
        b.claim(Role::Origin, "o", Some(ip("10.60.0.2")), 1)
            .unwrap();
        assert!(matches!(
            b.claim(Role::Proxy, "p", Some(ip("10.60.0.2")), 1),
            Err(ClaimError::Rejected(Rejection::Held { .. }))
        ));
        assert_eq!(
            b.claim(Role::Proxy, "p", Some(ip("127.0.0.1")), 1),
            Err(ClaimError::Rejected(Rejection::NotHost(ip("127.0.0.1"))))
        );
    }

    #[test]
    fn assignments_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let b = AddressBook::open(dir.path(), Network::parse("10.60.0.0/24").ok()).unwrap();
            b.claim(Role::Origin, "o1", None, 7).unwrap();
            b.claim(Role::Origin, "o2", None, 7).unwrap();
            assert_eq!(
                b.release(Role::Origin, "o2").unwrap(),
                Some(ip("10.60.0.2"))
            );
        }
        let b = reopen(dir.path(), Some("10.60.0.0/24"));
        // The release survived too: o2 is gone and its address is free again.
        assert!(b.get(Role::Origin, "o2").unwrap().is_none());
        assert_eq!(
            b.claim(Role::Origin, "o3", None, 9).unwrap().address,
            ip("10.60.0.2")
        );
        let a = b.get(Role::Origin, "o1").unwrap().unwrap();
        assert_eq!(a.address, ip("10.60.0.1"));
        assert_eq!(a.first_seen, 7);
        // And the reverse index survived too: the address is still taken.
        assert!(matches!(
            b.claim(Role::Proxy, "p", Some(ip("10.60.0.1")), 8),
            Err(ClaimError::Rejected(Rejection::Held { .. }))
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
        let mut got: Vec<IpAddr> = handles.into_iter().map(|h| h.join().unwrap()).collect();
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
    fn ipv6_network_parse_accepts_slash_64_to_120_and_masks_host_bits() {
        let n = Network::parse("fd49:89c1:4b5e:60:0:0:0:7/64").unwrap();
        assert_eq!(n.to_string(), "fd49:89c1:4b5e:60::/64");
        assert_eq!(n.prefix(), 64);
        assert_eq!(n.capacity(), MAX_ENTRIES);
        let n = Network::parse("fd49::1234/120").unwrap();
        assert_eq!(n.to_string(), "fd49::1200/120");
        assert_eq!(n.capacity(), 254);
        let err = Network::parse("fd49::/63").unwrap_err();
        assert!(err.contains("/64"), "{err}");
        assert!(Network::parse("fd49::/121").is_err());
        assert!(Network::parse("fd49::/128").is_err());
        assert!(Network::parse("fd49::/0").is_err());
        assert!(Network::parse("fd49::/x").is_err());
        assert!(Network::parse("fd49::zz/64").is_err());
    }

    #[test]
    fn ipv6_capacity_is_capped_at_max_entries_and_ipv4_keeps_its_count() {
        assert_eq!(MAX_ENTRIES, 65534);
        assert_eq!(Network::parse("fd49::/64").unwrap().capacity(), MAX_ENTRIES);
        assert_eq!(Network::parse("fd49::/112").unwrap().capacity(), 65534);
        assert_eq!(Network::parse("fd49::/113").unwrap().capacity(), 32766);
        assert_eq!(Network::parse("10.60.0.0/30").unwrap().capacity(), 2);
    }

    #[test]
    fn ipv6_host_excludes_the_network_and_all_ones_addresses_and_other_families() {
        let n = Network::parse("fd49::/120").unwrap();
        assert!(!n.is_host(ip("fd49::")));
        assert!(n.is_host(ip("fd49::1")));
        assert!(n.is_host(ip("fd49::fe")));
        assert!(!n.is_host(ip("fd49::ff")));
        assert!(!n.is_host(ip("fd49::100")));
        assert!(!n.contains(ip("10.60.0.1")));
        let v4 = Network::parse("10.60.0.0/24").unwrap();
        assert!(
            !v4.contains(ip("::a3c:1")),
            "an IPv4 network holds no IPv6 address"
        );
        let wide = Network::parse("fd49::/64").unwrap();
        assert!(wide.is_host(ip("fd49::ffff:ffff:ffff:fffe")));
        assert!(!wide.is_host(ip("fd49::ffff:ffff:ffff:ffff")));
        assert!(!wide.contains(ip("fd49:0:0:1::1")));
    }

    #[test]
    fn ipv6_allocation_starts_at_host_one_and_is_sticky() {
        let (b, _d) = book(Some("fd49:89c1:4b5e:60::/64"));
        let a = b.claim(Role::Origin, "o1", None, 1).unwrap().address;
        assert_eq!(a, ip("fd49:89c1:4b5e:60::1"));
        assert_eq!(
            b.claim(Role::Proxy, "p1", None, 1).unwrap().address,
            ip("fd49:89c1:4b5e:60::2")
        );
        assert_eq!(b.claim(Role::Origin, "o1", None, 2).unwrap().address, a);
    }

    #[test]
    fn ipv6_allocation_reports_exhaustion() {
        let (b, _d) = book(Some("fd49::/120"));
        for i in 0..254 {
            b.claim(Role::Origin, &format!("o{i}"), None, 1).unwrap();
        }
        assert!(matches!(
            b.claim(Role::Origin, "late", None, 1),
            Err(ClaimError::Rejected(Rejection::Exhausted {
                capacity: 254,
                ..
            }))
        ));
    }

    #[test]
    fn a_pin_of_the_other_family_is_outside_the_network() {
        let (b, _d) = book(Some("fd49::/64"));
        assert!(matches!(
            b.claim(Role::Origin, "o", Some(ip("10.60.0.1")), 1),
            Err(ClaimError::Rejected(Rejection::OutsideNetwork { .. }))
        ));
        b.claim(Role::Origin, "o", Some(ip("fd49::9")), 1).unwrap();
        let (b, _d) = book(Some("10.60.0.0/24"));
        assert!(matches!(
            b.claim(Role::Origin, "o", Some(ip("fd49::9")), 1),
            Err(ClaimError::Rejected(Rejection::OutsideNetwork { .. }))
        ));
    }

    #[test]
    fn ipv6_pins_compare_as_addresses_not_text() {
        let (b, _d) = book(Some("fd49::/64"));
        b.claim(Role::Origin, "o", Some(ip("fd49:0:0::5")), 1)
            .unwrap();
        // Same address, different spelling: the owner keeps it.
        b.claim(Role::Origin, "o", Some(ip("fd49::5")), 2).unwrap();
        assert!(matches!(
            b.claim(Role::Proxy, "p", Some(ip("fd49:0::5")), 1),
            Err(ClaimError::Rejected(Rejection::Held { .. }))
        ));
    }

    #[test]
    fn pin_only_mode_accepts_ipv6_but_refuses_reserved_ipv6_hosts() {
        let (b, _d) = book(None);
        b.claim(Role::Origin, "o", Some(ip("fd49::5")), 1).unwrap();
        b.claim(Role::Origin, "o4", Some(ip("10.60.0.5")), 1)
            .unwrap();
        for bad in ["::", "::1", "ff02::1", "fe80::1", "febf::1"] {
            assert_eq!(
                b.claim(Role::Proxy, "p", Some(ip(bad)), 1),
                Err(ClaimError::Rejected(Rejection::NotHost(ip(bad)))),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_database_written_by_the_ipv4_only_code_reopens_unchanged() {
        // Records exactly as the IPv4-only book wrote them: JSON assignment
        // under "role/name", and the address's 4 raw octets as the reverse key.
        let dir = tempfile::tempdir().unwrap();
        {
            let db = sled::open(dir.path()).unwrap();
            let owner = db.open_tree("by_owner").unwrap();
            let addr = db.open_tree("by_address").unwrap();
            owner
                .insert(
                    "origin/home",
                    &br#"{"address":"10.60.0.1","first_seen":3,"last_seen":4}"#[..],
                )
                .unwrap();
            addr.insert([10u8, 60, 0, 1], "origin/home").unwrap();
            db.flush().unwrap();
        }
        let b = reopen(dir.path(), Some("10.60.0.0/24"));
        let a = b.get(Role::Origin, "home").unwrap().unwrap();
        assert_eq!(a.address, ip("10.60.0.1"));
        assert_eq!((a.first_seen, a.last_seen), (3, 4));
        assert!(matches!(
            b.claim(Role::Proxy, "p", Some(ip("10.60.0.1")), 5),
            Err(ClaimError::Rejected(Rejection::Held { .. }))
        ));
        assert_eq!(
            b.claim(Role::Proxy, "p", None, 5).unwrap().address,
            ip("10.60.0.2")
        );
        assert_eq!(
            b.release(Role::Origin, "home").unwrap(),
            Some(ip("10.60.0.1"))
        );
        assert_eq!(
            b.claim(Role::Origin, "next", None, 6).unwrap().address,
            ip("10.60.0.1")
        );
    }

    #[test]
    fn concurrent_ipv6_registrations_never_share_an_address() {
        let (b, _d) = book(Some("fd49::/64"));
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
        let mut got: Vec<IpAddr> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        got.sort();
        got.dedup();
        assert_eq!(got.len(), 64);
    }

    #[test]
    fn expand_backends_uses_the_bracketed_form_for_an_ipv6_registrant() {
        let own = ip("fd49::5");
        let got = expand_backends(&[":25565".into(), "[fd49:0::5]:25566".into()], own).unwrap();
        assert_eq!(got, vec!["[fd49::5]:25565", "[fd49::5]:25566"]);
        let err = expand_backends(&["10.60.0.5:1".into()], own).unwrap_err();
        assert!(err.contains("registrant's own address"), "{err}");
        assert!(expand_backends(&["[fd49::6]:1".into()], own).is_err());
    }

    #[test]
    fn ipv4_written_as_ipv6_is_refused_for_networks_and_pins() {
        for bad in ["::/112", "::/64", "::1:0/112", "ff02::/64", "fe80::/64"] {
            assert!(Network::parse(bad).is_err(), "{bad}");
        }
        let err = Network::parse("::ffff:10.60.0.0/120").unwrap_err();
        assert!(err.contains("IPv4"), "{err}");
        assert!(Network::parse("::10.60.0.0/120").is_err());
        let (b, _d) = book(None);
        for bad in ["::ffff:10.60.0.5", "::10.60.0.5"] {
            assert_eq!(
                b.claim(Role::Origin, "o", Some(ip(bad)), 1),
                Err(ClaimError::Rejected(Rejection::NotHost(ip(bad)))),
                "{bad}"
            );
        }
        assert!(!is_ipv4_in_ipv6(ip("::1")));
        assert!(!is_ipv4_in_ipv6(ip("fd49::5")));
    }

    /// A /16 book with origin "a" at 10.60.0.1 and proxy "p" at 10.60.0.2,
    /// reopened on `network` with readdress `on`.
    fn moved(network: &str, on: bool) -> (AddressBook, tempfile::TempDir) {
        let (b, d) = book(Some("10.60.0.0/16"));
        b.claim(Role::Origin, "a", None, 1).unwrap();
        b.claim(Role::Proxy, "p", None, 1).unwrap();
        drop(b);
        (reopen(d.path(), Some(network)).with_readdress(on), d)
    }

    #[test]
    fn entries_outside_a_changed_network_refuse_startup_unless_readdress() {
        let (b, _d) = moved("fd49::/64", false);
        let e = check_network_change(&b, false).unwrap_err();
        assert!(
            e.starts_with("--tunnel-network fd49::/64 does not contain 2 stored"),
            "{e}"
        );
        assert!(e.contains("--tunnel-readdress"), "{e}");
        assert!(e.contains("origin a 10.60.0.1"), "{e}");
        assert!(e.contains("proxy p 10.60.0.2"), "{e}");
        assert_eq!(check_network_change(&b, true).unwrap().len(), 2);
    }

    #[test]
    fn an_unchanged_network_or_pin_only_mode_passes_the_check() {
        let (b, d) = book(Some("10.60.0.0/16"));
        b.claim(Role::Origin, "a", None, 1).unwrap();
        assert!(check_network_change(&b, false).unwrap().is_empty());
        drop(b);
        let b = reopen(d.path(), None);
        assert!(check_network_change(&b, false).unwrap().is_empty());
    }

    #[test]
    fn a_readdressed_owner_gets_a_fresh_address_and_frees_the_old_one() {
        let (b, _d) = moved("fd49::/64", true);
        let a = b.claim(Role::Origin, "a", None, 5).unwrap();
        assert_eq!(a.address, ip("fd49::1"));
        assert_eq!(a.first_seen, 5);
        assert!(b.by_address.contains_key(addr_key(ip("fd49::1"))).unwrap());
        assert!(!b
            .by_address
            .contains_key(addr_key(ip("10.60.0.1")))
            .unwrap());
        // A stale pin of the old network is refused, not silently granted.
        assert!(matches!(
            b.claim(Role::Proxy, "p", Some(ip("10.60.0.2")), 6),
            Err(ClaimError::Rejected(Rejection::OutsideNetwork { .. }))
        ));
        // Re-registering keeps the new address.
        assert_eq!(
            b.claim(Role::Origin, "a", None, 7).unwrap().address,
            ip("fd49::1")
        );
        assert_eq!(b.outside_network().unwrap().len(), 1, "only p is left");
    }

    #[test]
    fn without_readdress_an_out_of_network_owner_stays_sticky() {
        let (b, _d) = moved("fd49::/64", false);
        assert_eq!(
            b.claim(Role::Origin, "a", None, 5).unwrap().address,
            ip("10.60.0.1")
        );
    }

    #[test]
    fn shrinking_the_network_readdresses_only_entries_outside_it() {
        let (b, d) = book(Some("10.60.0.0/16"));
        b.claim(Role::Origin, "in", None, 1).unwrap();
        b.claim(Role::Origin, "out", Some(ip("10.60.1.5")), 1)
            .unwrap();
        drop(b);
        let b = reopen(d.path(), Some("10.60.0.0/24")).with_readdress(true);
        let out = b.outside_network().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "out");
        assert_eq!(
            b.claim(Role::Origin, "in", None, 2).unwrap().address,
            ip("10.60.0.1")
        );
        assert_eq!(
            b.claim(Role::Origin, "out", None, 2).unwrap().address,
            ip("10.60.0.2")
        );
    }

    #[test]
    fn resolve_flags_refuses_readdress_without_a_network() {
        let e = resolve_flags(None, "14d", false, true).unwrap_err();
        assert!(e.contains("--tunnel-readdress"), "{e}");
        assert!(resolve_flags(Some("fd49::/64"), "14d", false, true).is_ok());
    }

    #[test]
    fn tunnel_readdress_with_ha_is_refused() {
        let e = resolve_flags(Some("fd49::/64"), "14d", true, true).unwrap_err();
        assert!(
            e.contains("--tunnel-readdress") && e.contains("--ha-peers"),
            "{e}"
        );
    }

    #[test]
    fn resolve_flags_accepts_an_ipv6_network() {
        let (net, _) = resolve_flags(Some("fd49:89c1:4b5e:60::/64"), "14d", false, false).unwrap();
        assert_eq!(net.unwrap().to_string(), "fd49:89c1:4b5e:60::/64");
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

    #[test]
    fn parse_duration_refuses_an_overflowing_value_instead_of_panicking() {
        // u64::MAX / 86400 ≈ 2.1e14, so this many days does not fit in seconds.
        let err = parse_duration("300000000000000d").unwrap_err();
        assert!(err.contains("too large"), "{err}");
        assert!(parse_duration("6000000000000000h").is_err());
        assert!(parse_duration("400000000000000000m").is_err());
    }

    #[test]
    fn resolve_flags_defaults_to_pin_only_and_fourteen_days() {
        let (net, stale) = resolve_flags(None, "14d", false, false).unwrap();
        assert!(net.is_none());
        assert_eq!(stale, Duration::from_secs(14 * 86400));
    }

    #[test]
    fn resolve_flags_parses_the_network_and_the_duration() {
        let (net, stale) = resolve_flags(Some("10.60.0.0/16"), "0", false, false).unwrap();
        assert_eq!(net.unwrap().to_string(), "10.60.0.0/16");
        assert!(stale.is_zero());
    }

    #[test]
    fn resolve_flags_rejects_a_bad_network_and_a_bad_duration() {
        assert!(resolve_flags(Some("nonsense"), "14d", false, false).is_err());
        assert!(resolve_flags(None, "soon", false, false).is_err());
    }

    #[test]
    fn resolve_flags_accepts_a_network_under_ha() {
        let (net, _) = resolve_flags(Some("10.60.0.0/16"), "14d", true, false).unwrap();
        assert_eq!(net, Some(Network::parse("10.60.0.0/16").unwrap()));
        assert!(resolve_flags(None, "14d", true, false).is_ok());
    }

    fn net24() -> Network {
        Network::parse("10.60.0.0/24").unwrap()
    }

    #[test]
    fn a_rejection_advances_the_applied_index() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim_at(
            Role::Origin,
            "a",
            Some(ip("10.60.0.2")),
            1,
            Some(net24()),
            4,
        )
        .unwrap();
        let out = b
            .claim_at(
                Role::Origin,
                "b",
                Some(ip("10.60.0.2")),
                2,
                Some(net24()),
                5,
            )
            .unwrap();
        let held = Rejection::Held {
            address: ip("10.60.0.2"),
            role: Role::Origin,
            name: "a".into(),
        };
        assert_eq!(out, Outcome::Rejected(held.clone()));
        assert_eq!(b.applied_index().unwrap(), Some(5));
        assert_eq!(
            b.last_outcome().unwrap(),
            Some((5, Outcome::Rejected(held)))
        );
        // The rejection wrote nothing else.
        assert_eq!(b.get(Role::Origin, "b").unwrap(), None);
        assert_eq!(b.allocated(), 1);
    }

    #[test]
    fn a_replayed_rejection_is_not_re_evaluated() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim_at(
            Role::Origin,
            "a",
            Some(ip("10.60.0.2")),
            1,
            Some(net24()),
            4,
        )
        .unwrap();
        b.claim_at(
            Role::Origin,
            "b",
            Some(ip("10.60.0.2")),
            2,
            Some(net24()),
            5,
        )
        .unwrap();
        assert_eq!(
            b.release_at(Role::Origin, "a", 6).unwrap(),
            Some(Some(ip("10.60.0.2")))
        );
        // Replaying index 5 must not now grant `.2` to b.
        let replay = b
            .claim_at(
                Role::Origin,
                "b",
                Some(ip("10.60.0.2")),
                2,
                Some(net24()),
                5,
            )
            .unwrap();
        assert_eq!(replay, Outcome::AlreadyApplied);
        assert_eq!(b.get(Role::Origin, "b").unwrap(), None);
        assert_eq!(b.release_at(Role::Origin, "a", 6).unwrap(), None);
        assert_eq!(b.applied_index().unwrap(), Some(6));
        // last_outcome still describes the last claim, with its own index.
        assert_eq!(b.last_outcome().unwrap().unwrap().0, 5);
    }

    #[test]
    fn claim_uses_the_given_network_not_the_opened_one() {
        let (b, _d) = book(None);
        let out = b
            .claim_at(Role::Origin, "a", None, 7, Some(net24()), 1)
            .unwrap();
        assert_eq!(
            out,
            Outcome::Granted(Assignment {
                address: ip("10.60.0.1"),
                first_seen: 7,
                last_seen: 7
            })
        );
        // And the other way: given no network, an allocation is refused even
        // though the book was opened with one.
        let (b, _d) = book(Some("10.60.0.0/24"));
        assert_eq!(
            b.claim_at(Role::Origin, "a", None, 7, None, 1).unwrap(),
            Outcome::Rejected(Rejection::NoNetwork)
        );
    }

    #[test]
    fn claim_at_ignores_the_readdress_flag() {
        let dir = tempfile::tempdir().unwrap();
        let old = Network::parse("10.50.0.0/24").unwrap();
        let b = AddressBook::open(dir.path(), Some(old))
            .unwrap()
            .with_readdress(true);
        b.claim_at(Role::Origin, "a", None, 1, Some(old), 1)
            .unwrap();
        // The owner sits outside the new network; claim_at keeps its address.
        let out = b
            .claim_at(Role::Origin, "a", None, 2, Some(net24()), 2)
            .unwrap();
        let Outcome::Granted(a) = out else {
            panic!("{out:?}")
        };
        assert_eq!(a.address, ip("10.50.0.1"));
    }

    #[test]
    fn touch_changes_last_seen_only() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim_at(Role::Proxy, "p", None, 10, Some(net24()), 1)
            .unwrap();
        let before = b.last_outcome().unwrap();
        assert_eq!(b.touch_at(Role::Proxy, "p", 99, 2).unwrap(), Some(true));
        assert_eq!(
            b.get(Role::Proxy, "p").unwrap(),
            Some(Assignment {
                address: ip("10.60.0.1"),
                first_seen: 10,
                last_seen: 99
            })
        );
        assert_eq!(b.applied_index().unwrap(), Some(2));
        assert_eq!(b.last_outcome().unwrap(), before);
        assert_eq!(b.touch_at(Role::Proxy, "p", 123, 2).unwrap(), None);
        assert_eq!(b.get(Role::Proxy, "p").unwrap().unwrap().last_seen, 99);
        // An unknown owner advances the index and nothing else.
        assert_eq!(b.touch_at(Role::Proxy, "x", 5, 3).unwrap(), Some(false));
        assert_eq!(b.applied_index().unwrap(), Some(3));
        assert_eq!(b.allocated(), 1);
    }

    #[test]
    fn the_applied_index_and_outcome_survive_a_reopen() {
        let (b, d) = book(Some("10.60.0.0/24"));
        b.claim_at(Role::Origin, "a", None, 1, Some(net24()), 9)
            .unwrap();
        drop(b);
        let b = reopen(d.path(), Some("10.60.0.0/24"));
        assert_eq!(b.applied_index().unwrap(), Some(9));
        assert!(matches!(
            b.last_outcome().unwrap(),
            Some((9, Outcome::Granted(_)))
        ));
    }

    #[test]
    fn the_non_indexed_paths_write_nothing_to_meta() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim(Role::Origin, "a", None, 1).unwrap();
        assert!(b
            .claim(Role::Origin, "b", Some(ip("10.60.0.1")), 1)
            .is_err());
        b.release(Role::Origin, "a").unwrap();
        assert_eq!(b.applied_index().unwrap(), None);
        assert_eq!(b.last_outcome().unwrap(), None);
    }

    #[test]
    fn json_formats_carry_addresses_as_strings() {
        let a = Assignment {
            address: ip("fd49::5"),
            first_seen: 1,
            last_seen: 2,
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"address":"fd49::5","first_seen":1,"last_seen":2}"#
        );
        let r = Rejection::OutsideNetwork {
            address: ip("10.61.0.1"),
            network: Network::parse("fd49:89c1:4b5e:60::/64").unwrap(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            json.contains(r#""network":"fd49:89c1:4b5e:60::/64""#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Rejection>(&json).unwrap(), r);
    }

    #[test]
    fn release_at_of_an_unknown_owner_advances_the_applied_index() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        assert_eq!(b.release_at(Role::Origin, "nobody", 4).unwrap(), Some(None));
        assert_eq!(b.applied_index().unwrap(), Some(4));
        assert_eq!(b.last_outcome().unwrap(), None);
        assert_eq!(b.release_at(Role::Origin, "nobody", 4).unwrap(), None);
    }

    #[test]
    fn reject_at_records_the_outcome_and_skips_a_replay() {
        let (b, _d) = book(None);
        assert_eq!(
            b.reject_at(Rejection::NotInitialized, 3).unwrap(),
            Outcome::Rejected(Rejection::NotInitialized)
        );
        assert_eq!(b.applied_index().unwrap(), Some(3));
        assert_eq!(
            b.last_outcome().unwrap(),
            Some((3, Outcome::Rejected(Rejection::NotInitialized)))
        );
        assert_eq!(
            b.reject_at(Rejection::NoNetwork, 3).unwrap(),
            Outcome::AlreadyApplied
        );
        assert!(b.mark_applied(4).unwrap());
        assert!(!b.mark_applied(4).unwrap());
        assert_eq!(b.applied_index().unwrap(), Some(4));
        assert!(b.entries().unwrap().is_empty());
    }

    #[test]
    fn replace_installs_a_snapshot_over_a_non_empty_book() {
        let net = Network::parse("10.60.0.0/24").unwrap();
        let (src, _s) = book(None);
        src.claim_at(Role::Origin, "a", None, 7, Some(net), 1)
            .unwrap();
        src.claim_at(Role::Proxy, "p", Some(ip("10.60.0.9")), 8, Some(net), 2)
            .unwrap();
        let snapshot = src.snapshot().unwrap();
        assert_eq!(snapshot.entries.len(), 2);

        let (dst, _d) = book(None);
        dst.claim_at(
            Role::Origin,
            "stale",
            Some(ip("10.60.0.50")),
            1,
            Some(net),
            9,
        )
        .unwrap();
        dst.replace(&snapshot).unwrap();
        assert_eq!(dst.snapshot().unwrap(), snapshot);
        assert_eq!(dst.get(Role::Origin, "stale").unwrap(), None);
        // `by_address` was replaced too: the stale address is free again.
        assert!(matches!(
            dst.claim_at(Role::Origin, "x", Some(ip("10.60.0.50")), 1, Some(net), 3)
                .unwrap(),
            Outcome::Granted(_)
        ));
        assert!(matches!(
            dst.claim_at(Role::Origin, "y", Some(ip("10.60.0.9")), 1, Some(net), 4)
                .unwrap(),
            Outcome::Rejected(Rejection::Held { .. })
        ));

        // An empty snapshot empties the book and forgets its index.
        dst.replace(&BookSnapshot {
            entries: vec![],
            applied_index: None,
            last_outcome: None,
        })
        .unwrap();
        assert!(dst.entries().unwrap().is_empty());
        assert_eq!(dst.applied_index().unwrap(), None);
        assert_eq!(dst.last_outcome().unwrap(), None);
    }
}
