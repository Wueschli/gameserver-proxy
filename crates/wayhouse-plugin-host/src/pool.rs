//! A bounded pool for compiling plugin modules off the caller's thread.
//!
//! Compilation cannot be interrupted once started, so the bounds are: a fixed number of
//! worker threads, a bounded queue (a full queue answers [`PoolError::Busy`] at once),
//! and a caller-side timeout ([`PoolError::TimedOut`]). A timed-out job still occupies
//! its worker until it finishes; it does not occupy the caller or the queue.

use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;

use crate::caps::Capabilities;
use crate::module::ModuleError;
use crate::runtime::{Plugin, PluginHost};

type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error(transparent)]
    Module(#[from] ModuleError),
    #[error("the compile queue is full")]
    Busy,
    #[error("compiling took longer than the timeout")]
    TimedOut,
}

pub struct CompilePool {
    host: Arc<PluginHost>,
    sender: SyncSender<Job>,
    timeout: Duration,
}

impl CompilePool {
    /// `workers` threads (at least 1) behind a queue of `queue` waiting jobs.
    pub fn new(
        host: Arc<PluginHost>,
        workers: usize,
        queue: usize,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Job>(queue);
        let receiver = Arc::new(Mutex::new(receiver));
        for i in 0..workers.max(1) {
            let receiver = receiver.clone();
            std::thread::Builder::new()
                .name(format!("wayhouse-plugin-compile-{i}"))
                .spawn(move || loop {
                    let job = {
                        let guard = receiver
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.recv()
                    };
                    match job {
                        Ok(job) => job(),
                        Err(_) => return,
                    }
                })
                .context("spawning a plugin compile worker")?;
        }
        Ok(Self {
            host,
            sender,
            timeout,
        })
    }

    /// Compile `bytes` on a worker; see [`PluginHost::load`].
    pub fn load(&self, bytes: Vec<u8>, approved: Capabilities) -> Result<Plugin, PoolError> {
        let host = self.host.clone();
        self.run(move || host.load(&bytes, &approved))?
            .map_err(PoolError::Module)
    }

    /// Run any blocking job (a guest call) on a worker under the pool's timeout:
    /// `Busy` when the queue is full, `TimedOut` when it does not finish in time.
    pub fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, PoolError> {
        self.run(f)
    }

    pub(crate) fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, PoolError> {
        self.run_with_timeout(f, self.timeout)
    }

    pub(crate) fn run_with_timeout<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
        timeout: Duration,
    ) -> Result<T, PoolError> {
        let (tx, rx) = mpsc::channel();
        let job: Job = Box::new(move || {
            let _ = tx.send(f());
        });
        match self.sender.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                return Err(PoolError::Busy)
            }
        }
        match rx.recv_timeout(timeout) {
            Ok(v) => Ok(v),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                Err(PoolError::TimedOut)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Limits;

    fn pool(workers: usize, queue: usize, timeout: Duration) -> CompilePool {
        let host = Arc::new(PluginHost::new(Limits::default()).unwrap());
        CompilePool::new(host, workers, queue, timeout).unwrap()
    }

    fn good_module() -> Vec<u8> {
        wat::parse_str(
            r#"(module
  (@custom "wayhouse.plugin-abi" "\00\00\01\00")
  (@custom "wayhouse.plugin-caps" "{\"log\":true}")
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 0))
  (func (export "init") (param i32 i32)))"#,
        )
        .unwrap()
    }

    #[test]
    fn loads_on_a_worker_thread() {
        let p = pool(1, 1, Duration::from_secs(30));
        let name = p
            .run(|| std::thread::current().name().map(str::to_string))
            .unwrap();
        assert!(name.unwrap().starts_with("wayhouse-plugin-compile-"));
        let m = good_module();
        let approved = crate::inspect(&m).unwrap().caps;
        p.load(m, approved).unwrap();
    }

    #[test]
    fn call_runs_a_job_on_a_worker_thread() {
        let p = pool(1, 1, Duration::from_secs(5));
        let here = std::thread::current().id();
        let there = p.call(|| std::thread::current().id()).unwrap();
        assert_ne!(here, there);
    }

    #[test]
    fn module_errors_pass_through() {
        let p = pool(1, 1, Duration::from_secs(30));
        let approved = crate::inspect(&good_module()).unwrap().caps;
        assert!(matches!(
            p.load(b"junk".to_vec(), approved),
            Err(PoolError::Module(_))
        ));
    }

    /// A job that reports it has started, then blocks until the gate is dropped.
    fn gated_job(
        started: mpsc::Sender<()>,
        gate: mpsc::Receiver<()>,
    ) -> impl FnOnce() + Send + 'static {
        move || {
            let _ = started.send(());
            let _ = gate.recv();
        }
    }

    #[test]
    fn a_full_queue_answers_busy_at_once() {
        let p = Arc::new(pool(1, 1, Duration::from_secs(30)));
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        // The worker is running a job that blocks on the gate.
        let blocked = {
            let p = p.clone();
            std::thread::spawn(move || p.run(gated_job(started_tx, gate_rx)))
        };
        started_rx.recv().unwrap();
        // The one queue slot is taken by a job the busy worker cannot reach.
        p.sender.try_send(Box::new(|| ())).unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(p.run(|| ()), Err(PoolError::Busy)));
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(gate_tx);
        blocked.join().unwrap().unwrap();
    }

    #[test]
    fn a_slow_job_times_out_and_the_pool_stays_usable() {
        let p = pool(1, 1, Duration::from_secs(30));
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        // Blocks until released, so the short timeout always fires first.
        let timed_out =
            p.run_with_timeout(gated_job(started_tx, gate_rx), Duration::from_millis(100));
        assert!(matches!(timed_out, Err(PoolError::TimedOut)));
        started_rx.recv().unwrap();
        drop(gate_tx);
        // The worker finishes the abandoned job and serves the next one.
        assert_eq!(p.run(|| 7).unwrap(), 7);
    }
}
