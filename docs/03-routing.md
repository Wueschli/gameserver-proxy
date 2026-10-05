# 03 – Routing

Goal: map an incoming connection/session to a **pool** (and optionally an affinity
key) — ideally without game-protocol knowledge, with optional plugins where needed.

> **Implementation status (phase 3, partial).** `listeners[].routes` is a
> priority-ordered `[{ match, action }]` list, first match wins, with
> `action: { pool: <name> }`. Implemented matchers: `always`, `client_cidr`
> (source IP), `dst` (destination IP the client connected to, via
> `getsockname` / the bind address, against a `cidrs` list — see the caveat
> below), `port` (destination port from the accepting socket), `first-bytes`
> (a `prefix` — `"hex:..." | "ascii:..."`, ≤ 512 B — and/or a
> `length: { min, max }` byte-count window; on TCP `length` sees only what one
> peek returned, on UDP the exact datagram length), and `sni`
> (`host` patterns: exact, `*.suffix`, `.suffix` — matched against `server_name`
> from the peeked, non-terminated TLS ClientHello; TCP listeners only). The
> `sniffer` matcher exists (`{ type: sniffer, sniffer: <name>, host: [...] }`,
> several per listener) but **no sniffers are built in** — a `sniffer:`
> route never matches until a plugin is loaded (Phase 9). The TCP path
> `MSG_PEEK`s up to 4096 B (250 ms budget) before routing, only when a route
> needs bytes; UDP inspects the first datagram it already holds. A ClientHello
> split across TCP segments is reassembled: the path re-peeks every 5 ms until
> the whole first TLS record is buffered or the 250 ms budget expires, so `sni`
> still matches a multi-segment ClientHello. (`first-bytes` `length` on TCP is
> still "what has been peeked so far" — a `prefix` fits in the first segment.)
> The `consistent_hash` balancer is implemented (`balancer: consistent_hash`,
> pool-level `hash_on: src_ip | src_ip_port`; rendezvous/HRW hash over the
> healthy backends); so is `weighted` (`balancer: weighted` + a `weights:` map).
> **`dst` — two forms:** a normally-bound TCP/UDP listener sees
> only its own bind IP (useful across addresses the host serves separately; TCP
> can bind a non-local address with `freebind: true`). A UDP listener with
> `prefix: <cidr>` runs in **prefix mode** — one wildcard socket with
> `IP_PKTINFO` / `IPV6_RECVPKTINFO` serves the whole routed prefix (scheme A
> below): the real per-datagram destination feeds `dst`, replies go out from
> that address, and datagrams outside the prefix are dropped
> (`wayhouse_datagrams_dropped_total{reason="outside_prefix"}`). The sniffer **seam**
> is `wayhouse_core::sniff::Sniffer` → `RouteHint { host, key, reject }`, fed into
> routing before matchers run — a `reject` hint drops the connection / datagram
> outright (`result="sniffer_reject"` / `reason="sniffer_reject"`); loading real
> sniffers (sandboxed, from a separate repo) is Phase 9. The **push resolver** (`POST /route-hint`) is implemented:
> a listener with `route_hint: true` checks a short-lived `src_ip → pool` table
> before its route list (`wayhouse_route_hints_applied_total{listener}` counts hits).
> The **external resolver** (`action: { resolver: <name> }`, `resolvers:`
> section) is implemented — **phase 4 slices 1–4**: HTTP **and gRPC** transports
> (`type: http | grpc`), `pool` **and `target`** results (`target` = connect
> straight to a resolver-named instance, no pool / health / cap), `on_error:
> reject | fallback_route | stale_ok`, and a TTL'd LRU result cache (`cache: {
> key, positive_ttl_sec, negative_ttl_sec, max_entries }`; key parts `src_ip` /
> `src_ip_port` / `sni` / `routing_key` / `first_bytes:a:b`). Only
> `Resolution.sticky_key` is unused (deferred — it overlaps the request-keyed
> cache and the existing affinity mechanisms).
> Still pending elsewhere: the TCP side of prefix binding beyond `freebind` and
> the `first_available` balancer. The `weighted` balancer is implemented
> (`balancer: weighted` + a `weights:` map of `"ip:port"` → share, default 1) —
> weighted round-robin over the healthy set, one atomic tick per selection.
> Regex-over-first-bytes is folded into the Phase 9 plugin layer, not a
> `first-bytes` sub-form. A listener with a bare `pool:` is normalised to one
> `always` route.

## Evaluation order

1. **Early filters** (ACL, rate limit, geo) – before routing, may reject immediately.
2. **Listener binding** – the listener may already map 1:1 to a pool (simplest case,
   no further logic).
3. **Sniffer** (if a `sniffer` route is configured) – runs once on the peeked
   first bytes. A listener may route on several sniffers (e.g. `quic`,
   `wireguard` and `a2s` on one UDP port): they are tried in order of first
   appearance in the route list and the **first that recognises the bytes wins**
   — a later sniffer is not consulted, even if none of the winner's routes
   match (routing then falls through to non-sniffer routes such as `always`) —
   and a `sniffer` route only matches the sniffer it names. A `RouteHint { reject: true }` **drops the connection / datagram
   immediately** (before the push-resolver hint, so a spoofable `src_ip` hint
   cannot override it); TCP `wayhouse_listener_connections_total{result="sniffer_reject"}`,
   UDP `wayhouse_datagrams_dropped_total{reason="sniffer_reject"}` and no reply. A
   non-`reject` hint is carried into route matching.
4. **Push-resolver hint** – if the listener has `route_hint: true` and a live
   `src_ip → pool` entry exists (from `POST /route-hint`) whose pool still
   exists, it wins and steps 5–7 are skipped.
5. **Route matching** – the listener's ordered rule list; **the first matching rule
   wins**. Each rule: `match` + `action` (`pool` or `resolver`).
6. **Resolver** (if the rule requires it) – external lookup with cache/fallback.
7. **Default route** – when nothing matches.

## Matcher types

### `always`
Always matches. For default/catch-all rules.

### `port`
The destination port (of the accepting socket) selects the pool; `ports:`
accepts a `"lo-hi"` range too. Pairs naturally with a *port-range listener*
(`bind: "host:lo-hi"`, F1.4) — one config entry spawning a real socket per
port across the range, so `30000–30099 → pool-a` needs one listener, not one
per port. See [05](05-configuration.md).

### `dst` (destination IP / prefix of the incoming packet)
The address the client sent to — **not** from the payload but from the socket.
Compared against a CIDR/prefix list (LPM trie). This is the lever for games with **no**
protocol hint at all: the subdomain is mapped by DNS to its own destination IP and the
proxy distinguishes on `dst`. See the section
["Routing without a protocol hint"](#routing-without-a-protocol-hint-raw-data-to-an-ipport).
Data-path requirement: receive on a whole prefix with **one** socket (`IP_PKTINFO` /
`IPV6_RECVPKTINFO`), see [04](04-transport-and-client-ip.md).

### `client_cidr`
Source IP in a CIDR list. Uses: internal testers to a staging pool, region roughly by
IP block, partner ranges. Also for the launcher/push resolver (section below):
short-lived `src_ip → pool` mapping.

### `sni` (TCP, TLS handshake – without termination)
- Reads the `ClientHello`, extracts `server_name`.
- Comparison exact / suffix (`*.eu.example.com`) / regex.
- TLS is **not** terminated; the byte stream (including the ClientHello) is forwarded
  unchanged. Peek only.
- Cost: one `MSG_PEEK`, a minimal parser (no OpenSSL required).

### `first_bytes` (TCP or UDP first datagram)
- `prefix`: exact byte/string prefix (`hex:` or `ascii:` notation).
- `regex`: regex over the first `N` bytes (precompiled, `N` bounded).
- `length`: datagram length within a range (coarse heuristic, e.g. query vs.
  gameplay).
- `sniffer: <name>`: a named plugin returns structured hints, e.g.:
  - `sni` (generic, see above)
  - `minecraft` → handshake hostname + protocol version
  - `a2s` / `source-query` → Valve query recognized (route to a query pool)
  - `quic` → QUIC Initial recognized (v1, v2, IETF drafts; key `quic`). The plugin also decrypts the Initial (its keys derive from the packet's own destination connection ID and a public salt, RFC 9001 §5.2) and returns the TLS SNI as the hint's `host` (v1, v2 and drafts 29 to 34). When the SNI cannot be read (an older draft, or a ClientHello split across Initial packets with the SNI in a later one) only the key is set, so a `host:` route does not match and an empty-`host:` route still does
  - `wireguard` → handshake initiation recognized (key `wireguard`)
  - `openvpn` → client hard reset recognized, UDP or TCP-framed (key `openvpn`; a weak one-byte signal: about 3 in 256 random datagrams match, so it makes the first-packet gate leaky and belongs after stronger plugins in the sniffer list)
  - `raknet` → RakNet offline handshake recognized by its magic (Minecraft Bedrock and other RakNet games; key `raknet`)
  - `teamspeak3` → TeamSpeak 3 `TS3INIT1` client init recognized (key `teamspeak3`)
- Plugin contract: **read-only**, receives up to `peek_max_bytes`, returns
  `Option<RouteHint { key?: String, pool_hint?: String, reject?: bool }>`. No access to
  later bytes, no writing.

### `external` (resolver)
- A rule action instead of a fixed pool.
- Request to a gRPC/HTTP endpoint:
  ```json
  {
    "listener": "public-udp",
    "src": "203.0.113.7:51343",
    "dst": "198.51.100.10:7777",
    "sni": "eu.example.com",
    "first_bytes_b64": "AAECaGVsbG8=",
    "routing_key": "eu.example.com"
  }
  ```
- Response:
  ```json
  { "pool": "match-eu-1", "target": null, "sticky_key": "player:42", "ttl_sec": 30 }
  ```
  either `pool` (then normal LB) **or** `target` (a fixed instance, e.g. assigned by
  the matchmaker).
- **Cache**: key configurable (`src_ip`, `sni`, `routing_key`, hash of the first
  bytes). Positive TTL from the response, negative TTL separately.
- **Timeout/error**: `on_error: fallback_route | reject | stale_ok`.
- Use: the matchmaker assigns a player token to an instance; the proxy does not need
  to understand the token, only forward it and cache the decision.

## Load balancing within a pool

| Strategy | Use |
|----------|-----|
| `round_robin` | equivalent stateless backends |
| `least_conn` | long-lived sessions of uneven duration |
| `weighted` | heterogeneous hardware / canary; `weights: { "ip:port": N }`, default 1, weighted round-robin |
| `consistent_hash` | affinity without a sticky table; hash key = client IP or routing key |
| `first_available` | active/passive, a backend fills up to its cap, then the next *(not yet implemented)* |

## Session affinity

- **Implemented**: a `consistent_hash` pool (rendezvous hash of `src_ip` or
  `src_ip_port`). Stateless, so it holds at any worker count and after any idle
  eviction; an unhealthy or draining backend is skipped and its clients move to
  the next-highest score. Adding a backend remaps about 1/N of the clients.
- **Not implemented**: a `key → backend_id (+ TTL)` sticky table keyed by the
  resolver's `sticky_key` or a sniffer `key`. The UDP table that existed was
  removed in #56: it was per worker, so with `SO_REUSEPORT` (clients spread by
  source port) it kept only ~34% of clients on 4 workers.
- For UDP, affinity is effectively mandatory (otherwise the gameplay stream
  fragments across multiple instances): point UDP listeners at a
  `consistent_hash` pool.

## Routing without a protocol hint (raw data to an IP:port)

Many games send raw, often encrypted payload from the first byte to a fixed
`IP:port` — no hostname, no SNI, nothing stable in the first packet. Then there is
nothing **in the stream** to tell `survival` from `creative`.

**Principle:** route by what you still know without the payload — the **destination
address** (`dst_ip`) and the **destination port** the client sent to. So the
"subdomain" must be mapped by DNS to a distinguishable `(IP, port)` combination. Three
schemes, used alone or combined:

### Scheme A – one IP per server (recommended, especially with IPv6)

- DNS maps the name to its **own** address:
  ```
  survival.example.net   AAAA  2001:db8:ace:1::1
  creative.example.net   AAAA  2001:db8:ace:1::2
  hardcore.example.net    A    198.51.100.7      # IPv4 only when necessary
  ```
- The proxy is given a **routed prefix** (e.g. an IPv6 `/64` or `/48`) and accepts
  traffic on **all** addresses in it with **one single** wildcard socket — no socket
  per IP:
  - **UDP**: `IP_PKTINFO` / `IPV6_RECVPKTINFO` yields the actual destination address
    per datagram; the reply is sent via `cmsg` with the **same** source address.
  - **TCP**: `IP_FREEBIND` or `net.ipv4.ip_nonlocal_bind` / bind-any on the prefix;
    the destination IP of the accepted connection comes from `getsockname()`.
- Routing table: a `dst` matcher against an LPM trie `prefix → pool`. Millions of
  destination IPs cost almost nothing (memory ~ number of rules, not number of IPs).
- IPv6 is the normal case here: address space is effectively unlimited, each instance
  can get its own public address. IPv4 only with a small, expensive IP pool — then
  prefer scheme B.

### Scheme B – one port per server

- All names point to the **same** IP; the port distinguishes them:
  ```
  survival.example.net → 198.51.100.7:30001
  creative.example.net → 198.51.100.7:30002
  ```
- Where the game/protocol supports it, an **SRV record** hands out host+port
  (`_game._udp.survival.example.net SRV 0 0 30001 edge.example.net`). Otherwise the
  launcher / server-browser config carries the port.
- Proxy: a port-range listener (`bind: "0.0.0.0:30000-30099"`, F1.4) + `port`
  matcher (`30001 → pool-survival`).
- Limits: some clients hard-code the port; restrictive client firewalls; SRV support
  is rare outside a few protocols.

### Scheme C – pre-registration via launcher/API (push resolver)

- If the game starts through a launcher or server browser, **that** calls the control
  plane API before connecting:
  `POST /route-hint { src_ip: "203.0.113.7", pool: "survival", ttl_sec: 30 }`.
- The proxy keeps a short-lived `src_ip → pool` table. The first packet/SYN from that
  IP with no other hint is resolved via it, then the pool's balancer takes over.
- Weakness: several players behind **one** NAT IP wanting different subdomains at the
  same time cannot be told apart — unless the launcher can additionally set a short
  token that does end up in the first packet (then `first-bytes`). Otherwise fall back
  to scheme A/B.
- This is the "reverse" of the [`external` resolver](#external-resolver): push instead
  of pull, useful when the proxy cannot query the client context itself.

### What fundamentally does not work

Same game, **same IP, same port**, no token in the first packet, multiple logical
servers behind it: not distinguishable. Then there is no way around scheme A (own IP)
or B (own port) — or a launcher that supplies a token.

### DNS and routing table from one source

To keep `name → (IP/prefix | port) → pool` consistent, **one** inventory file (per
server: `name`, `address` or `port`, `pool`) should generate both the DNS zone and the
`listeners[].routes` with the `dst`/`port` match. Otherwise resolution and forwarding
drift apart.

## Examples

### A) One port, many match instances, matchmaker decides
```
listener public-udp (0.0.0.0:7777/udp)
  route 1: match = always  → action = external(resolver = matchmaker)
     resolver cache key = first_bytes(0..16)   # contains a session token
     on_error = reject
```

### B) Region by SNI, TLS stays with the backend
```
listener public-tcp (0.0.0.0:443/tcp)
  route 1: sni suffix ".eu.example.com" → pool match-eu   (consistent_hash on sni)
  route 2: sni suffix ".us.example.com" → pool match-us
  route 3: always                        → pool lobby-default
```

### C) Minecraft network (plugin only reads the hostname)
```
listener public-tcp (0.0.0.0:25565/tcp)
  route 1: sniffer minecraft, host == "survival.example.net" → pool mc-survival
  route 2: sniffer minecraft, host == "creative.example.net" → pool mc-creative
  route 3: always → pool mc-lobby
```

### D) Separate query and gameplay pools by packet length
```
listener public-udp (0.0.0.0:27015/udp)
  route 1: first-bytes prefix hex:FFFFFFFF → pool source-query   # A2S
  route 2: always                          → pool gameplay
```

### E) Raw UDP with no hint at all – subdomain by destination IP (scheme A)
```
DNS:  survival.example.net  AAAA  2001:db8:ace:1::1
      creative.example.net  AAAA  2001:db8:ace:1::2
      arena.example.net     AAAA  2001:db8:ace:1::3

listener raw-udp
  bind:      "[2001:db8:ace:1::]/64 :7777/udp"   # one wildcard socket, IPV6_RECVPKTINFO
  routes:
    route 1: dst 2001:db8:ace:1::1/128 → pool survival
    route 2: dst 2001:db8:ace:1::2/128 → pool creative
    route 3: dst 2001:db8:ace:1::3/128 → pool arena
    route 4: always                     → reject   # unknown destination IP
```
The client connects to `survival.example.net:7777`, immediately sends raw packets; the
proxy reads the destination address from the datagram `cmsg` and picks the pool.
Replies go back with `2001:db8:ace:1::1` as the source.

### F) Like E, but only IPv4 available – subdomain by port (scheme B)
```
DNS/SRV:  _game._udp.survival.example.net  SRV 0 0 30001 edge.example.net
          _game._udp.creative.example.net  SRV 0 0 30002 edge.example.net

listener raw-udp-v4
  bind:  "0.0.0.0:30000-30099/udp"
  routes:
    route 1: port 30001 → pool survival
    route 2: port 30002 → pool creative
    route 3: always     → reject
```
(Pseudo-code — real YAML has no `/udp` suffix on `bind`, `protocol: udp` is
its own key, and `routes:` entries are `{ match: {...}, action: {...} }`. See
`docs/05` for the actual shape; the range-bind idea itself (F1.4) is real and
built.)
