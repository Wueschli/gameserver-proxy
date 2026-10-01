# 12 – Container images & deployment

How to package the five binaries for Docker/Kubernetes, and — the part that
isn't obvious going in — how a proxy that needs to listen on many, changing
ports actually works once it's namespaced by a container runtime.

## Container images

Build with a standard multi-stage Dockerfile: a `rust:<version>` stage runs
`cargo build --release`, then the final stage copies just the binary onto a
minimal base. None of these five binaries shell out to an external
command at runtime (WireGuard interface management in `gsp`/`gsp-agent`
goes through kernel netlink directly via `defguard/wireguard-rs`, not the
`ip`/`wg` CLIs) — so the runtime image needs nothing but the binary and its
dynamic library dependencies (glibc, plus `ca-certificates` for any
TLS-verifying HTTP client: `--controller`, `--aggregator`,
`--tunnel-controller-url`, the HTTP/gRPC resolvers).

Add `strip = true` to the root `Cargo.toml`'s `[profile.release]` before
packaging — it isn't set today, and stripping trims roughly 15-20% off
every binary for free (no behavior change).

Measured image sizes (stripped release binaries, current `main`,
2026-09-07):

| Binary | on `debian:trixie-slim` + `ca-certificates` | on `gcr.io/distroless/cc-debian12` |
|---|---|---|
| `gsp` | 161 MB | **65 MB** |
| `gsp-controller` | 148 MB | **52 MB** |
| `gsp-agent` | 143 MB | **47 MB** |
| `gsp-ui` | 141 MB | **45 MB** |
| `gsp-aggregator` | 141 MB | **45 MB** |

The base image dominates, not the binary — `debian:trixie-slim` alone is
~118 MB before anything is added; `distroless/cc-debian12` (glibc +
libgcc/libstdc++, no shell, no package manager) is a fraction of that and
was confirmed to actually run these binaries (no runtime shell-out, so
distroless's missing shell is never a problem). Trade-off: no
`docker exec ... sh` for interactive debugging, and the `nonroot` variant
runs as a fixed non-root uid (fine here — nothing in this project needs
root once its sockets are bound; see `--tunnel-*`'s capability
requirements below for the one exception).

`gsp` and `gsp-controller` are the two heaviest either way, for real
reasons: `gsp` links `wasmtime` (phase 9 sniffer plugins) and
`defguard_wireguard_rs`/`boringtun` (phase 14 backend transport);
`gsp-controller` links `openraft` + `sled` for the HA/Raft log store
(phase 12). `gsp-agent`/`gsp-aggregator`/`gsp-ui` don't carry those.

## Networking: a proxy that binds many, changing ports

`gsp` is designed to add and remove listeners live — `ListenerManager`
(`crates/gsp-core/src/listeners.rs`) reconciles the running listener set
from config on every SIGHUP/reload, and `bind: "0.0.0.0:30000-30999"`
(`docs/01` F1.4, `docs/08` roadmap) spawns one real socket per port in a
range under a single listener config, for fleets that hand out one port
per match/instance. None of that is Docker- or Kubernetes-aware — it's
just `bind()`/`listen()` calls inside the container's network namespace.
**And that's exactly the problem**: neither platform lets a running
container dynamically "claim" a new externally-reachable port just by
calling `bind()`. Port exposure is a static declaration in both:

- **Docker**: `-p hostport:containerport` sets up a NAT/iptables rule at
  container start. `EXPOSE`/`-P` is documentation, not a live mechanism.
  There's no API for a running container to add another published port —
  the container has to be recreated with a new `-p` list.
- **Kubernetes**: a `Service`'s `spec.ports` is a fixed list, and a
  container's own `ports:` field in the Pod spec is documentation only
  (Kubernetes doesn't firewall ports you didn't declare, and it doesn't
  publish ones you didn't declare either). Nothing watches a container's
  live sockets and reconciles a `Service` to match them.

Two patterns actually work, and `gsp` already has one foot in each:

1. **Host networking** (`docker run --network host` /
   `hostNetwork: true` on the Pod) — the container shares the host's
   network namespace directly, so every `bind()` `gsp` does is immediately
   live with zero orchestration involvement. This is the closest match to
   `gsp`'s own reload-driven listener reconciliation: add a `bind:` entry,
   send SIGHUP (or let file-watch pick it up), and the new listener exists
   — in a container exactly like bare metal. Cost: no network-namespace
   isolation for that pod/container — an acceptable, often expected,
   trade-off for the thing that sits at the actual network edge.
2. **A pre-reserved port range** — publish/expose a large contiguous range
   up front (Docker supports a range: `-p 20000-40000:20000-40000/udp`;
   Kubernetes needs either `hostNetwork` scoped to a node-level reserved
   range, or enumerating `hostPort` per container — there's no native range
   syntax in a Pod spec) and only actually `bind()` the subset needed at
   any given time via `gsp`'s own `bind: "host:lo-hi"` range listener. This
   is the same shape **Agones** (the standard Kubernetes game-server
   operator) uses for its `GameServer` pods — `hostNetwork: true` plus one
   port pulled from a node-level reserved pool — because Kubernetes
   Services can't do dynamic port publishing either; `docs/08`'s roadmap
   entry for the range-bind feature draws this comparison explicitly.

Host networking is the simpler default when the proxy doesn't need
network-namespace isolation from other workloads on the node (usually
true for an edge process). Reach for the reserved-range pattern when
isolation is a hard requirement, or when running many `gsp` instances per
node and the orchestrator should own port allocation/collision-avoidance
across them.

## `--tunnel-*` (phase 14): `CAP_NET_ADMIN` and `/dev/net/tun`

`gsp --tunnel-*` and `gsp-agent` create a real WireGuard interface
(`docs/11`) — kernel-netlink by default, falling back to `boringtun`
userspace with `--tunnel-userspace`/`--userspace`. **Both backends need
`CAP_NET_ADMIN`**, and the userspace fallback additionally needs
`/dev/net/tun` present in the container. Grant them explicitly:

```
docker run --cap-add=NET_ADMIN --device=/dev/net/tun ...
```

```yaml
securityContext:
  capabilities:
    add: ["NET_ADMIN"]
volumes:
  - name: tun
    hostPath: { path: /dev/net/tun }
```

This is a real, already-verified requirement, not a guess: `docs/08`
phase 14 slice 6's end-to-end verification ran in exactly this
container shape (`crates/gsp-agent` + `gsp --tunnel-*` in Docker
containers with `NET_ADMIN` + `/dev/net/tun`) because the project's own
dev sandbox lacked `CAP_NET_ADMIN` even in the bounding set. A container
without both of these fails loud at interface bring-up, before any
listener binds or backend source is built — it does not silently run
without tunnel support.

The CI-runnable `make tunnel-e2e` (rootless network namespaces,
`docs/superpowers/specs/2026-10-01-tunnel-e2e-design.md`) verifies the tunnel
*logic* with the same binaries and flags, but it does not exercise container
capabilities — `NET_ADMIN` and `/dev/net/tun` as documented above stay the
deployment requirement.

## Open question

`docs/11`'s "Open questions" section separately flags tunnel-internal
address collision/exhaustion at fleet scale — a related but distinct
concern from this document's container networking question (that one is
about the WireGuard-internal `10.x.x.x`-style addressing between proxies
and origins; this document is about the container/orchestrator-facing side
of the same proxy's public listeners).
