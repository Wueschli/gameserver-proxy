# gsp-bench

Single-host latency / load harness for the NFR targets in
[`docs/01-requirements.md`](../../docs/01-requirements.md).

```sh
make bench                                   # both transports, defaults
make bench BENCH_ARGS="--protocol tcp --iterations 50000"
cargo run --release -p gsp-bench -- --help
```

## What it measures

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

## Out of scope

Loopback on one host isn't a same-DC network, so treat absolute numbers as a
floor. **N3** (≥ 20 Gbit/s aggregate), **N4/N5** (500k conns / 1M sessions) and
**N9** (HA) need real NICs, multiple hosts and a dedicated load generator
(`tcpkali`, `wrk2`, `iperf3`). This harness is the fast "did a change regress
the per-connection overhead?" check.
