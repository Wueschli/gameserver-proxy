//! Tier-2 regional health fabric: SWIM cluster membership over `foca`
//! (docs/10 "Tier 2— regional health fabric", phase 13). Control-plane only
//! — spawned by `runtime.rs` when `settings.gossip` is set, never touches
//! the hot path.
//!
//! **Slice 2** (membership): every gossip datagram carries an HMAC-SHA256
//! tag computed with the domain's pre-shared key; a bad or missing tag is
//! dropped silently, same "malformed input is never trusted" posture as
//! `sniff.rs`.
//!
//! **Slice 3** (this slice): a per-backend last-writer-wins health register,
//! piggybacked on foca's own broadcast/anti-entropy (`BroadcastHandler`) —
//! [`GossipHandle::publish_backend_health`] lets a caller assert this
//! instance's own verdict for one backend, [`GossipHandle::quorum_down`]
//! reads back the merged domain view. Nothing calls either yet — wiring a
//! real caller (`health.rs`) and a real reader (`Backend::domain_down`) is
//! slice 4; this module only carries and merges whatever it's given.
//!
//! This instance's own gossip identity is its `settings.gossip.bind`
//! address. `foca`'s `Identity::renew` is left at its default (`None`, no
//! auto-rejoin identity bump) — a deliberate simplification for this slice;
//! revisit if a declared-down instance needs to rejoin faster than
//! `remove_down_after` allows.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use foca::{AccumulatingRuntime, Config as FocaConfig, Foca, Invalidates, PostcardCodec, Timer};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use wayhouse_config::GossipConfig;

use crate::metrics_defs as m;

type HmacSha256 = Hmac<Sha256>;
const HMAC_TAG_LEN: usize = 32;
/// Bytes of sender timestamp in front of every datagram's payload.
const TIMESTAMP_LEN: usize = 8;
/// Bytes `tag` adds around a foca packet: timestamp, version byte, HMAC tag.
const FRAME_OVERHEAD: usize = TIMESTAMP_LEN + 1 + HMAC_TAG_LEN;
/// The gossip wire version, the second field of every datagram. Bump it on an
/// incompatible gossip format change; it is independent of the HTTP protocol major
/// (`wayhouse_http::protocol`, #185). A datagram carrying another value is dropped
/// before its payload is decoded.
const GOSSIP_VERSION: u8 = 1;
/// How far a datagram's sender timestamp may sit from our clock before it is
/// dropped as a replay (security review O4). Also the clock skew the mesh
/// tolerates between instances; a replay inside the window is still possible.
const MAX_DATAGRAM_AGE: std::time::Duration = std::time::Duration::from_secs(30);

/// One instance's asserted health for one backend, gossiped as a
/// last-writer-wins register (`Invalidates` below): a newer `changed_at`
/// from the same `origin` about the same `addr` replaces the old one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct BackendHealthRegister {
    addr: SocketAddr,
    up: bool,
    changed_at: u64,
    origin: SocketAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct BackendHealthKey {
    addr: SocketAddr,
    changed_at: u64,
}

impl Invalidates for BackendHealthKey {
    fn invalidates(&self, other: &Self) -> bool {
        self.addr == other.addr && self.changed_at > other.changed_at
    }
}

/// backend addr -> (origin instance -> its current register). One entry per
/// `(backend, origin)` pair ever seen; there is no expiry yet (a departed
/// instance's last-published register lingers) — an accepted simplification
/// for this slice, matching this codebase's "start simple" bias elsewhere.
type DomainMap = Arc<Mutex<HashMap<SocketAddr, HashMap<SocketAddr, BackendHealthRegister>>>>;

struct BroadcastMerger {
    domain: DomainMap,
}

impl foca::BroadcastHandler<SocketAddr> for BroadcastMerger {
    type Key = BackendHealthKey;
    type Error = postcard::Error;

    fn receive_item(
        &mut self,
        data: &[u8],
        _sender: Option<&SocketAddr>,
    ) -> Result<Option<Self::Key>, Self::Error> {
        let reg: BackendHealthRegister = postcard::from_bytes(data)?;
        let key = BackendHealthKey {
            addr: reg.addr,
            changed_at: reg.changed_at,
        };
        let mut domain = self.domain.lock().unwrap_or_else(PoisonError::into_inner);
        let per_origin = domain.entry(reg.addr).or_default();
        let is_new = per_origin
            .get(&reg.origin)
            .is_none_or(|cur| cur.changed_at < reg.changed_at);
        if is_new {
            per_origin.insert(reg.origin, reg);
            Ok(Some(key))
        } else {
            Ok(None)
        }
    }
}

/// Live gossip state, cheaply cloneable, read by (future slice 4) health
/// integration and by tests. Updated only by [`run`].
#[derive(Clone)]
pub struct GossipHandle {
    members: Arc<AtomicUsize>,
    domain: DomainMap,
    publish_tx: mpsc::UnboundedSender<(SocketAddr, bool)>,
}

/// The receiving half of [`GossipHandle::publish_backend_health`], held only
/// by [`run`]. Split out (rather than folding into `GossipHandle` itself) so
/// `GossipHandle` stays freely cloneable while only one task ever consumes
/// publish requests.
pub struct GossipInbox(mpsc::UnboundedReceiver<(SocketAddr, bool)>);

/// Everything `health.rs` needs to talk to the Tier-2 mesh: a handle plus
/// this domain's configured quorum fraction (`settings.gossip.
/// quorum_fraction`) — bundled so a caller doesn't have to thread the two
/// separately. `None` (no `settings.gossip`) means "fabric disabled",
/// exactly today's local-only behaviour.
#[derive(Clone)]
pub struct GossipFabric {
    pub handle: GossipHandle,
    pub quorum_fraction: f64,
}

impl GossipHandle {
    pub fn new() -> (Self, GossipInbox) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                members: Arc::new(AtomicUsize::new(0)),
                domain: Arc::new(Mutex::new(HashMap::new())),
                publish_tx: tx,
            },
            GossipInbox(rx),
        )
    }

    pub fn member_count(&self) -> usize {
        self.members.load(Ordering::Relaxed)
    }

    /// Assert this instance's own current health verdict for `addr` into the
    /// mesh. A no-op if the mesh task has already stopped (the channel is
    /// closed only once `run` returns, and `run` holds a `GossipHandle` of
    /// its own for its whole lifetime, so this never happens while the mesh
    /// is actually up).
    pub fn publish_backend_health(&self, addr: SocketAddr, up: bool) {
        let _ = self.publish_tx.send((addr, up));
    }

    /// How many known instances currently assert `addr` up vs. down —
    /// `(up_votes, down_votes)`. `(0, 0)` if nobody (including this
    /// instance) has ever published about it.
    pub fn domain_votes(&self, addr: SocketAddr) -> (usize, usize) {
        let domain = self.domain.lock().unwrap_or_else(PoisonError::into_inner);
        match domain.get(&addr) {
            None => (0, 0),
            Some(regs) => {
                let down = regs.values().filter(|r| !r.up).count();
                (regs.len() - down, down)
            }
        }
    }

    /// Whether at least `quorum_fraction` of the instances that have voted on
    /// `addr` currently say it's down. `false` when nobody has voted — an
    /// empty view means "fall back to today's local-only behaviour", never
    /// "assume down" (docs/10 "Tier 2", "fully rebuildable").
    pub fn quorum_down(&self, addr: SocketAddr, quorum_fraction: f64) -> bool {
        let (up, down) = self.domain_votes(addr);
        let total = up + down;
        total > 0 && (down as f64 / total as f64) >= quorum_fraction
    }
}

/// Spawns the mesh task for `cfg` and returns the fabric `health.rs` talks to
/// plus the task's handle. `runtime.rs` calls it for `settings.gossip`; the
/// stand-in in `gossip_disabled.rs` returns `None`.
#[allow(clippy::unnecessary_wraps)] // same signature as the disabled stand-in
pub fn spawn(
    cfg: GossipConfig,
    shutdown: watch::Receiver<bool>,
) -> Option<(GossipFabric, tokio::task::JoinHandle<()>)> {
    let (handle, inbox) = GossipHandle::new();
    let fabric = GossipFabric {
        handle: handle.clone(),
        quorum_fraction: cfg.quorum_fraction,
    };
    let task = tokio::spawn(run(cfg, handle, inbox, shutdown));
    Some((fabric, task))
}

type FocaInstance = Foca<SocketAddr, PostcardCodec, rand10::rngs::StdRng, BroadcastMerger>;

/// Runs the gossip mesh until `shutdown` fires. Binds `cfg.bind`, announces
/// to every configured seed, then services membership traffic, SWIM timers,
/// and publish requests off one task — no separate scheduler task or extra
/// channel plumbing beyond `inbox` (control-plane, human/domain-paced,
/// matching `health.rs`'s own single-task-loop shape).
pub async fn run(
    cfg: GossipConfig,
    handle: GossipHandle,
    mut inbox: GossipInbox,
    mut shutdown: watch::Receiver<bool>,
) {
    let socket = match UdpSocket::bind(cfg.bind).await {
        Ok(s) => s,
        Err(error) => {
            tracing::error!(bind = %cfg.bind, %error, "gossip: failed to bind, mesh disabled");
            return;
        }
    };
    tracing::info!(bind = %cfg.bind, seeds = cfg.seeds.len(), "gossip mesh started");

    let identity: SocketAddr = cfg.bind;
    // `new_lan` assumes sub-millisecond RTTs between members; correct for a
    // single failure domain (an AZ / region / rack row by definition), never
    // meant to span a WAN hop.
    let foca_cfg = FocaConfig::new_lan(NonZeroU32::new(10).unwrap());
    let max_packet = foca_cfg.max_packet_size.get();
    let rng: rand10::rngs::StdRng = rand10::make_rng();
    let mut foca: FocaInstance = Foca::with_custom_broadcast(
        identity,
        foca_cfg,
        rng,
        PostcardCodec,
        BroadcastMerger {
            domain: handle.domain.clone(),
        },
    );
    let mut runtime = AccumulatingRuntime::new();
    let mut warned_version = false;
    let mut timers: BinaryHeap<Reverse<TimerEntry>> = BinaryHeap::new();
    let mut recv_buf = vec![0u8; max_packet + FRAME_OVERHEAD];

    for seed in &cfg.seeds {
        if let Err(error) = foca.announce(*seed, &mut runtime) {
            tracing::warn!(%seed, ?error, "gossip: announce failed");
        }
    }
    drain_to_wire(&mut runtime, &socket, &cfg.psk, &mut timers).await;
    publish_member_count(&foca, &handle);

    loop {
        let next_deadline = timers.peek().map(|Reverse(TimerEntry(at, _))| *at);
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("gossip mesh stopping");
                    return;
                }
            }
            _ = sleep_until_opt(next_deadline) => {
                if let Some(Reverse(TimerEntry(_, timer))) = timers.pop() {
                    if let Err(error) = foca.handle_timer(timer, &mut runtime) {
                        tracing::debug!(?error, "gossip: timer handling error");
                    }
                }
            }
            recv = socket.recv_from(&mut recv_buf) => {
                match recv {
                    Ok((len, from)) => match verify_and_strip(&cfg.psk, &recv_buf[..len], wall_ms()) {
                        Ok(payload) => {
                            metrics::counter!(m::GOSSIP_MESSAGES_TOTAL, "direction" => "received")
                                .increment(1);
                            if let Err(error) = foca.handle_data(payload, &mut runtime) {
                                tracing::debug!(?error, "gossip: bad datagram");
                            }
                        }
                        Err(Reject::Auth) => {
                            metrics::counter!(m::GOSSIP_AUTH_REJECTED_TOTAL).increment(1);
                        }
                        Err(Reject::Stale) => {
                            metrics::counter!(m::GOSSIP_STALE_REJECTED_TOTAL).increment(1);
                        }
                        Err(Reject::Version(theirs)) => {
                            // One warning, then debug: a peer on another version
                            // sends every second and the counter carries the rate.
                            let msg = "gossip: dropping a datagram with another protocol version: upgrade the older side";
                            if std::mem::replace(&mut warned_version, true) {
                                tracing::debug!(theirs, ours = GOSSIP_VERSION, %from, "{msg}");
                            } else {
                                tracing::warn!(theirs, ours = GOSSIP_VERSION, %from, "{msg}");
                            }
                            metrics::counter!(m::GOSSIP_VERSION_REJECTED_TOTAL).increment(1);
                        }
                    },
                    Err(error) => {
                        tracing::warn!(%error, "gossip: recv error");
                    }
                }
            }
            Some((addr, up)) = inbox.0.recv() => {
                let reg = BackendHealthRegister {
                    addr,
                    up,
                    changed_at: crate::util::mono_ms(),
                    origin: identity,
                };
                // `add_broadcast` itself runs `data` through
                // `BroadcastMerger::receive_item(data, None)` — the sender
                // is `None` for exactly this "adding it myself" case — so
                // it performs the domain-map merge; don't merge it here too,
                // or foca sees an already-known key and never actually
                // queues it for dissemination.
                match postcard::to_allocvec(&reg) {
                    Ok(data) => {
                        if let Err(error) = foca.add_broadcast(&data) {
                            tracing::debug!(?error, "gossip: add_broadcast failed");
                        }
                    }
                    Err(error) => {
                        tracing::debug!(?error, "gossip: failed to encode health register");
                    }
                }
            }
        }
        drain_to_wire(&mut runtime, &socket, &cfg.psk, &mut timers).await;
        publish_member_count(&foca, &handle);
    }
}

/// `Timer` doesn't implement `Ord` the way a `BinaryHeap` needs (it only
/// orders by content, for out-of-order-delivery correction within foca
/// itself) — wrap it with the `Instant` deadline as the real sort key.
struct TimerEntry(Instant, Timer<SocketAddr>);

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for TimerEntry {}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

fn publish_member_count(foca: &FocaInstance, handle: &GossipHandle) {
    let n = foca.num_members();
    handle.members.store(n, Ordering::Relaxed);
    metrics::gauge!(m::GOSSIP_MEMBERS).set(n as f64);
}

async fn drain_to_wire(
    runtime: &mut AccumulatingRuntime<SocketAddr>,
    socket: &UdpSocket,
    psk: &str,
    timers: &mut BinaryHeap<Reverse<TimerEntry>>,
) {
    while let Some((dst, data)) = runtime.to_send() {
        let tagged = tag(psk, &data, wall_ms());
        match socket.send_to(&tagged, dst).await {
            Ok(_) => {
                metrics::counter!(m::GOSSIP_MESSAGES_TOTAL, "direction" => "sent").increment(1);
            }
            Err(error) => {
                tracing::debug!(%dst, %error, "gossip: send failed");
            }
        }
    }
    let now = Instant::now();
    while let Some((delay, timer)) = runtime.to_schedule() {
        timers.push(Reverse(TimerEntry(now + delay, timer)));
    }
    // Notifications (MemberUp/MemberDown/etc.) aren't consumed yet — slice 4
    // wires them into `Backend::domain_down`. Still drained fully: foca
    // expects its backlog empty before the next `handle_*` call.
    while runtime.to_notify().is_some() {}
}

/// Wall-clock milliseconds since the Unix epoch — the sender timestamp every
/// datagram carries. (`mono_ms` is per-process, so it means nothing to a peer.)
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Frames `payload` as `timestamp (u64 BE ms) || version (u8) || payload ||
/// HMAC(timestamp || version || payload)`. The timestamp sits inside the MAC so
/// an on-path attacker can neither replay a captured datagram after
/// [`MAX_DATAGRAM_AGE`] nor refresh it; the version byte is inside it too.
fn tag(psk: &str, payload: &[u8], now_ms: u64) -> Vec<u8> {
    tag_with_version(psk, GOSSIP_VERSION, payload, now_ms)
}

fn tag_with_version(psk: &str, version: u8, payload: &[u8], now_ms: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(TIMESTAMP_LEN + 1 + payload.len() + HMAC_TAG_LEN);
    out.extend_from_slice(&now_ms.to_be_bytes());
    out.push(version);
    out.extend_from_slice(payload);
    let mut mac = <HmacSha256 as Mac>::new_from_slice(psk.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(&out);
    out.extend_from_slice(&mac.finalize().into_bytes());
    out
}

/// Why a datagram was dropped.
#[derive(Debug, PartialEq, Eq)]
enum Reject {
    /// Too short, or the HMAC tag does not verify.
    Auth,
    /// Authentic, but its timestamp is further than [`MAX_DATAGRAM_AGE`] from
    /// our clock (a replay, or peers with badly skewed clocks).
    Stale,
    /// Authentic and fresh, but built by a node speaking another gossip
    /// version (an upgrade in progress): not decoded.
    Version(u8),
}

fn verify_and_strip<'a>(psk: &str, datagram: &'a [u8], now_ms: u64) -> Result<&'a [u8], Reject> {
    if datagram.len() < FRAME_OVERHEAD {
        return Err(Reject::Auth);
    }
    let (signed, tag) = datagram.split_at(datagram.len() - HMAC_TAG_LEN);
    let mut mac = <HmacSha256 as Mac>::new_from_slice(psk.as_bytes()).map_err(|_| Reject::Auth)?;
    mac.update(signed);
    mac.verify_slice(tag).map_err(|_| Reject::Auth)?;
    let (ts, rest) = signed.split_at(TIMESTAMP_LEN);
    let sent_ms = u64::from_be_bytes(ts.try_into().expect("split_at(8) yields 8 bytes"));
    if now_ms.abs_diff(sent_ms) > MAX_DATAGRAM_AGE.as_millis() as u64 {
        return Err(Reject::Stale);
    }
    let (&version, payload) = rest.split_first().expect("length checked above");
    if version != GOSSIP_VERSION {
        return Err(Reject::Version(version));
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    #[test]
    fn tag_round_trips() {
        let tagged = tag("secret", b"hello", NOW);
        assert_eq!(verify_and_strip("secret", &tagged, NOW), Ok(&b"hello"[..]));
    }

    #[test]
    fn frame_round_trips_with_version_byte() {
        let tagged = tag("secret", b"hello", NOW);
        assert_eq!(tagged[TIMESTAMP_LEN], GOSSIP_VERSION);
        assert_eq!(tagged.len(), TIMESTAMP_LEN + 1 + 5 + HMAC_TAG_LEN);
        assert_eq!(verify_and_strip("secret", &tagged, NOW), Ok(&b"hello"[..]));
    }

    #[test]
    fn valid_mac_but_other_version_is_dropped_as_version() {
        let tagged = tag_with_version("secret", 9, b"hello", NOW);
        assert_eq!(
            verify_and_strip("secret", &tagged, NOW),
            Err(Reject::Version(9))
        );
    }

    #[test]
    fn postcard_is_not_parsed_for_other_version() {
        // Random bytes under a valid MAC and another version: dropped before
        // any decoding, so no panic and no `Ok` payload for the codec.
        let garbage: Vec<u8> = (0..200u32).map(|i| (i * 37 % 251) as u8).collect();
        let tagged = tag_with_version("secret", 0, &garbage, NOW);
        assert_eq!(
            verify_and_strip("secret", &tagged, NOW),
            Err(Reject::Version(0))
        );
    }

    #[test]
    fn a_forged_version_without_the_key_is_an_auth_failure() {
        let mut tagged = tag("secret", b"hello", NOW);
        tagged[TIMESTAMP_LEN] = 9;
        assert_eq!(verify_and_strip("secret", &tagged, NOW), Err(Reject::Auth));
    }

    #[test]
    fn wrong_psk_is_rejected() {
        let tagged = tag("secret", b"hello", NOW);
        assert_eq!(verify_and_strip("other", &tagged, NOW), Err(Reject::Auth));
    }

    #[test]
    fn truncated_datagram_is_rejected() {
        assert_eq!(verify_and_strip("secret", b"short", NOW), Err(Reject::Auth));
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let mut tagged = tag("secret", b"hello", NOW);
        tagged[TIMESTAMP_LEN + 1] ^= 0xff;
        assert_eq!(verify_and_strip("secret", &tagged, NOW), Err(Reject::Auth));
    }

    #[test]
    fn a_rewritten_timestamp_is_rejected() {
        // Refreshing a captured datagram's timestamp must break the MAC.
        let old = NOW - 10 * MAX_DATAGRAM_AGE.as_millis() as u64;
        let mut tagged = tag("secret", b"hello", old);
        tagged[..TIMESTAMP_LEN].copy_from_slice(&NOW.to_be_bytes());
        assert_eq!(verify_and_strip("secret", &tagged, NOW), Err(Reject::Auth));
    }

    #[test]
    fn a_replayed_datagram_outside_the_window_is_rejected() {
        let tagged = tag("secret", b"hello", NOW);
        let later = NOW + MAX_DATAGRAM_AGE.as_millis() as u64 + 1;
        assert_eq!(
            verify_and_strip("secret", &tagged, later),
            Err(Reject::Stale)
        );
        // A sender whose clock runs ahead is just as stale.
        let earlier = NOW - MAX_DATAGRAM_AGE.as_millis() as u64 - 1;
        assert_eq!(
            verify_and_strip("secret", &tagged, earlier),
            Err(Reject::Stale)
        );
    }

    #[test]
    fn a_datagram_at_the_edge_of_the_window_is_accepted() {
        let tagged = tag("secret", b"hello", NOW);
        let edge = NOW + MAX_DATAGRAM_AGE.as_millis() as u64;
        assert_eq!(verify_and_strip("secret", &tagged, edge), Ok(&b"hello"[..]));
    }

    #[tokio::test]
    async fn two_instances_discover_each_other() {
        let psk = "test-psk".to_string();
        let bind_a: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let bind_b: SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Bind real sockets first so we know the ephemeral ports before
        // building each instance's `GossipConfig` (seeds need a real addr).
        let sock_a = tokio::net::UdpSocket::bind(bind_a).await.unwrap();
        let addr_a = sock_a.local_addr().unwrap();
        let sock_b = tokio::net::UdpSocket::bind(bind_b).await.unwrap();
        let addr_b = sock_b.local_addr().unwrap();
        drop(sock_a);
        drop(sock_b);

        let cfg_a = GossipConfig {
            bind: addr_a,
            seeds: vec![addr_b],
            quorum_fraction: 0.66,
            psk: psk.clone(),
        };
        let cfg_b = GossipConfig {
            bind: addr_b,
            seeds: vec![],
            quorum_fraction: 0.66,
            psk,
        };

        let (handle_a, inbox_a) = GossipHandle::new();
        let (handle_b, inbox_b) = GossipHandle::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let ha = handle_a.clone();
        let ta = tokio::spawn(run(cfg_a, ha, inbox_a, shutdown_rx.clone()));
        let hb = handle_b.clone();
        let tb = tokio::spawn(run(cfg_b, hb, inbox_b, shutdown_rx.clone()));

        // Membership converges within a couple of probe periods.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if handle_a.member_count() >= 1 && handle_b.member_count() >= 1 {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!(
                    "gossip never converged: a={} b={}",
                    handle_a.member_count(),
                    handle_b.member_count()
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let _ = shutdown_tx.send(true);
        let _ = ta.await;
        let _ = tb.await;
    }

    #[tokio::test]
    async fn wrong_psk_never_joins() {
        let bind_a: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let bind_b: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let sock_a = tokio::net::UdpSocket::bind(bind_a).await.unwrap();
        let addr_a = sock_a.local_addr().unwrap();
        let sock_b = tokio::net::UdpSocket::bind(bind_b).await.unwrap();
        let addr_b = sock_b.local_addr().unwrap();
        drop(sock_a);
        drop(sock_b);

        let cfg_a = GossipConfig {
            bind: addr_a,
            seeds: vec![addr_b],
            quorum_fraction: 0.66,
            psk: "psk-a".to_string(),
        };
        let cfg_b = GossipConfig {
            bind: addr_b,
            seeds: vec![],
            quorum_fraction: 0.66,
            psk: "psk-b".to_string(),
        };

        let (handle_a, inbox_a) = GossipHandle::new();
        let (handle_b, inbox_b) = GossipHandle::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let ta = tokio::spawn(run(cfg_a, handle_a.clone(), inbox_a, shutdown_rx.clone()));
        let tb = tokio::spawn(run(cfg_b, handle_b.clone(), inbox_b, shutdown_rx.clone()));

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(handle_a.member_count(), 0);
        assert_eq!(handle_b.member_count(), 0);

        let _ = shutdown_tx.send(true);
        let _ = ta.await;
        let _ = tb.await;
    }

    #[test]
    fn quorum_down_is_false_with_no_votes() {
        let (handle, _inbox) = GossipHandle::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        assert_eq!(handle.domain_votes(addr), (0, 0));
        assert!(!handle.quorum_down(addr, 0.66));
    }

    #[test]
    fn quorum_down_reflects_the_vote_fraction() {
        let (handle, _inbox) = GossipHandle::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let voter = |n: u16| -> SocketAddr { format!("10.0.0.{n}:1").parse().unwrap() };

        let mut domain = handle.domain.lock().unwrap_or_else(PoisonError::into_inner);
        domain.entry(addr).or_default().insert(
            voter(2),
            BackendHealthRegister {
                addr,
                up: false,
                changed_at: 1,
                origin: voter(2),
            },
        );
        domain.entry(addr).or_default().insert(
            voter(3),
            BackendHealthRegister {
                addr,
                up: true,
                changed_at: 1,
                origin: voter(3),
            },
        );
        drop(domain);

        // 1/2 down: below a 0.66 quorum, at/above a 0.5 one.
        assert_eq!(handle.domain_votes(addr), (1, 1));
        assert!(!handle.quorum_down(addr, 0.66));
        assert!(handle.quorum_down(addr, 0.5));
    }

    #[tokio::test]
    async fn published_health_reaches_the_other_instance() {
        let psk = "test-psk".to_string();
        let sock_a = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr_a = sock_a.local_addr().unwrap();
        let sock_b = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr_b = sock_b.local_addr().unwrap();
        drop(sock_a);
        drop(sock_b);

        let cfg_a = GossipConfig {
            bind: addr_a,
            seeds: vec![addr_b],
            quorum_fraction: 0.66,
            psk: psk.clone(),
        };
        let cfg_b = GossipConfig {
            bind: addr_b,
            seeds: vec![],
            quorum_fraction: 0.66,
            psk,
        };

        let (handle_a, inbox_a) = GossipHandle::new();
        let (handle_b, inbox_b) = GossipHandle::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let ta = tokio::spawn(run(cfg_a, handle_a.clone(), inbox_a, shutdown_rx.clone()));
        let tb = tokio::spawn(run(cfg_b, handle_b.clone(), inbox_b, shutdown_rx.clone()));

        // Wait for membership first, then assert A's own opinion about a
        // backend it "checks" propagates to B via foca's own anti-entropy.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while handle_a.member_count() < 1 || handle_b.member_count() < 1 {
            if tokio::time::Instant::now() > deadline {
                panic!("gossip never converged");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let backend: SocketAddr = "10.0.0.9:9999".parse().unwrap();
        handle_a.publish_backend_health(backend, false);

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if handle_b.domain_votes(backend) == (0, 1) {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!(
                    "backend health never propagated: b's votes = {:?}",
                    handle_b.domain_votes(backend)
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(handle_b.quorum_down(backend, 0.66));
        // A's own view already has its own vote, before any round trip.
        assert_eq!(handle_a.domain_votes(backend), (0, 1));

        let _ = shutdown_tx.send(true);
        let _ = ta.await;
        let _ = tb.await;
    }
}
