# 04 – Transport & client-IP preservation

## TCP

- **Accept**: `accept4()` with `SOCK_NONBLOCK`; `SO_REUSEPORT` sharding per worker.
- **Destination IP of the connection**: for `dst`-based subdomain routing without a
  socket per IP, the listener is bound with `IP_FREEBIND` / `ip_nonlocal_bind` to a
  routed prefix; the concrete destination IP of the accepted connection comes from
  `getsockname()`. (In transparent mode `getsockname()` already returns the real IP
  the client addressed.)
- **Peek for routing**: `recv(MSG_PEEK)` up to `peek_max_bytes` / `peek_timeout_ms`.
  If the client sends nothing first (server-speaks-first), the default route applies
  immediately.
- **Upstream connect**: non-blocking, `connect_timeout`. `TCP_NODELAY` set; the
  client's options are not "inherited" but set from config.
- **Data pump**: Linux `splice()` (socket→pipe→socket, zero-copy). Fallback: two
  directional buffers (32–64 KB each) with `readv/writev`. `SO_RCVBUF`/`SO_SNDBUF`
  configurable.
- **Half-close**: propagate `shutdown(SHUT_WR)` in one direction, keep the other
  direction running until it too closes.
- **Timeouts**: connect, idle (no byte in either direction), optional max-lifetime.
- **Keepalive**: optionally enable `TCP_KEEPALIVE` toward the backend to detect dead
  sessions in NAT-free internal networks.

## UDP

UDP has no connection — the proxy builds the "session" concept itself.

- **Session key**: `(src_ip, src_port, dst_ip, dst_port)` (the 4-tuple).
- **Receive**: `recvmmsg()` in batches on `SO_REUSEPORT` sockets, one loop per worker.
- **Receiving on a whole prefix** (for `dst` routing for games with no protocol hint,
  see [03](03-routing.md)): the listener does **not** bind a socket per destination IP
  but one wildcard socket and enables `IP_PKTINFO` / `IPV6_RECVPKTINFO`. Per datagram
  the `cmsg` yields the **actual destination address** (`ipi_addr` / `ipi6_addr`);
  then the `dst` match runs against the LPM trie. Replies must carry the same source
  address — the destination IP on send is set via `cmsg` (`IP_PKTINFO`) or through the
  session socket bound to the client IP. Requirement: the prefix is routed to the
  proxy host (no per-address NDP/ARP needed) and `net.ipv6.ip_nonlocal_bind` or
  `IP_FREEBIND` permits the bind.
- **One upstream socket per session**, bound to the backend via `connect(2)`:
  - Replies arrive directly on the right socket without a table lookup.
  - The kernel filters foreign senders.
  - `sendmmsg()` toward the backend in batches.
- **Session table**: `HashMap<4-tuple, SessionRef>` thread-local per worker (no
  locks). Slab allocator for `Session`.
- **Idle timeout**: timing wheel; any activity pushes the entry forward. Default
  30–120 s, configurable per route (turn-based games need more).
- **Caps**: max sessions global / per worker / per backend; new-session rate per
  source IP.
- **ICMP**: treat "port unreachable" from the backend as a passive health signal.
- **Fragmentation / MTU**: no reassembly; oversized replies use normal IP
  fragmentation. `IP_MTU_DISCOVER` toward the backend as needed.

### QUIC/DTLS
Treated as opaque UDP. Migration (client port change) breaks the session without
QUIC-CID awareness → an optional `quic` sniffer that reads the connection ID and uses
it as the session key instead of the 4-tuple. Later stage only.

## Passing the client IP to the backend

By default the backend only sees the proxy IP. Options, selectable per pool:

### 1. PROXY protocol (recommended when the backend supports it)
- **TCP**: v2 binary header (or v1 text) **before** the first payload to the backend.
  Carries the real source/destination address. The backend (or its framework) must
  parse it.
- **UDP**: the v2 header is prepended to the **first** datagram of the session;
  subsequent datagrams have no header. Requires backend support (e.g. a library that
  interprets the first payload as a PROXY header). Option `proxy_protocol: v2-udp`.
- Security: backends must accept PROXY headers **only** from the proxy IP (otherwise
  clients can spoof IPs).

### 2. Transparent mode (Linux TPROXY)
- Proxy socket with `IP_TRANSPARENT`; the upstream connection uses `IP_TRANSPARENT` +
  binds the **client IP** as its source. The backend sees the real client IP with no
  protocol change.
- Prerequisite: routing set up so the backend's return traffic goes back through the
  proxy (policy routing / `ip rule` + `iptables -t mangle` or nftables, or the
  backend's default gateway = the proxy). More work in the network design, but 100%
  transparent.
- Required capability: `CAP_NET_ADMIN` (or `CAP_NET_RAW`), no full root.

#### Config

Set `transparent: true` on a **TCP** listener (Linux only). gsp then:
- binds the listen socket with `IP_TRANSPARENT` (so it accepts connections a
  TPROXY rule redirected to a non-local address; `getsockname()` on the accepted
  socket still returns the original destination, which feeds `dst` / `port`
  routing exactly as a normal bind does);
- for every upstream connection — pool or resolver `target` — opens the backend
  socket with `IP_TRANSPARENT`, `bind()`s the real client `ip:port` as its
  source, then connects. If the client and backend address families differ the
  bind is skipped and a normal connect is used (logged).

UDP transparent mode is not implemented yet (`transparent` is rejected on a UDP
listener). `IPV6_TRANSPARENT` for an IPv6 *listen* address also still needs the
socket2 bump; an IPv6 client bound as the upstream source works today.

#### Network setup (example)

Redirect inbound game traffic to the proxy's port `7777` and mark it, then route
marked traffic locally:

```
# nftables: TPROXY inbound game ports to the local proxy
table inet tproxy {
  chain prerouting {
    type filter hook prerouting priority mangle; policy accept;
    ip daddr 198.51.100.0/24 tcp dport 7777 tproxy to :7777 meta mark set 1
  }
}

# policy routing: locally deliver anything with mark 1
ip rule add fwmark 1 lookup 100
ip route add local 0.0.0.0/0 dev lo table 100
```

The backend's **return** traffic must come back through the proxy host — either
make the proxy the backend's default gateway, or add an `ip rule` on the backend
network that sends the client prefixes back via the proxy. Without that the
client gets replies straight from the backend IP and the connection stalls.

### 3. No preservation
- The backend sees the proxy IP. Sufficient when anti-cheat/logic does not need the
  client IP or gets it elsewhere (in the game login token). Documented default
  behavior.

## Socket tuning (starting values, override via config)

| Parameter | TCP | UDP |
|-----------|-----|-----|
| `SO_REUSEPORT` | on (sharding) | on (sharding) |
| `SO_RCVBUF` / `SO_SNDBUF` | 256 KB | 4–8 MB (avoid loss on bursts) |
| `TCP_NODELAY` | on | – |
| `SO_BUSY_POLL` | optional | optional (latency ↓, CPU ↑) |
| `recvmmsg`/`sendmmsg` batch | – | 32–64 |
| pipe size for `splice` | 256 KB | – |

## OS limits

- Raise the FD limit (`RLIMIT_NOFILE`) (≥ 2 × expected connections).
- Large ephemeral port range toward the backend
  (`net.ipv4.ip_local_port_range`) — only matters in non-transparent mode; in
  transparent mode the socket binds the client IP.
- `net.core.somaxconn`, `net.core.netdev_max_backlog`, `nf_conntrack` (or disable
  conntrack for the proxy path via `NOTRACK`).
