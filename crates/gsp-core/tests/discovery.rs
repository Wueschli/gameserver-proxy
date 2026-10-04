//! Phase 8: backend discovery feeds the snapshot rebuild, level-triggered.
//!
//! These cover the runtime-facing contract without any real DNS / HTTP:
//! - a discovered set populates a pool that has a `source`;
//! - overlay edits layer on top of the discovered set (precedence);
//! - `refresh_loop` picks up a changed set and asks for a rebuild;
//! - a source that errors or returns empty keeps the last-known-good set.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gsp_config::{parse_str, SourceConfig};
use gsp_core::discovery::{refresh_loop, BackendSource, Discovery};
use gsp_core::{BackendOverlay, Runtime, Snapshot, SourceFactory};
use tokio::sync::{watch, Notify};

const CFG: &str = r#"
backend_sources:
  - name: fleet
    type: dns_srv
    record: "_game._udp.svc"
    refresh_interval_sec: 1
pools:
  - name: game
    source: fleet
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: game
"#;

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn pool_addrs(snap: &Snapshot, pool: &str) -> Vec<String> {
    let mut v: Vec<String> = snap
        .pool(pool)
        .unwrap()
        .backends()
        .iter()
        .map(|b| b.addr.to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn discovery_populates_a_pool_with_a_source() {
    let cfg = parse_str(CFG).unwrap();
    let overlay = BackendOverlay::new();
    let discovery = Discovery::new();

    // Before any refresh: the seed is empty, so the pool has no backends.
    let s0 = Snapshot::build_with_sources(&cfg, None, &overlay, &discovery);
    assert!(s0.pool("game").unwrap().backends().is_empty());

    discovery.store("game", vec![addr("10.0.0.1:7777"), addr("10.0.0.2:7777")]);
    let s1 = Snapshot::build_with_sources(&cfg, Some(&s0), &overlay, &discovery);
    assert_eq!(pool_addrs(&s1, "game"), ["10.0.0.1:7777", "10.0.0.2:7777"]);
}

#[test]
fn overlay_layers_on_top_of_the_discovered_set() {
    let cfg = parse_str(CFG).unwrap();
    let overlay = BackendOverlay::new();
    let discovery = Discovery::new();
    discovery.store("game", vec![addr("10.0.0.1:7777"), addr("10.0.0.2:7777")]);

    // discovered ∪ overlay-added − overlay-removed
    overlay.add("game", addr("10.0.0.9:7777"));
    overlay.remove("game", addr("10.0.0.1:7777"));

    let s = Snapshot::build_with_sources(&cfg, None, &overlay, &discovery);
    assert_eq!(pool_addrs(&s, "game"), ["10.0.0.2:7777", "10.0.0.9:7777"]);
}

/// A scripted source: each `fetch` pops the next result off a queue.
struct ScriptedSource {
    calls: AtomicUsize,
    script: Mutex<Vec<anyhow::Result<Vec<SocketAddr>>>>,
}

impl ScriptedSource {
    fn new(script: Vec<anyhow::Result<Vec<SocketAddr>>>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            script: Mutex::new(script),
        })
    }
}

#[async_trait::async_trait]
impl BackendSource for ScriptedSource {
    fn pool(&self) -> &str {
        "game"
    }
    fn kind(&self) -> &'static str {
        "dns_srv"
    }
    fn refresh_interval(&self) -> Duration {
        Duration::from_millis(50)
    }
    async fn fetch(&self) -> anyhow::Result<Vec<SocketAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut s = self.script.lock().unwrap();
        if s.is_empty() {
            // Keep returning the last scripted behaviour: a stable empty set.
            return Ok(Vec::new());
        }
        s.remove(0)
    }
}

#[tokio::test]
async fn refresh_picks_up_a_change_and_a_down_source_keeps_the_last_set() {
    let discovery = Arc::new(Discovery::new());
    let reload = Arc::new(Notify::new());
    let (sd_tx, mut sd_rx) = watch::channel(false);

    let source = ScriptedSource::new(vec![
        Ok(vec![addr("10.0.0.1:7777")]), // first refresh
        Ok(vec![addr("10.0.0.1:7777"), addr("10.0.0.2:7777")]), // grew
        Err(anyhow::anyhow!("source down")), // must not clear
        Ok(Vec::new()),                  // empty must not clear
    ]);

    let src_dyn: Arc<dyn BackendSource> = source.clone();
    let d = discovery.clone();
    let r = reload.clone();
    let task = tokio::spawn(async move {
        refresh_loop(src_dyn, d, r, &mut sd_rx).await;
    });

    // First non-empty result lands and wakes the reload path.
    tokio::time::timeout(Duration::from_secs(2), reload.notified())
        .await
        .expect("first change notified");
    assert_eq!(discovery.get("game").unwrap(), vec![addr("10.0.0.1:7777")]);

    // Second result (grew) also notifies.
    tokio::time::timeout(Duration::from_secs(2), reload.notified())
        .await
        .expect("second change notified");
    assert_eq!(
        discovery.get("game").unwrap(),
        vec![addr("10.0.0.1:7777"), addr("10.0.0.2:7777")]
    );

    // Let the error + empty refreshes run; neither may change the set.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        discovery.get("game").unwrap(),
        vec![addr("10.0.0.1:7777"), addr("10.0.0.2:7777")],
        "an errored / empty refresh must keep the last-known-good set"
    );
    assert!(source.calls.load(Ordering::SeqCst) >= 4);

    let _ = sd_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
}

/// A fixed-set source + a factory over it, keyed by the SRV `record` string
/// (abused as a literal `ip:port`) so a reconcile with a changed spec is
/// observable.
struct FixedSource {
    pool: String,
    addr: SocketAddr,
}

#[async_trait::async_trait]
impl BackendSource for FixedSource {
    fn pool(&self) -> &str {
        &self.pool
    }
    fn kind(&self) -> &'static str {
        "dns_srv"
    }
    fn refresh_interval(&self) -> Duration {
        Duration::from_millis(30)
    }
    async fn fetch(&self) -> anyhow::Result<Vec<SocketAddr>> {
        Ok(vec![self.addr])
    }
}

struct FixedFactory;

impl SourceFactory for FixedFactory {
    fn build(&self, pool: &str, cfg: &SourceConfig) -> anyhow::Result<Arc<dyn BackendSource>> {
        let record = match &cfg.kind {
            gsp_config::SourceKind::DnsSrv { record } => record.clone(),
            _ => unreachable!(),
        };
        Ok(Arc::new(FixedSource {
            pool: pool.to_string(),
            addr: record.parse()?,
        }))
    }
}

#[tokio::test]
async fn runtime_reconciles_sources_when_backend_sources_change() {
    let with_source = |rec: &str| {
        format!(
            "backend_sources:\n  - {{ name: fleet, type: dns_srv, record: \"{rec}\", refresh_interval_sec: 1 }}\n\
             pools:\n  - {{ name: game, source: fleet }}\n\
             listeners:\n  - {{ name: l, bind: \"127.0.0.1:0\", pool: game }}\n"
        )
    };

    let cfg = parse_str(&with_source("10.0.0.1:7777")).unwrap();
    let discovery = Arc::new(Discovery::new());
    let snap = Snapshot::build_with_sources(&cfg, None, &BackendOverlay::new(), &discovery);
    let runtime = Runtime::start_with_discovery(
        snap,
        Arc::default(),
        None,
        Arc::default(),
        discovery.clone(),
        Some(Arc::new(FixedFactory)),
        None,
        1,
    );
    let handle = runtime.handle();

    // The refresh task's first tick populates the pool.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(discovery.get("game").unwrap(), vec![addr("10.0.0.1:7777")]);

    // Reload with a re-parameterised source: rebuild the snapshot, store it,
    // reconcile. The task restarts and the new address lands.
    let cfg2 = parse_str(&with_source("10.0.0.2:7777")).unwrap();
    handle.store(Snapshot::build_with_sources(
        &cfg2,
        Some(&handle.current()),
        &BackendOverlay::new(),
        &discovery,
    ));
    let (running, stopped) = handle.reconcile_sources().await;
    assert_eq!((running, stopped), (1, 1));
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(discovery.get("game").unwrap(), vec![addr("10.0.0.2:7777")]);

    // Reload dropping the source entirely: the task stops, the cached set is
    // forgotten.
    let cfg3 = parse_str(
        "pools:\n  - { name: game, targets: [\"10.0.0.5:7777\"] }\n\
         listeners:\n  - { name: l, bind: \"127.0.0.1:0\", pool: game }\n",
    )
    .unwrap();
    handle.store(Snapshot::build_with_sources(
        &cfg3,
        Some(&handle.current()),
        &BackendOverlay::new(),
        &discovery,
    ));
    let (running, stopped) = handle.reconcile_sources().await;
    assert_eq!((running, stopped), (0, 1));
    assert!(discovery.get("game").is_none());

    runtime
        .shutdown_with_grace(Duration::from_millis(100))
        .await;
}
