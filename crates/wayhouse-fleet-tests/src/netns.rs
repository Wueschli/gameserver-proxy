//! A rootless network-namespace lab for the phase-14 tunnel e2e test
//! (`docs/superpowers/specs/2026-10-01-tunnel-e2e-design.md`).
//!
//! The test process itself runs inside an outer user+net+mount namespace
//! (`make tunnel-e2e` → `unshare -Urnm`) — that is the "lab". Each [`Ns`]
//! is a further, nested network namespace kept alive by a `sleep infinity`
//! "holder" process and addressed by that process's PID (`ip netns` is
//! avoided: it needs a writable `/var/run/netns`). Namespace *i* is joined
//! to the lab by a veth pair (`10.99.i.1/30` lab side, `10.99.i.2/30` inside,
//! plus `fd99:i::1/64` and `fd99:i::2/64` for the IPv6 underlay);
//! the lab forwards between namespaces, so traffic from one to another
//! really crosses the lab like it would cross the internet.

use std::fs::File;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context, Result};
use nix::sched::{setns, CloneFlags};

/// Namespace indices are handed out process-wide and never reused within one
/// process: a dead namespace's veth is torn down asynchronously by the kernel, so
/// a later `Lab` reusing `gv1l`/`10.99.1.0/30` would race that cleanup. Other
/// processes sharing the lab (every `cargo nextest` test is its own process, all
/// restarting at 1) are handled by [`Lab::add_ns`] skipping indices whose veth
/// already exists.
static NEXT_IDX: AtomicU8 = AtomicU8::new(1);

const HINT: &str = "this test needs CAP_NET_ADMIN in a network namespace — run it via \
                    `make tunnel-e2e` (which wraps it in `unshare -Urnm`), not plain `cargo test`";

fn run(argv: &[&str]) -> Result<String> {
    let out = Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("running {argv:?}"))?;
    ensure!(
        out.status.success(),
        "{argv:?} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `probe`; if it fails, say how to run this test properly.
pub fn require_lab_with(probe: &[&str]) -> Result<()> {
    run(probe).map(|_| ()).context(HINT)
}

/// One nested network namespace, held open by a `sleep` process.
pub struct Ns {
    idx: u8,
    pid: u32,
    holder: Child,
}

impl Ns {
    fn create(idx: u8) -> Result<Self> {
        // stdio must be null: an inherited pipe would keep any caller that is
        // reading our output (e.g. `$(...)`) waiting for the holder to exit.
        let holder = Command::new("unshare")
            .args(["--net", "--", "sleep", "infinity"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning `unshare --net -- sleep infinity` (is util-linux installed?)")?;
        let ns = Self {
            idx,
            pid: holder.id(),
            holder,
        };
        // `unshare` exec()s `sleep` in the *same* pid; wait until that pid's
        // netns differs from ours so we never wire up the lab's own namespace.
        let ours = std::fs::read_link("/proc/self/ns/net")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match std::fs::read_link(ns.ns_path()) {
                Ok(theirs) if theirs != ours => return Ok(ns),
                _ if Instant::now() >= deadline => {
                    bail!("namespace holder (pid {}) never left our netns", ns.pid)
                }
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    pub fn idx(&self) -> u8 {
        self.idx
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn ns_path(&self) -> String {
        format!("/proc/{}/ns/net", self.pid)
    }

    /// This namespace's address on its veth to the lab.
    pub fn underlay(&self) -> Ipv4Addr {
        Ipv4Addr::new(10, 99, self.idx, 2)
    }

    /// The lab's address on the same veth (what this namespace uses as gateway).
    pub fn lab_addr(&self) -> Ipv4Addr {
        Ipv4Addr::new(10, 99, self.idx, 1)
    }

    /// This namespace's IPv6 address on its veth to the lab.
    pub fn underlay6(&self) -> Ipv6Addr {
        Ipv6Addr::new(0xfd99, u16::from(self.idx), 0, 0, 0, 0, 0, 2)
    }

    /// The lab's IPv6 address on the same veth (this namespace's IPv6 gateway).
    pub fn lab_addr6(&self) -> Ipv6Addr {
        Ipv6Addr::new(0xfd99, u16::from(self.idx), 0, 0, 0, 0, 0, 1)
    }

    /// Run a command inside this namespace, returning its stdout.
    pub fn run(&self, argv: &[&str]) -> Result<String> {
        let net = format!("--net={}", self.ns_path());
        let mut full = vec!["nsenter", net.as_str(), "--"];
        full.extend_from_slice(argv);
        run(&full)
    }

    /// Run `f` on a fresh thread that has joined this namespace. Network
    /// namespace membership is per-thread, so the caller's thread is untouched.
    pub fn in_ns<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_thread(f)?
            .join()
            .map_err(|_| anyhow!("thread in namespace {} panicked", self.idx))?
    }

    /// Like [`Ns::in_ns`] but returns the handle so `f` can run for a while.
    pub fn spawn_thread<T, F>(&self, f: F) -> Result<JoinHandle<Result<T>>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let file = File::open(self.ns_path()).context("opening the namespace file")?;
        Ok(std::thread::spawn(move || {
            setns(&file, CloneFlags::CLONE_NEWNET).context("setns into the namespace")?;
            Ok(f())
        }))
    }
}

impl Drop for Ns {
    fn drop(&mut self) {
        let _ = self.holder.kill();
        let _ = self.holder.wait();
    }
}

/// The outer namespace the test process runs in; nested namespaces are wired
/// to it with [`Lab::add_ns`].
pub struct Lab {
    _private: (),
}

impl Lab {
    pub fn new() -> Result<Self> {
        require_lab_with(&["ip", "link", "add", "wayhouse-probe0", "type", "dummy"])?;
        run(&["ip", "link", "del", "wayhouse-probe0"])?;
        run(&["ip", "link", "set", "lo", "up"])?;
        std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")
            .context("enabling ip_forward in the lab namespace")?;
        std::fs::write("/proc/sys/net/ipv6/conf/all/forwarding", "1")
            .context("enabling IPv6 forwarding in the lab namespace (is IPv6 available?)")?;
        Ok(Self { _private: () })
    }

    /// Create the next namespace and join it to the lab with a veth pair.
    pub fn add_ns(&mut self) -> Result<Ns> {
        // Claim the first free index. `ip link add` is atomic in the kernel, so
        // "File exists" means another process (or a namespace whose veth is still
        // being torn down) owns this index: try the next one.
        let (i, lab_if, ns_if) = loop {
            let i = NEXT_IDX.fetch_add(1, Ordering::SeqCst);
            ensure!(i < 250, "too many namespaces created in one test run");
            let (lab_if, ns_if) = (format!("gv{i}l"), format!("gv{i}n"));
            match run(&[
                "ip", "link", "add", &lab_if, "type", "veth", "peer", "name", &ns_if,
            ]) {
                Ok(_) => break (i, lab_if, ns_if),
                Err(e) if e.to_string().contains("File exists") => continue,
                Err(e) => return Err(e),
            }
        };
        let ns = match Ns::create(i) {
            Ok(ns) => ns,
            Err(e) => {
                let _ = run(&["ip", "link", "del", &lab_if]);
                return Err(e);
            }
        };
        run(&["ip", "link", "set", &ns_if, "netns", &ns.pid().to_string()])?;
        run(&[
            "ip",
            "addr",
            "add",
            &format!("10.99.{i}.1/30"),
            "dev",
            &lab_if,
        ])?;
        // `nodad`: usable at once, not held "tentative" for a second.
        let lab6 = format!("{}/64", ns.lab_addr6());
        run(&["ip", "-6", "addr", "add", &lab6, "dev", &lab_if, "nodad"])?;
        run(&["ip", "link", "set", &lab_if, "up"])?;
        ns.run(&["ip", "link", "set", "lo", "up"])?;
        ns.run(&[
            "ip",
            "addr",
            "add",
            &format!("10.99.{i}.2/30"),
            "dev",
            &ns_if,
        ])?;
        let ns6 = format!("{}/64", ns.underlay6());
        ns.run(&["ip", "-6", "addr", "add", &ns6, "dev", &ns_if, "nodad"])?;
        ns.run(&["ip", "link", "set", &ns_if, "up"])?;
        let gw6 = ns.lab_addr6().to_string();
        ns.run(&["ip", "-6", "route", "add", "default", "via", &gw6])?;
        ns.run(&[
            "ip",
            "route",
            "add",
            "default",
            "via",
            &format!("10.99.{i}.1"),
        ])?;
        Ok(ns)
    }
}
