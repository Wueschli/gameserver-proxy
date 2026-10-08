//! Runs enabled plugins on their timer (design: the plugin system spec's HA and ticks
//! section, in its single-node form).
//!
//! Each pass over the installs ([`Runner::run_due`]) starts a tick for every enabled install
//! whose approved declaration has `on_timer` and whose interval has elapsed. The first tick
//! comes one full interval after the runner first sees the install (the grace period of the
//! design), so enabling a plugin never runs it in the request. A plugin is compiled on first
//! use through the [`CompilePool`], `init` runs once with its config, and every call runs on
//! the pool, so a slow or busy pool delays or skips ticks and never blocks the runtime.
//!
//! A call returns its [`Effects`]; the runner commits the state writes as one batch that is
//! compare-and-set on the install's state revision, then records the outcome in memory
//! (last result and the last [`MAX_LOG_LINES`] log lines; not replicated, lost on restart).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use wayhouse_plugin_host::{CallError, CompilePool, Effects, LogLevel, Plugin, PoolError};

use super::{CommitError, InstallRecord, PluginStore};

/// Log lines kept per install.
pub const MAX_LOG_LINES: usize = 100;

/// How often [`Runner::spawn`] looks for due ticks.
const SCAN_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusLog {
    pub level: &'static str,
    pub msg: String,
}

/// What an install's ticks have done so far.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PluginStatus {
    /// Ticks started (including ones that failed or were skipped).
    pub ticks: u64,
    pub last_tick_unix: Option<u64>,
    pub last_ok: Option<bool>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    pub logs: Vec<StatusLog>,
}

struct Loaded {
    sha256: String,
    plugin: Arc<Plugin>,
}

#[derive(Default)]
struct Schedule {
    loaded: HashMap<String, Loaded>,
    next_due: HashMap<String, Instant>,
}

pub struct Runner {
    store: PluginStore,
    pool: Arc<CompilePool>,
    schedule: tokio::sync::Mutex<Schedule>,
    status: Mutex<HashMap<String, PluginStatus>>,
}

fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The outcome of the load and the call, before it is committed.
enum Outcome {
    Effects(u64, Effects),
    Failed(String),
}

impl Runner {
    pub fn new(store: PluginStore, pool: Arc<CompilePool>) -> Arc<Self> {
        Arc::new(Self {
            store,
            pool,
            schedule: tokio::sync::Mutex::new(Schedule::default()),
            status: Mutex::new(HashMap::new()),
        })
    }

    /// Scan once a second until the task is dropped.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut every = tokio::time::interval(SCAN_EVERY);
            loop {
                every.tick().await;
                self.run_due(Instant::now()).await;
            }
        })
    }

    pub fn status(&self, id: &str) -> Option<PluginStatus> {
        self.lock_status().get(id).cloned()
    }

    fn lock_status(&self) -> std::sync::MutexGuard<'_, HashMap<String, PluginStatus>> {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One scheduling pass at `now`: start every due tick and wait for them all.
    pub async fn run_due(self: &Arc<Self>, now: Instant) {
        let installs = match self.store.list() {
            Ok(i) => i,
            Err(e) => {
                tracing::error!(error = %e, "plugin runner could not list installs");
                return;
            }
        };
        let mut due = Vec::new();
        {
            let mut sched = self.schedule.lock().await;
            let live = |r: &&InstallRecord| r.enabled && r.approved.triggers.on_timer;
            let ids: Vec<&str> = installs
                .iter()
                .filter(live)
                .map(|r| r.id.as_str())
                .collect();
            sched.loaded.retain(|id, _| ids.contains(&id.as_str()));
            sched.next_due.retain(|id, _| ids.contains(&id.as_str()));
            let all: Vec<&str> = installs.iter().map(|r| r.id.as_str()).collect();
            self.lock_status()
                .retain(|id, _| all.contains(&id.as_str()));
            for rec in installs.iter().filter(live) {
                let interval = Duration::from_secs(rec.approved.tick_interval_secs);
                let at = *sched
                    .next_due
                    .entry(rec.id.clone())
                    .or_insert_with(|| now + interval);
                if now >= at {
                    sched.next_due.insert(rec.id.clone(), now + interval);
                    due.push(rec.clone());
                }
            }
        }
        let mut tasks = tokio::task::JoinSet::new();
        for rec in due {
            let this = self.clone();
            tasks.spawn(async move { this.tick(rec).await });
        }
        while tasks.join_next().await.is_some() {}
    }

    async fn tick(self: Arc<Self>, rec: InstallRecord) {
        let outcome = self.call(&rec).await;
        self.finish(&rec.id, outcome);
    }

    /// Load (and `init`) if needed, then run `on_timer`, all on the pool.
    async fn call(&self, rec: &InstallRecord) -> Outcome {
        let cached = {
            let sched = self.schedule.lock().await;
            sched
                .loaded
                .get(&rec.id)
                .filter(|l| l.sha256 == rec.sha256)
                .map(|l| l.plugin.clone())
        };
        let plugin = match cached {
            Some(p) => p,
            None => match self.load_and_init(rec).await {
                Ok(p) => p,
                Err(why) => return Outcome::Failed(why),
            },
        };
        let (rev, snapshot) = match self.store.state(&rec.id) {
            Ok(s) => s,
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        let pool = self.pool.clone();
        let result =
            tokio::task::spawn_blocking(move || pool.call(move || plugin.on_timer(&snapshot)))
                .await;
        match flatten(result) {
            Ok(fx) => Outcome::Effects(rev, fx),
            Err(why) => Outcome::Failed(why),
        }
    }

    async fn load_and_init(&self, rec: &InstallRecord) -> Result<Arc<Plugin>, String> {
        let bytes = self
            .store
            .get_blob(&rec.sha256)
            .map_err(|e| e.to_string())?
            .ok_or("the module is no longer stored")?;
        let pool = self.pool.clone();
        let approved = rec.approved.clone();
        let loaded = tokio::task::spawn_blocking(move || pool.load(bytes, approved))
            .await
            .map_err(|e| e.to_string())?;
        let plugin = Arc::new(loaded.map_err(|e| match e {
            PoolError::Busy => "the plugin worker pool is busy; tick skipped".to_string(),
            e => e.to_string(),
        })?);
        let (rev, snapshot) = self.store.state(&rec.id).map_err(|e| e.to_string())?;
        let config = serde_json::to_vec(&rec.config).map_err(|e| e.to_string())?;
        let (p, pool) = (plugin.clone(), self.pool.clone());
        let fx = flatten(
            tokio::task::spawn_blocking(move || pool.call(move || p.init(&config, &snapshot)))
                .await,
        )
        .map_err(|e| format!("init failed: {e}"))?;
        self.commit(&rec.id, rev, &fx)?;
        self.record_logs(&rec.id, &fx);
        self.schedule.lock().await.loaded.insert(
            rec.id.clone(),
            Loaded {
                sha256: rec.sha256.clone(),
                plugin: plugin.clone(),
            },
        );
        Ok(plugin)
    }

    fn commit(&self, id: &str, rev: u64, fx: &Effects) -> Result<(), String> {
        if fx.state_puts.is_empty() {
            return Ok(());
        }
        match self.store.commit_state(id, rev, &fx.state_puts) {
            Ok(_) => Ok(()),
            Err(CommitError::Stale) => {
                Err("the state changed during the call; its writes were dropped".to_string())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn record_logs(&self, id: &str, fx: &Effects) {
        let mut all = self.lock_status();
        let st = all.entry(id.to_string()).or_default();
        st.logs.extend(fx.logs.iter().map(|l| StatusLog {
            level: level_name(l.level),
            msg: l.msg.clone(),
        }));
        let extra = st.logs.len().saturating_sub(MAX_LOG_LINES);
        st.logs.drain(..extra);
    }

    /// Commit a finished call's effects and record the result.
    fn finish(&self, id: &str, outcome: Outcome) {
        let result = match outcome {
            Outcome::Effects(rev, fx) => {
                self.record_logs(id, &fx);
                self.commit(id, rev, &fx)
            }
            Outcome::Failed(why) => Err(why),
        };
        let mut all = self.lock_status();
        let st = all.entry(id.to_string()).or_default();
        st.ticks += 1;
        st.last_tick_unix = Some(unix_now());
        match result {
            Ok(()) => {
                st.last_ok = Some(true);
                st.last_error = None;
                st.consecutive_failures = 0;
            }
            Err(why) => {
                tracing::warn!(install = id, error = %why, "plugin tick failed");
                st.last_ok = Some(false);
                st.last_error = Some(why);
                st.consecutive_failures = st.consecutive_failures.saturating_add(1);
            }
        }
    }
}

fn flatten(
    joined: Result<Result<Result<Effects, CallError>, PoolError>, tokio::task::JoinError>,
) -> Result<Effects, String> {
    match joined {
        Ok(Ok(Ok(fx))) => Ok(fx),
        Ok(Ok(Err(e))) => Err(e.to_string()),
        Ok(Err(PoolError::Busy)) => Err("the plugin worker pool is busy; tick skipped".to_string()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use wayhouse_plugin_host::{inspect, Limits, PluginHost};

    const ABI: &str = "\\00\\00\\01\\00";
    const TIMER: &str = r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true,"state":{"max_bytes":64}}"#;
    const NO_TIMER: &str = r#"{"log":true}"#;

    fn guest(caps: &str, funcs: &str) -> Vec<u8> {
        let caps = caps.replace('\\', "\\\\").replace('"', "\\\"");
        let src = format!(
            r#"(module
  (@custom "wayhouse.plugin-abi" "{ABI}")
  (@custom "wayhouse.plugin-caps" "{caps}")
  (import "wayhouse" "log" (func $log (param i32 i32 i32)))
  (import "wayhouse" "state_get" (func $get (param i32 i32 i32 i32) (result i32)))
  (import "wayhouse" "state_put" (func $put (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "hello")
  (data (i32.const 16) "n")
  (global $bump (mut i32) (i32.const 1024))
  (func (export "alloc") (param i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get 0)))
    (local.get $p))
  {funcs})"#
        );
        wat::parse_str(src).unwrap()
    }

    /// Adds one to the byte under key `n` and logs "hello" at each tick.
    fn counter() -> Vec<u8> {
        guest(
            TIMER,
            r#"(func (export "init") (param i32 i32))
               (func (export "on_timer")
                 (call $log (i32.const 2) (i32.const 0) (i32.const 5))
                 (drop (call $get (i32.const 16) (i32.const 1) (i32.const 100) (i32.const 1)))
                 (i32.store8 (i32.const 100)
                   (i32.add (i32.load8_u (i32.const 100)) (i32.const 1)))
                 (drop (call $put (i32.const 16) (i32.const 1) (i32.const 100) (i32.const 1))))"#,
        )
    }

    fn fixture(workers: usize, queue: usize) -> (Arc<Runner>, PluginStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let store = PluginStore::open(&db).unwrap();
        let host = Arc::new(PluginHost::new(Limits::default()).unwrap());
        let pool =
            Arc::new(CompilePool::new(host, workers, queue, Duration::from_secs(10)).unwrap());
        (Runner::new(store.clone(), pool), store, dir)
    }

    fn install(store: &PluginStore, id: &str, module: &[u8]) {
        let sha = format!("sha-{id}");
        store.put_blob(&sha, module).unwrap();
        store
            .create(&InstallRecord {
                id: id.into(),
                name: "demo".into(),
                sha256: sha,
                size: module.len(),
                approved: inspect(module).unwrap().caps,
                config: serde_json::json!({}),
                enabled: true,
                created_at: 1,
                created_by: None,
            })
            .unwrap();
    }

    fn counter_value(store: &PluginStore, id: &str) -> Option<u8> {
        store.state(id).unwrap().1.get("n").map(|v| v[0])
    }

    fn secs(t0: Instant, s: u64) -> Instant {
        t0 + Duration::from_secs(s)
    }

    #[tokio::test]
    async fn ticks_once_per_interval_and_keeps_state() {
        let (r, store, _d) = fixture(1, 4);
        install(&store, "a", &counter());
        let t0 = Instant::now();
        r.run_due(t0).await;
        assert_eq!(
            counter_value(&store, "a"),
            None,
            "first tick is one interval later"
        );
        r.run_due(secs(t0, 29)).await;
        assert_eq!(counter_value(&store, "a"), None);
        r.run_due(secs(t0, 30)).await;
        assert_eq!(counter_value(&store, "a"), Some(1));
        r.run_due(secs(t0, 31)).await;
        assert_eq!(counter_value(&store, "a"), Some(1), "not due again yet");
        r.run_due(secs(t0, 60)).await;
        assert_eq!(counter_value(&store, "a"), Some(2));
        let st = r.status("a").unwrap();
        assert_eq!(
            (st.ticks, st.last_ok, st.consecutive_failures),
            (2, Some(true), 0)
        );
        assert!(st
            .logs
            .iter()
            .all(|l| l.msg == "hello" && l.level == "info"));
        assert_eq!(st.logs.len(), 2);
    }

    #[tokio::test]
    async fn a_disabled_install_is_not_ticked_and_waits_again_when_enabled() {
        let (r, store, _d) = fixture(1, 4);
        install(&store, "a", &counter());
        let t0 = Instant::now();
        r.run_due(t0).await;
        store.set_enabled("a", false).unwrap();
        r.run_due(secs(t0, 60)).await;
        assert_eq!(counter_value(&store, "a"), None);
        store.set_enabled("a", true).unwrap();
        r.run_due(secs(t0, 61)).await;
        assert_eq!(
            counter_value(&store, "a"),
            None,
            "re-enabling restarts the grace period"
        );
        r.run_due(secs(t0, 91)).await;
        assert_eq!(counter_value(&store, "a"), Some(1));
    }

    #[tokio::test]
    async fn a_plugin_without_on_timer_is_never_ticked() {
        let (r, store, _d) = fixture(1, 4);
        install(
            &store,
            "a",
            &guest(NO_TIMER, r#"(func (export "init") (param i32 i32))"#),
        );
        let t0 = Instant::now();
        r.run_due(t0).await;
        r.run_due(secs(t0, 1000)).await;
        assert!(r.status("a").is_none());
    }

    #[tokio::test]
    async fn a_trapping_plugin_keeps_its_state_and_ticks_again_next_interval() {
        let (r, store, _d) = fixture(1, 4);
        install(
            &store,
            "a",
            &guest(
                TIMER,
                r#"(func (export "init") (param i32 i32))
                   (func (export "on_timer")
                     (drop (call $put (i32.const 16) (i32.const 1) (i32.const 0) (i32.const 1)))
                     unreachable)"#,
            ),
        );
        let t0 = Instant::now();
        r.run_due(t0).await;
        r.run_due(secs(t0, 30)).await;
        let st = r.status("a").unwrap();
        assert_eq!(
            (st.last_ok, st.consecutive_failures, st.ticks),
            (Some(false), 1, 1)
        );
        assert!(st.last_error.as_deref().is_some_and(|e| e.contains("trap")));
        assert!(
            store.state("a").unwrap().1.is_empty(),
            "a failed call commits nothing"
        );
        r.run_due(secs(t0, 45)).await;
        assert_eq!(r.status("a").unwrap().ticks, 1, "no hot loop");
        r.run_due(secs(t0, 60)).await;
        assert_eq!(r.status("a").unwrap().consecutive_failures, 2);
    }

    #[tokio::test]
    async fn deleting_an_install_stops_it_and_drops_its_state_and_status() {
        let (r, store, _d) = fixture(1, 4);
        install(&store, "a", &counter());
        let t0 = Instant::now();
        r.run_due(t0).await;
        r.run_due(secs(t0, 30)).await;
        assert!(r.status("a").is_some());
        store.delete("a").unwrap();
        r.run_due(secs(t0, 60)).await;
        assert!(r.status("a").is_none());
        assert_eq!(store.state("a").unwrap().0, 0);
    }

    #[tokio::test]
    async fn a_commit_made_stale_is_dropped_and_reported() {
        let (r, store, _d) = fixture(1, 4);
        install(&store, "a", &counter());
        store
            .commit_state("a", 0, &BTreeMap::from([("n".to_string(), vec![7])]))
            .unwrap();
        // A call that read revision 0 finishes after the state moved to revision 1.
        let fx = Effects {
            state_puts: BTreeMap::from([("n".to_string(), vec![9])]),
            ..Effects::default()
        };
        r.finish("a", Outcome::Effects(0, fx));
        assert_eq!(counter_value(&store, "a"), Some(7));
        let st = r.status("a").unwrap();
        assert_eq!(st.last_ok, Some(false));
        assert!(st.last_error.unwrap().contains("changed"));
    }

    #[tokio::test]
    async fn a_busy_pool_skips_the_tick_and_says_so() {
        // One worker and no queue: while the worker is held, every submission is Busy.
        let (r, store, _d) = fixture(1, 0);
        install(&store, "a", &counter());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let pool = r.pool.clone();
        let hold = std::thread::spawn(move || {
            pool.call(move || {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv();
            })
        });
        started_rx.recv().unwrap();
        let t0 = Instant::now();
        r.run_due(t0).await;
        r.run_due(secs(t0, 30)).await;
        let st = r.status("a").unwrap();
        assert_eq!(st.last_ok, Some(false));
        assert!(st.last_error.unwrap().contains("busy"));
        assert_eq!(counter_value(&store, "a"), None);
        release_tx.send(()).unwrap();
        hold.join().unwrap().unwrap();
    }
}
