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

    pub(crate) fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
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
        match rx.recv_timeout(self.timeout) {
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
    fn module_errors_pass_through() {
        let p = pool(1, 1, Duration::from_secs(30));
        let approved = crate::inspect(&good_module()).unwrap().caps;
        assert!(matches!(
            p.load(b"junk".to_vec(), approved),
            Err(PoolError::Module(_))
        ));
    }

    #[test]
    fn a_full_queue_answers_busy_at_once() {
        let p = Arc::new(pool(1, 1, Duration::from_millis(100)));
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let gate_rx = Arc::new(Mutex::new(gate_rx));
        // Occupy the worker and the one queue slot with jobs that wait on the gate.
        let mut waiters = Vec::new();
        for _ in 0..2 {
            let (p, g) = (p.clone(), gate_rx.clone());
            waiters.push(std::thread::spawn(move || {
                let _ = p.run(move || {
                    let _ = g.lock().unwrap().recv();
                });
            }));
        }
        std::thread::sleep(Duration::from_millis(50));
        let started = std::time::Instant::now();
        assert!(matches!(p.run(|| ()), Err(PoolError::Busy)));
        assert!(started.elapsed() < Duration::from_millis(50));
        drop(gate_tx);
        for w in waiters {
            w.join().unwrap();
        }
    }

    #[test]
    fn a_slow_job_times_out_and_the_pool_stays_usable() {
        let p = pool(1, 1, Duration::from_millis(50));
        assert!(matches!(
            p.run(|| std::thread::sleep(Duration::from_millis(300))),
            Err(PoolError::TimedOut)
        ));
        std::thread::sleep(Duration::from_millis(400));
        assert!(p.run(|| 7).is_ok());
    }
}
