# gsp-bench

Single-host latency / load harness for the NFR targets in
[`docs/01-requirements.md`](../../docs/01-requirements.md). Two modes.

## `latency` mode (default)

```sh
make bench                                   # both transports, defaults
make bench BENCH_ARGS="--protocol tcp --iterations 50000"
cargo run --release -p gsp-bench -- --help
```

Client, the proxy's `Runtime`, and the backend all run inside this one
process's tokio runtime — fast, but structurally blind to anything that only
shows up with a real, separate proxy process under real concurrent load (see
`concurrency` mode below for that).

| output | NFR | how |
|--------|-----|-----|
| **added p50 / p99** request→response latency (direct vs. through an in-process proxy), `PASS`/`MISS` | **N1** `< 0.5 ms`, **N2** `< 2 ms` | many sequential round-trips on one connection; `added = proxy − direct` |
| single-stream throughput, direct vs. proxy | (context for N3) | one connection, 512 MiB each way, MiB/s |
| idle RSS growth per held connection (with `--connections ≥ 1000`) | (loose N8) | `VmRSS` delta / connection count |

`--connections N` holds N busy request/response connections open during the
measurement to add scheduler contention; the timed connection is separate. Use
it for stress/soak observation — with a low worker count it will saturate the
box and the `direct` baseline degrades too, so the N1/N2 verdict is only
meaningful at little/no background load (which matches the NFR's intent).

`--strict` exits non-zero on a `MISS`.

## `concurrency` mode

```sh
cargo run --release -p gsp-bench -- --mode concurrency
cargo run --release -p gsp-bench -- --mode concurrency --protocol tcp --steps 1000,5000,20000
cargo run --release -p gsp-bench -- --mode concurrency --protocol udp --steps 1000,10000
```

Builds the real `gsp` binary (`cargo build --release -p gsp`) and spawns it as
a genuine **separate OS process**, then ramps a real client-held connection
count through `--steps` (each step's connections stay open through later
steps), sampling at each step:

- the **proxy child's own** RSS and open-fd count, read straight from
  `/proc/<pid>/...` — meaningful now that the proxy is a distinct process,
  not a number shared with (and polluted by) this harness's own client-side
  sockets;
- **added p50/p99 latency** of a short probe connection, the same N1/N2 check
  as `latency` mode, but taken while thousands of other connections are held
  open through a real, separate proxy process.

Backends stay in-process (this mode isn't testing backend scaling); only the
proxy under test is out-of-process. `--workers` sets the *proxy's*
`settings.workers`.

### What this does and doesn't prove

This is still loopback on one host, so it says nothing about **N3** (a real
NIC's aggregate bandwidth — loopback bandwidth is typically *higher* than the
20 Gbit/s target, so a local "pass" would be meaningless) or **N9** (real
multi-host HA/failover). And the connection *count* it can reach is capped
well below the **N4/N5** targets (500k conns / 1M sessions) by the client's
own ephemeral-port range for outbound connections to one destination
(`cat /proc/sys/net/ipv4/ip_local_port_range` — typically ~28k on Linux
without extra source addresses); pushing past that needs multiple client
source addresses/processes, which this mode doesn't attempt.

What it *does* give, that `latency` mode structurally can't: a real, separate
proxy process under real concurrent load, at a scale far beyond one measured
connection — enough to surface most classes of concurrency bug (accept-queue
backpressure, fd/allocator behaviour, lock contention under many live
connections) even though it can't reach the full NFR numbers. See
`crates/gsp-bench/src/concurrency.rs`'s module doc for the full reasoning,
and `docs/06-operations-observability.md` for where this sits relative to a
real load-test setup.

**A methodology note surfaced while building this**: the in-process backend
echo listener needs an explicit large `listen()` backlog (this crate binds it
via `socket2` with a backlog of 4096) — the OS default (128) turns the
*backend's* accept queue into the bottleneck once the proxy tries to open
hundreds of new backend connections at once, timing out the proxy's own
`connect_timeout_ms` and flapping the backend passively unhealthy. That's a
benchmark-harness artifact, not a proxy behaviour — worth remembering if you
extend this further.

## Out of scope (both modes)

**N3** (≥ 20 Gbit/s aggregate) and **N9** (real HA) need real NICs, multiple
hosts and a dedicated load generator (`tcpkali`, `wrk2`, `iperf3`). **N4/N5**
(500k conns / 1M sessions) at their full scale need multiple client source
addresses/hosts too — `concurrency` mode gets partway there (tens of
thousands, single host) but not the full number. This crate is the fast "did
a change regress the per-connection overhead, or a real proxy's behaviour
under real concurrent load?" check, not a substitute for a dedicated load-test
environment.
