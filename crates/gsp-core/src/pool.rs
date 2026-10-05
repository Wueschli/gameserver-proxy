//! A backend pool: a set of targets, per-backend health and load state, and a
//! selection strategy.
//!
//! [`Pool::acquire`] returns a [`BackendGuard`] that holds an active-session
//! slot for the lifetime of the connection (released on drop). Health state is
//! driven by the active checker ([`crate::health`]) and by passive connect
//! results reported through [`BackendGuard::observe`].

use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gsp_config::{Balancer, HashOn, HealthCheck, HealthCheckKind, PoolConfig, ProxyProtocol};

use crate::metrics_defs as m;
use crate::util::mono_ms;

#[derive(Debug, thiserror::Error)]
pub enum PickError {
    #[error("no healthy backend in pool {0}")]
    NoHealthyBackend(String),
    #[error("all healthy backends in pool {0} are at capacity")]
    AllAtCapacity(String),
}

/// Operator-controlled backend state, set through the admin API and orthogonal
/// to the active/passive *health* flag. `Enabled` is the default; `Draining`
/// and `Disabled` both take the backend out of new-session selection while
/// existing [`BackendGuard`]s keep running (drain). `Disabled` additionally
/// signals "administratively down" for observability — the data path treats the
/// two identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminState {
    Enabled,
    Draining,
    Disabled,
}

impl AdminState {
    fn to_u8(self) -> u8 {
        match self {
            AdminState::Enabled => 0,
            AdminState::Draining => 1,
            AdminState::Disabled => 2,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => AdminState::Draining,
            2 => AdminState::Disabled,
            _ => AdminState::Enabled,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AdminState::Enabled => "enabled",
            AdminState::Draining => "draining",
            AdminState::Disabled => "disabled",
        }
    }

    /// Parse the wire form accepted by `PATCH /pools/{p}/backends/{addr}`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "enabled" => Some(AdminState::Enabled),
            "draining" => Some(AdminState::Draining),
            "disabled" => Some(AdminState::Disabled),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
struct Streaks {
    ok: u32,
    fail: u32,
}

#[derive(Debug)]
pub struct Backend {
    pub addr: SocketAddr,
    pool: Arc<str>,
    healthy: AtomicBool,
    /// Tier-2 regional health fabric override (phase 13, docs/10 "Tier 2").
    /// Additive to `healthy`, never replacing it: `is_healthy()` requires
    /// both. Only [`Backend::observe_domain`] ever sets this — it can only
    /// push a backend *down*, never revive one; only this instance's own
    /// `rise` streak (via [`Backend::observe`]) can clear `healthy`.
    domain_down: AtomicBool,
    admin_state: AtomicU8,
    active: AtomicUsize,
    /// `mono_ms` at which the next active probe is due.
    next_check_ms: AtomicU64,
    /// An active probe is in flight, so the sweep must not start another.
    probing: AtomicBool,
    streaks: Mutex<Streaks>,
    rise: u32,
    fall: u32,
    check_kind: HealthCheckKind,
    check_interval: Duration,
    check_timeout: Duration,
    max_sessions: Option<usize>,
    /// Relative share for `Balancer::Weighted` (>= 1); `1` for every other
    /// balancer.
    weight: u32,
}

/// Deterministic offset in `[0, interval)` derived from the address.
fn first_check_jitter_ms(addr: SocketAddr, interval: Duration) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    addr.hash(&mut h);
    h.finish() % (interval.as_millis() as u64).max(1)
}

impl Backend {
    #[allow(clippy::too_many_arguments)] // one caller (`Pool::new`); a struct would just move the list
    fn new(
        addr: SocketAddr,
        pool: Arc<str>,
        hc: &HealthCheck,
        max_sessions: Option<usize>,
        initially_healthy: bool,
        initially_domain_down: bool,
        initial_state: AdminState,
        weight: u32,
    ) -> Arc<Self> {
        Arc::new(Self {
            addr,
            pool,
            healthy: AtomicBool::new(initially_healthy),
            domain_down: AtomicBool::new(initially_domain_down),
            admin_state: AtomicU8::new(initial_state.to_u8()),
            active: AtomicUsize::new(0),
            // Spread first checks over one interval, per backend, so a pool
            // that appears all at once (startup, reload, discovery) does not
            // probe everything in the same instant.
            next_check_ms: AtomicU64::new(mono_ms() + first_check_jitter_ms(addr, hc.interval)),
            probing: AtomicBool::new(false),
            streaks: Mutex::new(Streaks::default()),
            rise: hc.rise,
            fall: hc.fall,
            check_kind: hc.kind.clone(),
            check_interval: hc.interval,
            check_timeout: hc.timeout,
            max_sessions,
            weight,
        })
    }

    /// This instance's own local verdict (active/passive checks), ignoring
    /// any Tier-2 domain override. Not exposed outside `pool.rs` — carrying
    /// state across a reload (see [`Pool::new`]) is the only caller that
    /// needs to distinguish it from [`Backend::is_healthy`].
    fn local_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    /// Whether the Tier-2 regional health fabric currently overrides this
    /// backend to down (phase 13). Not exposed outside `pool.rs`, same
    /// reason as [`Backend::local_healthy`].
    fn domain_down_flag(&self) -> bool {
        self.domain_down.load(Ordering::Acquire)
    }

    /// Healthy overall: this instance's own checks say so **and** the Tier-2
    /// domain view isn't overriding it down (phase 13, docs/10 "Tier 2" — a
    /// no-op, always `true`, until something calls [`Backend::observe_domain`]).
    pub fn is_healthy(&self) -> bool {
        self.local_healthy() && !self.domain_down_flag()
    }

    /// Feed the Tier-2 regional health fabric's current quorum verdict for
    /// this backend (phase 13). Can only ever push this backend *down* or
    /// clear that override — it never touches the local `healthy` flag, so a
    /// backend still only returns healthy on this instance's own `rise`
    /// streak (docs/10 "Tier 2" authority model). Returns `Some(new_state)`
    /// when `is_healthy()` actually flips as a result.
    pub fn observe_domain(&self, quorum_down: bool) -> Option<bool> {
        let was_healthy = self.is_healthy();
        self.domain_down.store(quorum_down, Ordering::Release);
        metrics::gauge!(
            m::BACKEND_DOMAIN_DOWN,
            "pool" => self.pool.to_string(),
            "backend" => self.addr.to_string(),
        )
        .set(if quorum_down { 1.0 } else { 0.0 });
        let now_healthy = self.is_healthy();
        (was_healthy != now_healthy).then_some(now_healthy)
    }

    pub fn admin_state(&self) -> AdminState {
        AdminState::from_u8(self.admin_state.load(Ordering::Acquire))
    }

    pub fn set_admin_state(&self, state: AdminState) {
        self.admin_state.store(state.to_u8(), Ordering::Release);
    }

    /// Eligible to receive *new* sessions: passing health checks and not
    /// draining / disabled by an operator.
    pub fn takes_new_sessions(&self) -> bool {
        self.is_healthy() && self.admin_state() == AdminState::Enabled
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub fn check_timeout(&self) -> Duration {
        self.check_timeout
    }

    pub fn check_kind(&self) -> &HealthCheckKind {
        &self.check_kind
    }

    /// Claims the backend for an active probe: true when one is due and none is
    /// in flight. The caller must call [`Backend::finish_probe`] afterwards.
    pub(crate) fn begin_probe(&self, now_ms: u64) -> bool {
        if now_ms < self.next_check_ms.load(Ordering::Relaxed) {
            return false;
        }
        if self
            .probing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        self.next_check_ms.store(
            now_ms + self.check_interval.as_millis() as u64,
            Ordering::Relaxed,
        );
        true
    }

    pub(crate) fn finish_probe(&self) {
        self.probing.store(false, Ordering::Release);
    }

    /// This instance's own verdict, for the sweep's `none` recovery path.
    pub(crate) fn locally_healthy(&self) -> bool {
        self.local_healthy()
    }

    #[cfg(test)]
    pub(crate) fn force_due(&self) {
        self.next_check_ms.store(0, Ordering::Relaxed);
    }

    pub(crate) fn is_due(&self, now_ms: u64) -> bool {
        now_ms >= self.next_check_ms.load(Ordering::Relaxed)
    }

    /// Feed a health observation (active check result or passive connect
    /// result). Returns `Some(new_state)` when the healthy flag flips.
    pub fn observe(&self, ok: bool) -> Option<bool> {
        let mut s = self.streaks.lock().unwrap_or_else(PoisonError::into_inner);
        if ok {
            s.fail = 0;
            s.ok = s.ok.saturating_add(1);
            if !self.healthy.load(Ordering::Acquire) && s.ok >= self.rise {
                self.healthy.store(true, Ordering::Release);
                return Some(true);
            }
        } else {
            s.ok = 0;
            s.fail = s.fail.saturating_add(1);
            if self.healthy.load(Ordering::Acquire) && s.fail >= self.fall {
                self.healthy.store(false, Ordering::Release);
                return Some(false);
            }
        }
        None
    }

    fn try_acquire(self: &Arc<Self>) -> Option<BackendGuard> {
        let prev = self.active.fetch_add(1, Ordering::Relaxed);
        if let Some(max) = self.max_sessions {
            if prev >= max {
                self.active.fetch_sub(1, Ordering::Relaxed);
                return None;
            }
        }
        metrics::gauge!(
            m::BACKEND_ACTIVE_SESSIONS,
            "pool" => self.pool.to_string(),
            "backend" => self.addr.to_string(),
        )
        .increment(1.0);
        Some(BackendGuard {
            backend: self.clone(),
        })
    }
}

/// Holds one active-session slot on a backend. Drop releases it.
#[derive(Debug)]
pub struct BackendGuard {
    backend: Arc<Backend>,
}

impl BackendGuard {
    pub fn addr(&self) -> SocketAddr {
        self.backend.addr
    }

    /// A handle to the underlying backend, for passive health observations made
    /// by code that runs past the guard's own scope (e.g. the UDP reply pump,
    /// which outlives this call but not the [`Session`](crate::listener_udp)).
    pub fn backend(&self) -> Arc<Backend> {
        self.backend.clone()
    }

    /// Report a connect result for passive health tracking.
    pub fn observe(&self, ok: bool) {
        if let Some(new_state) = self.backend.observe(ok) {
            tracing::info!(
                pool = %self.backend.pool,
                backend = %self.backend.addr,
                healthy = new_state,
                "backend health changed (passive)"
            );
        }
    }
}

impl Drop for BackendGuard {
    fn drop(&mut self) {
        self.backend.active.fetch_sub(1, Ordering::Relaxed);
        metrics::gauge!(
            m::BACKEND_ACTIVE_SESSIONS,
            "pool" => self.backend.pool.to_string(),
            "backend" => self.backend.addr.to_string(),
        )
        .decrement(1.0);
    }
}

#[derive(Debug)]
pub struct Pool {
    pub name: Arc<str>,
    pub balancer: Balancer,
    /// `Some` iff `balancer == ConsistentHash`.
    pub hash_on: Option<HashOn>,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    /// PROXY protocol header to prepend to each upstream connection, if any.
    pub proxy_protocol: ProxyProtocol,
    backends: Vec<Arc<Backend>>,
    rr: AtomicUsize,
}

impl Pool {
    /// Build a pool from config. `prev` (the same pool from the previous
    /// snapshot, if any) is consulted to carry over per-backend health state
    /// by address across a hot reload.
    pub fn new(cfg: &PoolConfig, prev: Option<&Arc<Pool>>) -> Self {
        let name: Arc<str> = Arc::from(cfg.name.as_str());
        let backends = cfg
            .targets
            .iter()
            .map(|&addr| {
                let prev_backend = prev.and_then(|p| p.backends.iter().find(|b| b.addr == addr));
                let carried_healthy = prev_backend.map(|b| b.local_healthy()).unwrap_or(true);
                let carried_domain_down =
                    prev_backend.map(|b| b.domain_down_flag()).unwrap_or(false);
                let carried_state = prev_backend
                    .map(|b| b.admin_state())
                    .unwrap_or(AdminState::Enabled);
                Backend::new(
                    addr,
                    name.clone(),
                    &cfg.health_check,
                    cfg.max_sessions,
                    carried_healthy,
                    carried_domain_down,
                    carried_state,
                    cfg.weights.get(&addr).copied().unwrap_or(1),
                )
            })
            .collect();
        Self {
            name,
            balancer: cfg.balancer,
            hash_on: cfg.hash_on,
            connect_timeout: cfg.connect_timeout,
            idle_timeout: cfg.idle_timeout,
            proxy_protocol: cfg.proxy_protocol,
            backends,
            rr: AtomicUsize::new(0),
        }
    }

    pub fn backends(&self) -> &[Arc<Backend>] {
        &self.backends
    }

    /// Look up a backend by address (admin API: state changes, introspection).
    pub fn backend(&self, addr: SocketAddr) -> Option<&Arc<Backend>> {
        self.backends.iter().find(|b| b.addr == addr)
    }

    /// Reserve a slot on the backend at `want` specifically (UDP session
    /// affinity). Returns `None` if that backend is gone, unhealthy, or full;
    /// the caller then falls back to [`Pool::acquire`].
    pub fn acquire_addr(&self, want: SocketAddr) -> Option<BackendGuard> {
        let b = self.backends.iter().find(|b| b.addr == want)?;
        if !b.takes_new_sessions() {
            return None;
        }
        b.try_acquire()
    }

    /// Select a healthy backend with free capacity and reserve a session slot.
    /// Uses round-robin / least-conn ordering; `consistent_hash` pools fall back
    /// to round-robin here (no client key). Prefer [`Pool::acquire_for`].
    pub fn acquire(&self) -> Result<BackendGuard, PickError> {
        self.acquire_for(None)
    }

    /// Like [`Pool::acquire`], but `client` supplies the key a `consistent_hash`
    /// pool hashes on (`hash_on`); the other balancers ignore it.
    pub fn acquire_for(&self, client: Option<SocketAddr>) -> Result<BackendGuard, PickError> {
        let mut healthy: Vec<&Arc<Backend>> = self
            .backends
            .iter()
            .filter(|b| b.takes_new_sessions())
            .collect();
        if healthy.is_empty() {
            metrics::counter!(
                m::LB_SELECTIONS,
                "pool" => self.name.to_string(),
                "strategy" => strategy_str(self.balancer),
                "result" => "no_backend",
            )
            .increment(1);
            return Err(PickError::NoHealthyBackend(self.name.to_string()));
        }

        let round_robin = |healthy: &mut Vec<&Arc<Backend>>| {
            let start = self.rr.fetch_add(1, Ordering::Relaxed) % healthy.len();
            healthy.rotate_left(start);
        };
        match self.balancer {
            Balancer::LeastConn => healthy.sort_by_key(|b| b.active()),
            Balancer::RoundRobin => round_robin(&mut healthy),
            Balancer::ConsistentHash => match client {
                // Highest rendezvous score first; the rest stay ordered by
                // descending score so capacity fall-through is deterministic.
                Some(c) => {
                    healthy.sort_by_key(|b| std::cmp::Reverse(hrw_score(self.hash_on, c, b.addr)))
                }
                None => round_robin(&mut healthy),
            },
            // Weighted round-robin: one atomic tick indexes into the cumulative
            // weight line of the healthy set, then rotate so the chosen backend
            // leads (capacity fall-through then walks the others in their
            // existing order). Blocky rather than interleaved — fine for the
            // handful of backends a pool holds.
            Balancer::Weighted => {
                let total: u64 = healthy.iter().map(|b| b.weight as u64).sum();
                if total > 0 {
                    let mut pos = self.rr.fetch_add(1, Ordering::Relaxed) as u64 % total;
                    let mut idx = 0;
                    for (i, b) in healthy.iter().enumerate() {
                        let w = b.weight as u64;
                        if pos < w {
                            idx = i;
                            break;
                        }
                        pos -= w;
                    }
                    healthy.rotate_left(idx);
                }
            }
        }

        for b in healthy {
            if let Some(guard) = b.try_acquire() {
                metrics::counter!(
                    m::LB_SELECTIONS,
                    "pool" => self.name.to_string(),
                    "strategy" => strategy_str(self.balancer),
                    "result" => "ok",
                )
                .increment(1);
                return Ok(guard);
            }
        }

        metrics::counter!(
            m::LB_SELECTIONS,
            "pool" => self.name.to_string(),
            "strategy" => strategy_str(self.balancer),
            "result" => "at_capacity",
        )
        .increment(1);
        Err(PickError::AllAtCapacity(self.name.to_string()))
    }
}

fn strategy_str(b: Balancer) -> &'static str {
    match b {
        Balancer::RoundRobin => "round_robin",
        Balancer::LeastConn => "least_conn",
        Balancer::ConsistentHash => "consistent_hash",
        Balancer::Weighted => "weighted",
    }
}

/// Rendezvous (HRW) score for `(client key, backend)`. The backend with the
/// highest score owns the key; when a backend joins/leaves only keys whose top
/// score was on that backend move. Uses `DefaultHasher` — stable for the life
/// of the process, which is all consistent_hash needs (a reload rebuilds pools,
/// and there is no cross-instance shared state).
fn hrw_score(hash_on: Option<HashOn>, client: SocketAddr, backend: SocketAddr) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    client.ip().hash(&mut h);
    if matches!(hash_on, Some(HashOn::SrcIpPort)) {
        client.port().hash(&mut h);
    }
    backend.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcfg(targets: &[&str], balancer: Balancer, max: Option<usize>) -> PoolConfig {
        PoolConfig {
            name: "t".into(),
            targets: targets.iter().map(|s| s.parse().unwrap()).collect(),
            source: None,
            balancer,
            hash_on: matches!(balancer, Balancer::ConsistentHash).then_some(HashOn::SrcIp),
            weights: std::collections::HashMap::new(),
            connect_timeout: Duration::from_millis(300),
            idle_timeout: Duration::from_secs(90),
            proxy_protocol: ProxyProtocol::None,
            health_check: HealthCheck {
                kind: HealthCheckKind::TcpConnect,
                interval: Duration::from_secs(2),
                timeout: Duration::from_millis(500),
                rise: 2,
                fall: 3,
            },
            max_sessions: max,
        }
    }

    #[test]
    fn round_robin_cycles_through_targets() {
        let p = Pool::new(
            &pcfg(
                &["127.0.0.1:1", "127.0.0.1:2", "127.0.0.1:3"],
                Balancer::RoundRobin,
                None,
            ),
            None,
        );
        let seq: Vec<u16> = (0..6).map(|_| p.acquire().unwrap().addr().port()).collect();
        assert_eq!(seq, vec![1, 2, 3, 1, 2, 3]);
    }

    #[test]
    fn weighted_distributes_new_sessions_by_weight() {
        let mut cfg = pcfg(&["127.0.0.1:1", "127.0.0.1:2"], Balancer::Weighted, None);
        cfg.weights = [
            ("127.0.0.1:1".parse().unwrap(), 3),
            ("127.0.0.1:2".parse().unwrap(), 1),
        ]
        .into_iter()
        .collect();
        let p = Pool::new(&cfg, None);

        let (mut n1, mut n2) = (0, 0);
        for _ in 0..80 {
            // guard drops each iteration — `weighted` ignores the active count,
            // so the ratio is purely the configured weights
            match p.acquire().unwrap().addr().port() {
                1 => n1 += 1,
                2 => n2 += 1,
                other => panic!("unexpected backend port {other}"),
            }
        }
        assert_eq!((n1, n2), (60, 20), "3:1 weight ⇒ 3:1 sessions");
    }

    #[test]
    fn weighted_falls_through_when_the_heavier_backend_is_full() {
        let mut cfg = pcfg(&["127.0.0.1:1", "127.0.0.1:2"], Balancer::Weighted, Some(1));
        cfg.weights = [("127.0.0.1:1".parse().unwrap(), 10)].into_iter().collect();
        let p = Pool::new(&cfg, None);

        let _g1 = p.acquire().unwrap(); // weight 10 ⇒ :1 is picked and now full
        let g2 = p.acquire().unwrap(); // :1 still picked by weight, full ⇒ fall through
        assert_eq!(g2.addr().port(), 2);
    }

    #[test]
    fn weighted_without_a_weights_map_is_plain_round_robin() {
        // every backend defaults to weight 1
        let p = Pool::new(
            &pcfg(&["127.0.0.1:1", "127.0.0.1:2"], Balancer::Weighted, None),
            None,
        );
        let seq: Vec<u16> = (0..4).map(|_| p.acquire().unwrap().addr().port()).collect();
        assert_eq!(seq, vec![1, 2, 1, 2]);
    }

    #[test]
    fn least_conn_prefers_the_idle_backend() {
        let p = Pool::new(
            &pcfg(&["127.0.0.1:1", "127.0.0.1:2"], Balancer::LeastConn, None),
            None,
        );
        let g1 = p.acquire().unwrap(); // one backend now has 1 active session
        let busy = g1.addr().port();
        let g2 = p.acquire().unwrap(); // must go to the other (0 < 1)
        assert_ne!(
            g2.addr().port(),
            busy,
            "second session should pick the idle backend"
        );
    }

    #[test]
    fn respects_per_backend_capacity() {
        let p = Pool::new(&pcfg(&["127.0.0.1:1"], Balancer::RoundRobin, Some(1)), None);
        let _g = p.acquire().unwrap();
        let err = p.acquire().unwrap_err();
        assert!(matches!(err, PickError::AllAtCapacity(_)), "got {err:?}");
    }

    #[test]
    fn unhealthy_backends_are_skipped() {
        let p = Pool::new(
            &pcfg(&["127.0.0.1:1", "127.0.0.1:2"], Balancer::RoundRobin, None),
            None,
        );
        for _ in 0..3 {
            p.backends()[0].observe(false); // fall = 3
        }
        assert!(!p.backends()[0].is_healthy());
        for _ in 0..4 {
            assert_eq!(p.acquire().unwrap().addr().port(), 2);
        }
    }

    #[test]
    fn consistent_hash_is_stable_and_spreads() {
        let p = Pool::new(
            &pcfg(
                &["127.0.0.1:1", "127.0.0.1:2", "127.0.0.1:3", "127.0.0.1:4"],
                Balancer::ConsistentHash,
                None,
            ),
            None,
        );
        let pick = |ip: &str| {
            let g = p
                .acquire_for(Some(format!("{ip}:40000").parse().unwrap()))
                .unwrap();
            g.addr().port()
        };
        // Same client key -> same backend, every time.
        let a = pick("203.0.113.7");
        for _ in 0..20 {
            assert_eq!(pick("203.0.113.7"), a);
        }
        // src_ip keying: the port must not matter.
        let g = p
            .acquire_for(Some("203.0.113.7:59999".parse().unwrap()))
            .unwrap();
        assert_eq!(g.addr().port(), a);
        // Different clients don't all collapse onto one backend.
        let spread: std::collections::BTreeSet<u16> =
            (0..40).map(|i| pick(&format!("198.51.100.{i}"))).collect();
        assert!(spread.len() >= 2, "hash should use more than one backend");
    }

    #[test]
    fn consistent_hash_reassigns_only_the_lost_backend_share() {
        let targets = ["127.0.0.1:1", "127.0.0.1:2", "127.0.0.1:3", "127.0.0.1:4"];
        let p = Pool::new(&pcfg(&targets, Balancer::ConsistentHash, None), None);
        let clients: Vec<std::net::SocketAddr> = (0..60)
            .map(|i| format!("198.51.100.{i}:1000").parse().unwrap())
            .collect();
        let before: Vec<u16> = clients
            .iter()
            .map(|&c| p.acquire_for(Some(c)).unwrap().addr().port())
            .collect();

        // Take backend :1 out of service.
        for _ in 0..3 {
            p.backends()[0].observe(false);
        }
        let after: Vec<u16> = clients
            .iter()
            .map(|&c| p.acquire_for(Some(c)).unwrap().addr().port())
            .collect();

        for (b, a) in before.iter().zip(&after) {
            if *b != 1 {
                assert_eq!(b, a, "clients not on the lost backend must not move");
            } else {
                assert_ne!(*a, 1, "clients on the lost backend must move elsewhere");
            }
        }
    }

    #[test]
    fn all_unhealthy_yields_error() {
        let p = Pool::new(&pcfg(&["127.0.0.1:1"], Balancer::RoundRobin, None), None);
        for _ in 0..3 {
            p.backends()[0].observe(false);
        }
        assert!(matches!(
            p.acquire().unwrap_err(),
            PickError::NoHealthyBackend(_)
        ));
    }

    #[test]
    fn draining_backend_gets_no_new_sessions_but_keeps_existing_ones() {
        let p = Pool::new(
            &pcfg(&["127.0.0.1:1", "127.0.0.1:2"], Balancer::RoundRobin, None),
            None,
        );
        // Open a session on :1, then drain it.
        let held = p.acquire_addr("127.0.0.1:1".parse().unwrap()).unwrap();
        p.backend("127.0.0.1:1".parse().unwrap())
            .unwrap()
            .set_admin_state(AdminState::Draining);

        // New selection avoids the draining backend entirely.
        for _ in 0..6 {
            assert_eq!(p.acquire().unwrap().addr().port(), 2);
        }
        // Affinity to the draining backend is refused (caller falls back).
        assert!(p.acquire_addr("127.0.0.1:1".parse().unwrap()).is_none());
        // The pre-existing guard is untouched.
        assert_eq!(held.addr().port(), 1);
        assert_eq!(
            p.backend("127.0.0.1:1".parse().unwrap()).unwrap().active(),
            1
        );
    }

    #[test]
    fn disabled_leaves_only_error_when_it_is_the_last_backend() {
        let p = Pool::new(&pcfg(&["127.0.0.1:1"], Balancer::RoundRobin, None), None);
        p.backends()[0].set_admin_state(AdminState::Disabled);
        assert!(matches!(
            p.acquire().unwrap_err(),
            PickError::NoHealthyBackend(_)
        ));
    }

    #[test]
    fn admin_state_survives_a_reload() {
        let cfg = pcfg(&["127.0.0.1:1", "127.0.0.1:2"], Balancer::RoundRobin, None);
        let s1 = Pool::new(&cfg, None);
        s1.backends()[0].set_admin_state(AdminState::Draining);
        let s2 = Pool::new(&cfg, Some(&Arc::new(s1)));
        assert_eq!(s2.backends()[0].admin_state(), AdminState::Draining);
        assert_eq!(s2.backends()[1].admin_state(), AdminState::Enabled);
    }

    #[test]
    fn observe_applies_rise_and_fall_thresholds() {
        let p = Pool::new(&pcfg(&["127.0.0.1:1"], Balancer::RoundRobin, None), None);
        let b = &p.backends()[0];
        assert_eq!(b.observe(false), None); // 1 fail
        assert_eq!(b.observe(false), None); // 2 fails
        assert_eq!(b.observe(false), Some(false)); // 3 fails -> down
        assert_eq!(b.observe(true), None); // 1 ok
        assert_eq!(b.observe(true), Some(true)); // 2 oks -> up (rise = 2)
    }

    #[test]
    fn domain_down_overrides_healthy_but_local_rise_still_clears_it() {
        let p = Pool::new(&pcfg(&["127.0.0.1:1"], Balancer::RoundRobin, None), None);
        let b = &p.backends()[0];
        assert!(b.is_healthy()); // starts healthy, no domain override yet

        assert_eq!(b.observe_domain(true), Some(false)); // domain says down
        assert!(!b.is_healthy());
        assert_eq!(b.observe_domain(true), None); // no change, no flip reported

        // A local passive/active success does NOT clear a domain override.
        assert_eq!(b.observe(true), None);
        assert!(!b.is_healthy());

        // Only the domain view clearing it brings it back.
        assert_eq!(b.observe_domain(false), Some(true));
        assert!(b.is_healthy());
    }

    #[test]
    fn domain_down_never_revives_a_locally_unhealthy_backend() {
        let p = Pool::new(&pcfg(&["127.0.0.1:1"], Balancer::RoundRobin, None), None);
        let b = &p.backends()[0];
        b.observe(false);
        b.observe(false);
        assert_eq!(b.observe(false), Some(false)); // locally down (fall = 3)
        assert!(!b.is_healthy());

        // Clearing the domain override alone must not revive it — only this
        // instance's own `rise` streak can.
        assert_eq!(b.observe_domain(false), None);
        assert!(!b.is_healthy());
    }

    #[test]
    fn domain_down_carries_across_a_reload_by_address() {
        let p1 = Pool::new(&pcfg(&["127.0.0.1:1"], Balancer::RoundRobin, None), None);
        p1.backends()[0].observe_domain(true);
        assert!(!p1.backends()[0].is_healthy());

        let p2 = Pool::new(
            &pcfg(&["127.0.0.1:1"], Balancer::RoundRobin, None),
            Some(&Arc::new(p1)),
        );
        assert!(!p2.backends()[0].is_healthy());
    }
}
