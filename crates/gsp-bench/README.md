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

## Measured on this box

Reference numbers from one run of each mode, so the shape of the results is
inspectable without re-running them. Machine: 16 cores, 27 GiB RAM, loopback,
`--release`. Config for the concurrency ramp is minimal (one pool, a bare
`pool:` route — no `routes:` list, ACL, rate limiter, GeoIP, or sniffer on the
per-connection path); a production config with those enabled will cost more
than this baseline (see the "latency ledger" in `HANDOVER.md` for what each
feature adds).

### `latency` mode

```
TCP request/response  (5000 samples)
            mean       p50       p90       p99     p99.9       max
  direct    8.1µ      7.1µ      9.9µ     17.1µ     56.7µ    521.1µ
  proxy    10.2µ     10.3µ     11.9µ     20.9µ     40.4µ     56.0µ
  added    p50 +3.2µs   p99 +3.8µs

UDP request/response  (5000 samples)
            mean       p50       p90       p99     p99.9       max
  direct    6.6µ      5.9µ      8.2µ     15.7µ     51.2µ    280.5µ
  proxy    11.5µ      9.5µ     16.1µ     29.7µ     84.6µ    118.2µ
  added    p50 +3.6µs   p99 +14.0µs

throughput (1 stream, informational): direct 10970 MiB/s   proxy 6479 MiB/s

NFR N1 (added p50 < 500µs) / N2 (added p99 < 2000µs): PASS
```

### `concurrency` mode

TCP, `--steps 1000,5000,10000,20000`:

| held | connected | failed | proxy RSS | proxy fds | added p50 | added p99 |
|-----:|----------:|-------:|----------:|----------:|----------:|----------:|
| 1,000 | 1,000 | 0 | 30.6 MiB | 2,014 | 14.5µs | 61.0µs |
| 5,000 | 5,000 | 0 | 112.3 MiB | 10,014 | 10.1µs | 27.0µs |
| 10,000 | 9,801 | 0 | 212.5 MiB | 19,512 | 9.3µs | 33.0µs |
| 20,000 | 20,000 | 0 | 426.3 MiB | 40,014 | 4.9µs | 6.1µs |

UDP, `--steps 1000,5000`:

| held | connected | failed | proxy RSS | proxy fds | added p50 | added p99 |
|-----:|----------:|-------:|----------:|----------:|----------:|----------:|
| 1,000 | 968 | 32 | 17.9 MiB | 984 | 4.3µs | −0.6µs* |
| 5,000 | 4,936 | 64 | 55.7 MiB | 4,953 | 6.0µs | 22.6µs |

\* noise at low sample count on a fast box, not a real negative overhead.

**Reading these**: RSS scales roughly linearly (~21 KiB/TCP connection at
20k), fds track ~2/connection (client + backend socket) as expected. The
important line isn't any single number — it's that **added latency does not
degrade as concurrency climbs from 1k to 20k**; it stays in the
single-digit-to-tens-of-µs range the whole way (if anything it drops
slightly, plausibly cache/scheduler warm-up), with no sign of the O(n) growth
or lock-contention creep that would show up as p99 climbing with connection
count. That is the main thing this mode is for: catching that failure shape,
not just producing a number. The small UDP failure rate (1–3%) is a
benchmark-harness artifact — a 300 ms session-establishment confirm timeout
racing the burst — not a proxy-side failure.

## Out of scope (both modes)

**N3** (≥ 20 Gbit/s aggregate) and **N9** (real HA) need real NICs, multiple
hosts and a dedicated load generator (`tcpkali`, `wrk2`, `iperf3`). **N4/N5**
(500k conns / 1M sessions) at their full scale need multiple client source
addresses/hosts too — `concurrency` mode gets partway there (tens of
thousands, single host) but not the full number. This crate is the fast "did
a change regress the per-connection overhead, or a real proxy's behaviour
under real concurrent load?" check, not a substitute for a dedicated load-test
environment.
