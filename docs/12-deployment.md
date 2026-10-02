# 12 – Container images & deployment

How to package the five binaries for Docker/Kubernetes, and — the part that
isn't obvious going in — how a proxy that needs to listen on many, changing
ports actually works once it's namespaced by a container runtime.

## Container images

The reference build is [`deploy/Dockerfile`](../deploy/Dockerfile): one file, a
shared `rust:1-trixie` builder and five runtime targets on
`gcr.io/distroless/cc-debian13:nonroot` (see [`deploy/README.md`](../deploy/README.md);
`make deploy-images` builds all five). None of these five binaries shell out to an
external command at runtime (WireGuard interface management in `gsp`/`gsp-agent`
goes through kernel netlink directly via `defguard/wireguard-rs`, not the
`ip`/`wg` CLIs) — so the runtime image needs nothing but the binary and its
dynamic library dependencies (glibc). It does not need a system CA bundle either: the
HTTP clients (`--controller`, `--aggregator`, `--tunnel-controller-url`, the HTTP
resolvers) verify against a root bundle compiled into the binary, plus whatever
`--ca-file` adds — see
[TLS for the fleet services](#tls-for-the-fleet-services) for what that implies. The builder and runtime base
must be on the same Debian release, or a binary can fail to start on an older glibc.

`[profile.release]` sets `strip = true`, which trims roughly 15-20% off every binary.

Measured image sizes (`docker images`, stripped release binaries, `deploy/Dockerfile`
on `distroless/cc-debian13:nonroot`, CI run 36922142697, 2026-10-01):

| Binary | image size |
|---|---|
| `gsp` | **47.4 MB** |
| `gsp-controller` | **37.9 MB** |
| `gsp-agent` | 34.3 MB |
| `gsp-ui` | 34.3 MB (includes the built frontend) |
| `gsp-aggregator` | 33.3 MB |

For comparison, an earlier 2026-09-07 measurement on `cc-debian12` gave 45-65 MB.

The base image dominates, not the binary — `debian:trixie-slim` alone is
~118 MB before anything is added; `distroless/cc-debian13` (glibc +
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

## Examples

[`deploy/compose/`](../deploy/compose/) is a runnable control-plane demo (controller,
aggregator, UI, one `gsp` pulling its config from the controller) with a tunnel
override; [`deploy/k8s/`](../deploy/k8s/) has plain manifests (the proxy as a
`hostNetwork` DaemonSet). Both are reference only. CI smoke-tests the compose demo and schema-validates the manifests (the `deploy`
job, blocking since 2026-10-02; first green run 2026-10-01). Known limitation: aggregator intent fan-out (drain etc.) cannot reach
a `gsp` from these examples, because the `admin_url` it reports is derived from
`settings.admin.listen` and no flag overrides it.

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
docker run --user 0 --cap-add=NET_ADMIN --device=/dev/net/tun ...
```

`--user 0` matters for the nonroot images in `deploy/`: Docker and Kubernetes put an
added capability only in a *non-root* process's bounding set, so uid 65532 would still
get `EPERM` creating the interface.

```yaml
securityContext:
  runAsUser: 0
  runAsNonRoot: false
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

## TLS for the fleet services

Without the settings below, `gsp-controller`, `gsp-aggregator`, `gsp-ui` and `gsp`'s
admin API serve **plain HTTP**. Run as-is across a network, that exposes:

- the `--auth-token` bearer token on every request, and the `/admin/adopt` calls;
- the full config text, on `GET /config` and the SSE `GET /config/subscribe`;
- every origin's registration (WireGuard public key, public endpoint, fronted
  backend addresses) on the `/peers*` and `/proxy-peers*` routes.

Two ways to encrypt it: **native TLS** (below), or a **reverse proxy
you run that terminates TLS**, with the controller listening only on loopback or a
private network (`--listen 127.0.0.1:9901`, or a private bridge/pod network). The
aggregator, the UI and `gsp`'s admin API serve native TLS the same way.

### Native TLS

```sh
gsp-controller --listen 0.0.0.0:8443 \
  --tls-cert /etc/gsp/tls/fullchain.pem --tls-key /etc/gsp/tls/privkey.pem
```

- `--tls-cert` is a PEM chain, leaf first (a Let's Encrypt `fullchain.pem` works);
  `--tls-key` a PEM private key (PKCS#8, SEC1 or PKCS#1). Both or neither. With them,
  `--listen` speaks **HTTPS only** — every route, including the SSE streams.
- **Renewal without a restart:** both files are checked every 30 s and a changed pair is
  swapped in for new connections. A broken or half-written pair (no certificate, no
  key, key not matching the certificate) is logged and the current certificate stays in
  service until a good pair appears. Bad files at **startup** stop the controller with
  an error naming the file.
- Clients use `https://` URLs; with a private CA they add `--ca-file`. HA replicas can
  each serve native TLS and name their peers `--ha-peers 1=https://ctl-1:8443,…`
  (plus `--ca-file`), with no terminator at all.
- TLS handshakes run in their own tasks with a 10 s timeout, so a client that connects
  and stalls cannot hold up others. No client certificates (mTLS).
- **`gsp-aggregator`** takes the same two flags with the same behaviour. Instances then
  push to `--aggregator https://…` (plus `--ca-file` for a private CA), and the same
  goes for a child tier's `--parent-url` and `gsp-ui --aggregator-url`.
- **`gsp-ui`** takes the same two flags. Serving HTTPS itself, it marks the session
  cookie `Secure` (without the flags it doesn't — a browser drops a `Secure` cookie on
  `http://` and login would loop). The live view's WebSocket works over HTTP/2 too:
  a browser on h2 opens it as an extended `CONNECT`, which the UI accepts (and the
  session cookie counts in whichever `cookie` header h2 splits it into). Plain
  HTTP is not redirected; serve only HTTPS on the port browsers use.
- **`gsp`'s admin API** is configured in the YAML, next to `listen`:
  `settings.admin.tls: { cert: <chain.pem>, key: <key.pem> }` (both required;
  startup-only like `listen`; the files renew like the flags above; `gsp --check`
  loads them). The admin URL `gsp` reports to the aggregator then becomes
  `https://<settings.admin.listen>`, so the certificate needs that address as a SAN
  (an IP SAN for an IP `listen`), and the aggregator takes `--ca-file` for a private
  CA. As before, the reported URL is the literal `listen` address — a wildcard bind
  (`0.0.0.0`) is not reachable as an admin URL, TLS or not.

Verified by `gsp-fleet-tests`: `controller_native_tls.rs` (`gsp --check` fails on
`UnknownIssuer` without `--ca-file` and passes with it; `/config/subscribe` streams over
TLS; `--tls-cert` alone is refused), `aggregator_native_tls.rs` (a `gsp` pushes to an
`https://` aggregator, read back over HTTPS), `ui_native_tls.rs` (login over HTTPS
sets a `Secure` cookie that unlocks `/ui/session`), `admin_native_tls.rs` (an
aggregator fans an intent verb out to an `https://` admin API; `gsp --check` names a
bad `settings.admin.tls.cert`) and `ha_tls.rs`
`three_replicas_replicate_over_native_tls`; certificate loading, renewal and the
listener are unit-tested in `crates/gsp-http/tests/tls_{certs,server}.rs`.

### Proxy configuration

Four routes are long-lived server-sent-event streams — `/config/subscribe`,
`/peers/subscribe`, `/proxy-peers/subscribe` and `/intent/subscribe`. The controller
sends keep-alives, but a proxy that buffers responses or enforces a short read
timeout will stall or drop them, so those must be off/long.

Caddy (certificates from Let's Encrypt automatically):

```
controller.example.com {
    reverse_proxy 127.0.0.1:9901 {
        flush_interval -1        # stream SSE immediately
    }
}
```

nginx:

```nginx
server {
    listen 443 ssl;
    http2 on;
    server_name controller.example.com;
    ssl_certificate     /etc/letsencrypt/live/controller.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/controller.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:9901;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
        proxy_buffering off;      # SSE
        proxy_read_timeout 1h;    # idle SSE streams
    }
}
```

> These two snippets are **not tested in this repository** (CI has neither proxy, and
> an end-to-end https test would need a certificate the clients trust — see the limits
> below). Check them against your proxy's version.

In Kubernetes the equivalent is an Ingress (or Gateway) with TLS in front of the
`gsp-controller` Service; make sure its response buffering and timeouts allow SSE.

### Pointing the clients at it

Every HTTP client in the fleet uses `reqwest` with rustls and builds its requests from
the base URL as given — no code forces a scheme — so these take an `https://` base URL:
`gsp --controller`, `gsp --tunnel-controller-url`, `gsp-agent --controller-url`,
`gsp-ui --controller-url` / `--aggregator-url`, `gsp-controller --parent-url`, and the
`parent_url` in a `POST /admin/adopt` body.
To check it works:

```sh
curl -fsS https://controller.example.com/healthz
gsp --check --controller https://controller.example.com --controller-token "$TOKEN"
```

**Private or self-signed CA.** Every binary (`gsp`, `gsp-agent`, `gsp-controller`,
`gsp-aggregator`, `gsp-ui`) takes `--ca-file <PATH>`: a PEM file with one or more CA
certificates to trust for **all** of that process's outbound HTTPS, in addition to the
built-in Mozilla roots (it never replaces them, so public endpoints keep working).
Non-certificate PEM sections (a private key pasted into the same file) are ignored. A
missing or unreadable file, one with no certificates, or a malformed certificate stops
the process at startup with an error naming the file. The file is read **once, at
startup**: `SIGHUP` / a config reload does not re-read it, so restart the process after
changing it (e.g. when rotating the CA).

```sh
gsp --check --controller https://controller.internal:8443 --ca-file /etc/gsp/ca.pem
```

This exact shape — `gsp --check` against a real `gsp-controller` behind a TLS
terminator whose certificate a private CA signed, failing without `--ca-file` and
passing with it — is the `gsp-fleet-tests` test `ca_file.rs`, run by `make check`.
The two proxy snippets above are still untested.

A TLS or connection failure is logged with its cause (e.g. `invalid peer certificate:
UnknownIssuer`), not just "error sending request" — that is usually the first sign a
`--ca-file` is missing or wrong.

### HA replicas over TLS

Put a TLS terminator in front of **each** replica's listener and give `--ha-peers`
base URLs instead of `host:port` (same list on every replica), plus `--ca-file` if the
terminators' certificates come from a private CA:

```sh
gsp-controller --listen 127.0.0.1:9901 --ha-node-id 1 --ca-file /etc/gsp/ca.pem \
  --ha-peers 1=https://ctl-1.internal:8443,2=https://ctl-2.internal:8443,3=https://ctl-3.internal:8443
```

Raft RPCs (`/raft/append`, `/raft/vote`, `/raft/snapshot`) and writes a follower
forwards to the leader then go over HTTPS. `id=host:port` still means plain HTTP, and
the two forms can be mixed. An entry that is not `http(s)://host[:port]` (another
scheme, a path, a query, user info) stops the replica at startup naming the entry.
Verified by `gsp-fleet-tests` `ha_tls.rs`: three replicas that reach each other only
through private-CA terminators elect a leader, forward writes and all serve the last
one with `--ca-file`, and never elect a leader without it.

**Only at bootstrap.** `--ha-peers` is read when the cluster is first initialised;
after that each member's address lives in the replicated Raft membership. Changing
`--ha-peers` on an existing cluster does **not** change the addresses replicas use, so
an existing plain-HTTP cluster cannot be moved to `https://` by editing the flag — that
needs a membership change, which is not built yet.

### Limits (read these before relying on it)

- **No system certificate store.** The clients trust the Mozilla root bundle compiled
  into the binary (`reqwest`'s `rustls-tls` / `webpki-roots`) plus `--ca-file`, **not**
  the OS store or `SSL_CERT_FILE`. For a private or internal CA — or a self-signed
  certificate — pass it with `--ca-file`. No client certificates (mTLS).
- **HA over TLS needs a fresh cluster** (native TLS on each replica, or a terminator
  per replica; see "HA replicas over TLS"). With `host:port` peers, replica traffic is plain HTTP; then keep
  the replicas on a private network — `--ha-token` is a shared secret, not encryption.
- **The UI behind a proxy** sees plain HTTP, so its session cookie is `HttpOnly;
  SameSite=Lax` but **not** `Secure`: add it, redirect HTTP to HTTPS (and consider
  HSTS) at the proxy — or use the UI's native TLS, which sets `Secure` itself.
- **A TLS proxy is a trust boundary.** It sees every token and registration in the
  clear. Run it on a host you control — or use native TLS where it exists.

## Tunnel addressing

Tunnel-internal addressing (the WireGuard-side `10.x.x.x` space between proxies and
origins) is not an open question any more: `gsp-controller --tunnel-network` allocates
the addresses, so nothing here hand-picks one. See `docs/11` "Address authority". This
document covers only the container/orchestrator-facing side of the proxy's public
listeners.
