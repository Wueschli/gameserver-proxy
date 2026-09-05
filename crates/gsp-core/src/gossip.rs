//! Tier-2 regional health fabric: SWIM cluster membership over `foca`
//! (docs/10 "Tier 2— regional health fabric", phase 13). Control-plane only
//! — spawned by `runtime.rs` when `settings.gossip` is set, never touches
//! the hot path.
//!
//! **Slice 2 scope**: membership only. Every gossip datagram carries an
//! HMAC-SHA256 tag computed with the domain's pre-shared key; a bad or
//! missing tag is dropped silently, same "malformed input is never trusted"
//! posture as `sniff.rs`. The per-backend health broadcast (a last-writer-
//! wins register piggybacked on foca's own anti-entropy) and the
//! `Backend::domain_down` integration are slices 3 and 4 — this module does
//! not touch `pool.rs` yet.
//!
//! This instance's own gossip identity is its `settings.gossip.bind`
//! address. `foca`'s `Identity::renew` is left at its default (`None`, no
//! auto-rejoin identity bump) — a deliberate simplification for this slice;
//! revisit if a declared-down instance needs to rejoin faster than
//! `remove_down_after` allows.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use foca::{
    AccumulatingRuntime, Config as FocaConfig, Foca, NoCustomBroadcast, PostcardCodec, Timer,
};
use gsp_config::GossipConfig;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::metrics_defs as m;

type HmacSha256 = Hmac<Sha256>;
const HMAC_TAG_LEN: usize = 32;

/// Live gossip state, cheaply cloneable, read by (future) health integration
/// and tests. Updated only by [`run`].
#[derive(Clone, Default)]
pub struct GossipHandle {
    members: Arc<AtomicUsize>,
}

impl GossipHandle {
    pub fn member_count(&self) -> usize {
        self.members.load(Ordering::Relaxed)
    }
}

/// Runs the gossip mesh until `shutdown` fires. Binds `cfg.bind`, announces
/// to every configured seed, then services membership traffic and SWIM
/// timers off one task — no separate scheduler task or channel plumbing
/// (control-plane, human/domain-paced, matching `health.rs`'s own
/// single-task-loop shape).
pub async fn run(cfg: GossipConfig, handle: GossipHandle, mut shutdown: watch::Receiver<bool>) {
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
    let mut foca: Foca<SocketAddr, PostcardCodec, rand10::rngs::StdRng, NoCustomBroadcast> =
        Foca::new(identity, foca_cfg, rng, PostcardCodec);
    let mut runtime = AccumulatingRuntime::new();
    let mut timers: BinaryHeap<Reverse<TimerEntry>> = BinaryHeap::new();
    let mut recv_buf = vec![0u8; max_packet + HMAC_TAG_LEN];

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
                    Ok((len, _from)) => match verify_and_strip(&cfg.psk, &recv_buf[..len]) {
                        Some(payload) => {
                            metrics::counter!(m::GOSSIP_MESSAGES_TOTAL, "direction" => "received")
                                .increment(1);
                            if let Err(error) = foca.handle_data(payload, &mut runtime) {
                                tracing::debug!(?error, "gossip: bad datagram");
                            }
                        }
                        None => {
                            metrics::counter!(m::GOSSIP_AUTH_REJECTED_TOTAL).increment(1);
                        }
                    },
                    Err(error) => {
                        tracing::warn!(%error, "gossip: recv error");
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

fn publish_member_count(
    foca: &Foca<SocketAddr, PostcardCodec, rand10::rngs::StdRng, NoCustomBroadcast>,
    handle: &GossipHandle,
) {
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
        let tagged = tag(psk, &data);
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
    // wires them into `Backend::observe_domain`. Still drained fully: foca
    // expects its backlog empty before the next `handle_*` call.
    while runtime.to_notify().is_some() {}
}

fn tag(psk: &str, payload: &[u8]) -> Vec<u8> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(psk.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(payload);
    let tag = mac.finalize().into_bytes();
    let mut out = Vec::with_capacity(payload.len() + HMAC_TAG_LEN);
    out.extend_from_slice(payload);
    out.extend_from_slice(&tag);
    out
}

fn verify_and_strip<'a>(psk: &str, datagram: &'a [u8]) -> Option<&'a [u8]> {
    if datagram.len() < HMAC_TAG_LEN {
        return None;
    }
    let (payload, tag) = datagram.split_at(datagram.len() - HMAC_TAG_LEN);
    let mut mac = <HmacSha256 as Mac>::new_from_slice(psk.as_bytes()).ok()?;
    mac.update(payload);
    mac.verify_slice(tag).ok()?;
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_round_trips() {
        let tagged = tag("secret", b"hello");
        assert_eq!(verify_and_strip("secret", &tagged), Some(&b"hello"[..]));
    }

    #[test]
    fn wrong_psk_is_rejected() {
        let tagged = tag("secret", b"hello");
        assert_eq!(verify_and_strip("other", &tagged), None);
    }

    #[test]
    fn truncated_datagram_is_rejected() {
        assert_eq!(verify_and_strip("secret", b"short"), None);
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let mut tagged = tag("secret", b"hello");
        tagged[0] ^= 0xff;
        assert_eq!(verify_and_strip("secret", &tagged), None);
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

        let handle_a = GossipHandle::default();
        let handle_b = GossipHandle::default();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let ha = handle_a.clone();
        let ta = tokio::spawn(run(cfg_a, ha, shutdown_rx.clone()));
        let hb = handle_b.clone();
        let tb = tokio::spawn(run(cfg_b, hb, shutdown_rx.clone()));

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

        let handle_a = GossipHandle::default();
        let handle_b = GossipHandle::default();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let ta = tokio::spawn(run(cfg_a, handle_a.clone(), shutdown_rx.clone()));
        let tb = tokio::spawn(run(cfg_b, handle_b.clone(), shutdown_rx.clone()));

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(handle_a.member_count(), 0);
        assert_eq!(handle_b.member_count(), 0);

        let _ = shutdown_tx.send(true);
        let _ = ta.await;
        let _ = tb.await;
    }
}
