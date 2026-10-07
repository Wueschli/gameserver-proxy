# 12 – Container images & deployment

How to package the five binaries for Docker/Kubernetes, and — the part that
isn't obvious going in — how a proxy that needs to listen on many, changing
ports actually works once it's namespaced by a container runtime.

## Container images

The reference build is [`deploy/Dockerfile`](../deploy/Dockerfile): one file, a
shared `rust:1-trixie` builder and six runtime targets (the five binaries plus `wayhouse-minimal`, below) on
`gcr.io/distroless/cc-debian13:nonroot` (see [`deploy/README.md`](../deploy/README.md);
`make deploy-images` builds all six; `make deploy-scan` runs an informational Trivy
scan of them and of the lockfiles they're built from — CI's `deploy` and `trivy` jobs).
`wayhouse-minimal` is `wayhouse` built with `--no-default-features` (issue #62): no WASM sniffer loader,
gRPC resolver, `dns_srv` source, Tier-2 gossip fabric or WireGuard tunnel client, so the
binary is much smaller. It takes the same arguments as `wayhouse`; a config or flag that needs a
dropped feature (`settings.sniffers`, a `grpc` resolver, a `dns_srv` source, `settings.gossip`,
a `tunnel` source, `--tunnel-iface`) is refused at startup and under `--check`, naming the
cargo feature.

The published `ghcr.io` images are private for now; how to pull them (classic PAT with `read:packages`, Kubernetes pull secret) and how to make them public later is in [`deploy/README.md`](../deploy/README.md#pulling-private-images).

None of these five binaries shell out to an
external command at runtime (WireGuard interface management in `wayhouse`/`wayhouse-agent`
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
| `wayhouse` | **47.4 MB** |
| `wayhouse-controller` | **37.9 MB** |
| `wayhouse-agent` | 34.3 MB |
| `wayhouse-ui` | 34.3 MB (includes the built frontend) |
| `wayhouse-aggregator` | 33.3 MB |

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

`wayhouse` and `wayhouse-controller` are the two heaviest either way, for real
reasons: `wayhouse` links `wasmtime` (phase 9 sniffers) and
`defguard_wireguard_rs`/`boringtun` (phase 14 backend transport);
`wayhouse-controller` links `openraft` + `sled` for the HA/Raft log store
(phase 12). `wayhouse-agent`/`wayhouse-aggregator`/`wayhouse-ui` don't carry those.

## Examples

[`deploy/compose/`](../deploy/compose/) is a runnable control-plane demo (controller,
aggregator, UI, one `wayhouse` pulling its config from the controller) with a tunnel
override; [`deploy/k8s/`](../deploy/k8s/) has plain manifests (the proxy as a
`hostNetwork` DaemonSet, plus the RBAC its `kubernetes` sources need). Both are reference only (the images themselves are published to GHCR on version tags by `.github/workflows/release.yml`). CI smoke-tests the compose demo and schema-validates the manifests (the `deploy`
job, blocking since 2026-10-02; first green run 2026-10-01). Aggregator intent fan-out (drain etc.) goes to the `admin_url`
each `wayhouse` reports, by default `http(s)://<settings.admin.listen>`; when that
address is not reachable from the aggregator (`0.0.0.0`, loopback, a container
port mapping, a TLS terminator in front), set `wayhouse --aggregator-admin-url`. The
k8s DaemonSet reports `http://<node IP>:9900`, and the aggregator presents
`--instance-token` (the admin `auth_token`). The compose demo cannot use
fan-out: its `wayhouse` admin API listens on the host's loopback only, which the
bridge-networked aggregator cannot reach.

The aggregator separates the credentials: `--ingest-token` is what the `wayhouse`
instances push with (`--aggregator-token` on `wayhouse`; it unlocks only
`POST /ingest`), `--auth-token` is for `wayhouse-ui` and operators (`/fleet/*`). Setting
`--auth-token` requires a different `--ingest-token`; the aggregator refuses to
start otherwise.
It only stores an `admin_url` whose host is the pushing connection's own source
IP, or one covered by `--instance-url-allow` (CIDR, hostname or `*.suffix`,
repeatable); set the allowlist when the aggregator sees a different source
address than the one reported (NAT, a load balancer, a TLS terminator, the
compose bridge, or a child tier relaying through `--parent-url`). A refused push
gets a `400` and a warning in the aggregator's log. A hostname entry trusts
whoever controls DNS for that name, so prefer CIDRs where the addresses are stable. See `docs/10`, "The
aggregator".

## Networking: a proxy that binds many, changing ports

`wayhouse` is designed to add and remove listeners live — `ListenerManager`
(`crates/wayhouse-core/src/listeners.rs`) reconciles the running listener set
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

Two patterns actually work, and `wayhouse` already has one foot in each:

1. **Host networking** (`docker run --network host` /
   `hostNetwork: true` on the Pod) — the container shares the host's
   network namespace directly, so every `bind()` `wayhouse` does is immediately
   live with zero orchestration involvement. This is the closest match to
   `wayhouse`'s own reload-driven listener reconciliation: add a `bind:` entry,
   send SIGHUP (or let file-watch pick it up), and the new listener exists
   — in a container exactly like bare metal. Cost: no network-namespace
   isolation for that pod/container — an acceptable, often expected,
   trade-off for the thing that sits at the actual network edge.
2. **A pre-reserved port range** — publish/expose a large contiguous range
   up front (Docker supports a range: `-p 20000-40000:20000-40000/udp`;
   Kubernetes needs either `hostNetwork` scoped to a node-level reserved
   range, or enumerating `hostPort` per container — there's no native range
   syntax in a Pod spec) and only actually `bind()` the subset needed at
   any given time via `wayhouse`'s own `bind: "host:lo-hi"` range listener. This
   is the same shape **Agones** (the standard Kubernetes game-server
   operator) uses for its `GameServer` pods — `hostNetwork: true` plus one
   port pulled from a node-level reserved pool — because Kubernetes
   Services can't do dynamic port publishing either; `docs/08`'s roadmap
   entry for the range-bind feature draws this comparison explicitly.

Host networking is the simpler default when the proxy doesn't need
network-namespace isolation from other workloads on the node (usually
true for an edge process). Reach for the reserved-range pattern when
isolation is a hard requirement, or when running many `wayhouse` instances per
node and the orchestrator should own port allocation/collision-avoidance
across them.

## `--tunnel-*` (phase 14): `CAP_NET_ADMIN` and `/dev/net/tun`

`wayhouse --tunnel-*` and `wayhouse-agent` create a real WireGuard interface
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
container shape (`crates/wayhouse-agent` + `wayhouse --tunnel-*` in Docker
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

## Authentication at startup

Every fleet API is open when no token is configured. Startup now enforces:

- **Non-loopback bind needs auth.** `wayhouse-controller`, `wayhouse-aggregator` and `wayhouse-ui`
  (`--listen`) and `wayhouse`'s admin API (`settings.admin.listen`) refuse to start on a
  non-loopback address without a token (`--auth-token`, `--ui-password` /
  `--users-file`, `settings.admin.auth_token`). If the network boundary really is your
  only control, pass `--insecure-no-auth`; it logs a warning instead.
- **HA needs `--ha-token`.** `wayhouse-controller --ha-peers` / `--ha-join` refuse to start
  without it, with no opt-out: `/raft/*` and `/admin/ha/members` would otherwise accept
  anyone. HA peers should use `https://` URLs (see below), since the token travels in
  each request.
- **Minimum secret length: 16 bytes** for `--auth-token`, `--ha-token`,
  `settings.admin.auth_token` and `settings.gossip.psk`. Generate one with
  `openssl rand -hex 32`. `--ui-password` and the outbound tokens
  (`--controller-token` etc., which must match the remote side) are not checked.

## TLS for the fleet services

Without the settings below, `wayhouse-controller`, `wayhouse-aggregator`, `wayhouse-ui` and `wayhouse`'s
admin API serve **plain HTTP**. Run as-is across a network, that exposes:

- the `--auth-token` bearer token on every request, and the `/admin/adopt` calls;
- the full config text, on `GET /config` and the SSE `GET /config/subscribe`;
- every origin's registration (WireGuard public key, public endpoint, fronted
  backend addresses) on the `/peers*` and `/proxy-peers*` routes.

Two ways to encrypt it: **native TLS** (below), or a **reverse proxy
you run that terminates TLS**, with the controller listening only on loopback or a
private network (`--listen 127.0.0.1:9901`, or a private bridge/pod network). The
aggregator, the UI and `wayhouse`'s admin API serve native TLS the same way.

### Native TLS

```sh
wayhouse-controller --listen 0.0.0.0:8443 \
  --tls-cert /etc/wayhouse/tls/fullchain.pem --tls-key /etc/wayhouse/tls/privkey.pem
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
- TLS handshakes run in their own tasks, so a client that connects and stalls cannot
  hold up others. A client must send its ClientHello within 3 s and finish the
  handshake within 10 s. At most 16 handshakes may be pending per source (an IPv4
  address or an IPv6 /64); more are closed at once. At 512 pending handshakes in total a
  new connection is still accepted and the oldest pending handshake is dropped, so a
  flood of idle connects cannot lock real clients out. A source may also open at most 20
  new connections a second on average (bursts of 64), so it cannot cycle connects under
  its pending cap. A limit being hit logs a warning (at most once a minute) and
  counts into `wayhouse_tls_handshakes_refused_total` / `wayhouse_tls_handshakes_evicted_total` on
  every binary's `/metrics` (docs/06). The limits are flags: `--tls-max-pending` (512),
  `--tls-max-pending-per-source` (16), `--tls-new-per-source-per-sec` (20; `0` turns the
  rate limit off) and `--tls-new-per-source-burst` (64); raise the per-source ones for
  a fleet that reaches the service from behind one NAT. No client certificates (mTLS).
- **`wayhouse-aggregator`** takes the same two flags with the same behaviour. Instances then
  push to `--aggregator https://…` (plus `--ca-file` for a private CA), and the same
  goes for a child tier's `--parent-url` and `wayhouse-ui --aggregator-url`.
- **`wayhouse-ui`** takes the same two flags. Serving HTTPS itself, it marks the session
  cookie `Secure` (without the flags it doesn't — a browser drops a `Secure` cookie on
  `http://` and login would loop). The live view's WebSocket works over HTTP/2 too:
  a browser on h2 opens it as an extended `CONNECT`, which the UI accepts (and the
  session cookie counts in whichever `cookie` header h2 splits it into). Plain
  HTTP is not redirected; serve only HTTPS on the port browsers use.
- **`wayhouse`'s admin API** is configured in the YAML, next to `listen`:
  `settings.admin.tls: { cert: <chain.pem>, key: <key.pem> }` (both required, plus the
  optional handshake limits `max_pending`, `max_pending_per_source`,
  `new_per_source_per_sec`, `new_per_source_burst`, same meaning as the flags above;
  startup-only like `listen`; the files renew like the flags above; `wayhouse --check`
  loads them). The admin URL `wayhouse` reports to the aggregator then becomes
  `https://<settings.admin.listen>`, so the certificate needs that address as a SAN
  (an IP SAN for an IP `listen`), and the aggregator takes `--ca-file` for a private
  CA. As before, the reported URL is the literal `listen` address — a wildcard bind
  (`0.0.0.0`) is not reachable as an admin URL, TLS or not. With `--controller` one
  YAML reaches every instance, so put each host's own certificate at the same path.
  Health probes against the admin port must then use HTTPS (Kubernetes:
  `httpGet.scheme: HTTPS`).

Verified by `wayhouse-fleet-tests`: `controller_native_tls.rs` (`wayhouse --check` fails on
`UnknownIssuer` without `--ca-file` and passes with it; `/config/subscribe` streams over
TLS; `--tls-cert` alone is refused), `aggregator_native_tls.rs` (a `wayhouse` pushes to an
`https://` aggregator, read back over HTTPS), `ui_native_tls.rs` (login over HTTPS
sets a `Secure` cookie that unlocks `/ui/session`), `admin_native_tls.rs` (an
aggregator fans an intent verb out to an `https://` admin API; `wayhouse --check` names a
bad `settings.admin.tls.cert`) and `ha_tls.rs`
`three_replicas_replicate_over_native_tls`; certificate loading, renewal and the
listener are unit-tested in `crates/wayhouse-http/tests/tls_{certs,server}.rs`.

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
`wayhouse-controller` Service; make sure its response buffering and timeouts allow SSE.

### Pointing the clients at it

Every HTTP client in the fleet uses `reqwest` with rustls and builds its requests from
the base URL as given — no code forces a scheme — so these take an `https://` base URL:
`wayhouse --controller`, `wayhouse --tunnel-controller-url`, `wayhouse-agent --controller-url`,
`wayhouse-ui --controller-url` / `--aggregator-url`, `wayhouse-controller --parent-url`, and the
`parent_url` in a `POST /admin/adopt` body.
To check it works:

```sh
curl -fsS https://controller.example.com/healthz
wayhouse --check --controller https://controller.example.com --controller-token "$TOKEN"
```

**Private or self-signed CA.** Every binary (`wayhouse`, `wayhouse-agent`, `wayhouse-controller`,
`wayhouse-aggregator`, `wayhouse-ui`) takes `--ca-file <PATH>`: a PEM file with one or more CA
certificates to trust for **all** of that process's outbound HTTPS, in addition to the
built-in Mozilla roots (it never replaces them, so public endpoints keep working).
Non-certificate PEM sections (a private key pasted into the same file) are ignored. A
missing or unreadable file, one with no certificates, or a malformed certificate stops
the process at startup with an error naming the file. The file is read **once, at
startup**: `SIGHUP` / a config reload does not re-read it, so restart the process after
changing it (e.g. when rotating the CA).

```sh
wayhouse --check --controller https://controller.internal:8443 --ca-file /etc/wayhouse/ca.pem
```

This exact shape — `wayhouse --check` against a real `wayhouse-controller` behind a TLS
terminator whose certificate a private CA signed, failing without `--ca-file` and
passing with it — is the `wayhouse-fleet-tests` test `ca_file.rs`, run by `make check`.
The two proxy snippets above are still untested.

A TLS or connection failure is logged with its cause (e.g. `invalid peer certificate:
UnknownIssuer`), not just "error sending request" — that is usually the first sign a
`--ca-file` is missing or wrong.

### HA replicas over TLS

Put a TLS terminator in front of **each** replica's listener and give `--ha-peers`
base URLs instead of `host:port` (same list on every replica), plus `--ca-file` if the
terminators' certificates come from a private CA:

```sh
wayhouse-controller --listen 127.0.0.1:9901 --ha-node-id 1 --ca-file /etc/wayhouse/ca.pem \
  --ha-peers 1=https://ctl-1.internal:8443,2=https://ctl-2.internal:8443,3=https://ctl-3.internal:8443
```

Raft RPCs (`/raft/append`, `/raft/vote`, `/raft/snapshot`) and writes a follower
forwards to the leader then go over HTTPS. `id=host:port` still means plain HTTP, and
the two forms can be mixed. An entry that is not `http(s)://host[:port]` (another
scheme, a path, a query, user info) stops the replica at startup naming the entry.
Verified by `wayhouse-fleet-tests` `ha_tls.rs`: three replicas that reach each other only
through private-CA terminators elect a leader, forward writes and all serve the last
one with `--ca-file`, and never elect a leader without it.

**Only at bootstrap.** `--ha-peers` is read when the cluster is first initialised;
after that each member's address lives in the replicated Raft membership. Changing
`--ha-peers` on an existing cluster does **not** change the addresses replicas use. To
move a running cluster to `https://` peers, change each member's address with a `PUT`
(see "HA membership" below).

### HA membership

Members are changed on a running cluster with the admin routes, gated by
`--auth-token` (pass it as a bearer token) and answered by any node, which forwards a
change to the leader. `X-Actor` is logged.

```sh
# A new or replacement node always starts empty with --ha-join (never --ha-peers: a
# node with --ha-peers calls initialize and could split the cluster).
wayhouse-controller --listen 0.0.0.0:7070 --ha-node-id 4 --ha-token "$HA_TOKEN" \
  --tunnel-network 10.60.0.0/24 --ha-join

curl -H "Authorization: Bearer $TOKEN" http://ctl-1:7070/admin/ha/members            # voters, learners, leader
curl -H "Authorization: Bearer $TOKEN" -d '{"id":4,"addr":"ctl-4:7070"}' \
  http://ctl-1:7070/admin/ha/members                                                # add (waits for catch-up)
curl -H "Authorization: Bearer $TOKEN" -X DELETE http://ctl-1:7070/admin/ha/members/2  # remove
curl -H "Authorization: Bearer $TOKEN" -X PUT -d '{"addr":"https://ctl-3.internal:8443"}' \
  http://ctl-1:7070/admin/ha/members/3                                              # change an address
```

An add answers once the new node has caught up, by log or, after the log was purged,
by snapshot; a large catch-up can take minutes (a forwarded add waits up to five).
Before an add or a `PUT` the leader asks the node at the given address who it is
(`/raft/whoami`, behind `--ha-token`) and refuses (`422`) unless it answers with that
node id, and an add also unless the node's log is empty (or it is already a learner of
this cluster): this keeps a node id from being pointed at another node's address, which
`openraft` warns can produce two leaders. Other answers: `409` the node is already a
voter, `404` unknown id, `422` removing the last voter, `503` the node is unreachable or
there is no leader. Removing the current leader is allowed. A joined node restarts with
`--ha-join` again. `--ha-join` and `--ha-peers` together, and `--tunnel-readdress` with
either, are refused at startup. Verified by `wayhouse-fleet-tests` `ha_tunnel_addresses.rs`
(join, remove the leader, move a node to a new port) and `ha_snapshots.rs` (catch-up by
snapshot after a purge).

### Upgrading a single controller to HA

A controller that already holds registrations (origins, proxies, addresses) from before
HA can become the first node of an HA cluster. Restart it with `--ha-node-id` and the
`--ha-peers` of the new cluster next to the other, empty nodes. Every node sets aside
pre-HA data it finds as `peers.pre-ha/`, `proxy-peers.pre-ha/` and
`tunnel-addresses.pre-ha/` under `--data-dir` (it never deletes them), and the cluster's
leader imports exactly one node's copy, whichever node wins the first election.

- The leader asks every voter what it holds (`/raft/whoami`) before initializing, so
  registry writes answer `503` until every voter has answered; a voter that is down
  delays the upgrade. `--ha-import-source <node-id>` (the same value on every node)
  waits only for that node; `none` imports nothing.
- When **more than one** node has pre-HA data (for example a pin-only `--ha-peers`
  deployment where each node kept its own registries) nothing is initialized: the leader
  logs an `ERROR` naming the nodes and their counts, and registry writes stay `503`
  until you restart the nodes with `--ha-import-source <node-id>`. The other nodes'
  set-aside data stays untouched (one `WARN` each).
- Imported registrations are re-written as new revisions that continue after the source's
  last revision number, so every edge's `since` cursor from the single-node days stays
  valid. Tombstones the old node wrote are carried over too (the entry lists the names
  whose newest log entry is a removal), so a subscribed edge that had not received one
  still drops the peer, and a registry with no live registration keeps its log head.
  `ImportContent` is a Raft log entry: a node on an older build applies an `Import`
  without those tombstones, so upgrade every node of the cluster before the first
  initialization.
- Clients' pinned addresses that used to collide across nodes now get `409`: uniqueness
  is cluster-wide, where it was per node before.
- Snapshots persisted by a build older than this feature (before 2026-10-03) are not
  supported: a follower refuses to install one (`ERROR` naming the snapshot format) rather
  than read it as empty registries and an empty book. Start such a node from empty
  storage, or let the cluster re-initialize as above.
- The registries compact their logs: each keeps one entry per name (its registration, or
  the tombstone of a removed one), so a registry log and the Raft snapshot stay bounded by
  the number of distinct names ever registered, not by how often they re-register.
  Revision numbers have gaps afterwards; a subscriber's `since` cursor is unaffected.

### Limits (read these before relying on it)

- **No system certificate store.** The clients trust the Mozilla root bundle compiled
  into the binary (`reqwest`'s `rustls-tls` / `webpki-roots`) plus `--ca-file`, **not**
  the OS store or `SSL_CERT_FILE`. For a private or internal CA — or a self-signed
  certificate — pass it with `--ca-file`. No client certificates (mTLS).
- **HA over TLS** (native TLS on each replica, or a terminator per replica; see "HA
  replicas over TLS"): bootstrap with `https://` peers, or move a running cluster with
  one `PUT /admin/ha/members/{id}` per member (see "HA membership"). With `host:port`
  peers, replica traffic is plain HTTP; then keep
  the replicas on a private network — `--ha-token` is a shared secret, not encryption.
  The controller logs a warning for each non-loopback `http://` peer.
- **Upgrading a gossip mesh** (the datagram format gained a MAC'd sender timestamp,
  security review O4): upgrade all instances in a failure domain together. While old
  and new instances are mixed they cannot exchange membership or health, so the
  domain quorum derived from gossip is unreliable until the rollout finishes (local
  health checks keep working). Old-to-new datagrams are counted in
  `wayhouse_gossip_stale_rejected_total`. Instance clocks must also agree within 30 s.
- **The UI behind a proxy** sees plain HTTP, so its session cookie is `HttpOnly;
  SameSite=Lax` but **not** `Secure`: add it, redirect HTTP to HTTPS (and consider
  HSTS) at the proxy — or use the UI's native TLS, which sets `Secure` itself.
- **A TLS proxy is a trust boundary.** It sees every token and registration in the
  clear. Run it on a host you control — or use native TLS where it exists.

## Tunnel addressing

Tunnel-internal addressing (the WireGuard-side space between proxies and origins) is
not an open question any more: `wayhouse-controller --tunnel-network` allocates the
addresses, so nothing here hand-picks one. See `docs/11` "Address authority".

**Choosing the network.** Use an IPv6 unique local address (ULA) network by default:
generate a random `/48` once and take a `/64` from it, so it cannot clash with any
origin's own LAN or another deployment's tunnel:

```
printf 'fd%02x:%02x%02x:%02x%02x::/48\n' $(od -An -N5 -tu1 /dev/urandom)
# e.g. fd49:89c1:4b5e::/48 -> --tunnel-network fd49:89c1:4b5e:60::/64
```

Choose an IPv4 network (`/16` to `/30`, e.g. `10.60.0.0/16`) instead when a game server
binds `0.0.0.0` only: it cannot be reached on an IPv6 tunnel address. The family is
fixed per controller; changing it later needs `--tunnel-readdress` (docs/11).

**Containers.** The tunnel interface needs IPv6 enabled in its network namespace.
Docker starts containers on a network without IPv6 with
`net.ipv6.conf.all.disable_ipv6=1`; for a container with its own network namespace set
`sysctls: { net.ipv6.conf.all.disable_ipv6: "0" }` (Compose) or the equivalent
`securityContext.sysctls` entry (Kubernetes). With host networking, as in
`deploy/compose/compose.tunnel.yml` and the DaemonSet's `hostNetwork`, the host's own
setting applies and such a sysctl is refused, so the host must not have IPv6 disabled.

This document otherwise covers only the container/orchestrator-facing side of the
proxy's public listeners.
