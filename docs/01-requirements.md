# 01 – Requirements

## Functional requirements

### F1 – Listeners
- F1.1 Multiple listeners at the same time (different IP:port/protocol).
- F1.2 Transport: TCP and UDP; QUIC/DTLS as "opaque UDP" with no special handling.
- F1.3 Dual stack (IPv4 + IPv6).
- F1.4 Optional port-range listener (one listener for `30000–30999`) for games with
  dynamic ports and for subdomain-per-port routing (scheme B).
- F1.5 Optional prefix listener: bind to a routed IP prefix with **one** socket,
  destination address per packet/connection via `IP_PKTINFO`/`getsockname`. Basis for
  `dst` routing for games without a protocol hint (scheme A).

### F2 – Routing
- F2.1 Static listener → pool mapping.
- F2.2 Routing by SNI (on a TLS handshake) without terminating TLS.
- F2.3 Routing by first-packet match (byte prefix, regex over the first N bytes).
- F2.4 Routing via an external resolver (gRPC/HTTP callback) with cache & timeout
  fallback.
- F2.5 Routing by client IP / geo (CIDR lists).
- F2.6 Routing by **destination IP/prefix** (`dst`) and **destination port** — for
  games that send raw data to a fixed `IP:port` with no hostname/SNI/token. The
  subdomain is mapped by DNS to its own destination IP (scheme A) or its own port
  (scheme B).
- F2.7 Push resolver: short-lived `src_ip → pool` mapping set in advance by a
  launcher/API (scheme C).
- F2.8 Fallback/default route when no rule matches.
- F2.9 Deterministic rule priority (first matching rule wins).

### F3 – Upstream / pool
- F3.1 Selection strategies: round-robin, least-connections, consistent hashing
  (on client IP or routing key), weighted, "first available".
- F3.2 Session affinity: same 4-tuple / same routing key → same backend, as long as
  that backend is healthy.
- F3.3 Health checks: active (TCP connect, UDP probe, HTTP GET on a sidecar port) and
  passive (error rate/timeouts observed in the data path).
- F3.4 Backend states: `healthy`, `unhealthy`, `draining`, `disabled`.
- F3.5 Max sessions / max new sessions per second per backend.

### F4 – Session management
- F4.1 UDP session table keyed by 4-tuple with a configurable idle timeout.
- F4.2 TCP: normal connection lifetime, half-close propagation.
- F4.3 Graceful draining: redirect new sessions, end old ones after a grace period.
- F4.4 Caps: global and per-listener connection limits.

### F5 – Client-IP preservation (details in [04](04-transport-and-client-ip.md))
- F5.1 PROXY protocol v1/v2 for TCP, optional per pool.
- F5.2 PROXY protocol v2 for UDP (header in the first datagram) as an option.
- F5.3 Transparent mode (TPROXY, `IP_TRANSPARENT`) on Linux.
- F5.4 Without any of that: only the proxy IP is visible (documented behavior).

### F6 – Configuration & control plane
- F6.1 Declarative file (YAML) as the source of truth.
- F6.2 Hot reload without dropping connections (SIGHUP / file watch / API).
- F6.3 Admin API (HTTP/gRPC) for: register/deregister backend, set state, read config
  snapshot, trigger draining.
- F6.4 Optional dynamic backend discovery adapter (DNS SRV, Consul, Kubernetes
  EndpointSlices) behind a stable internal interface.
- F6.5 Validation on load; an invalid config is rejected and the old one stays active.

### F7 – Observability (details in [06](06-operations-observability.md))
- F7.1 Prometheus metrics.
- F7.2 Structured connection logs (JSON), sampleable.
- F7.3 Optional OpenTelemetry tracing for the connection-setup phase.
- F7.4 Health/readiness endpoints for the proxy itself.

### F8 – Security (details in [07](07-security-ddos.md))
- F8.1 IP allow/deny lists (CIDR), per listener.
- F8.2 Rate limiting: new connections/datagrams per source IP and per subnet.
- F8.3 SYN-flood protection (SYN cookies via the kernel), UDP amplification protection
  (respond-only-after-first-valid-packet heuristic, optional plugin).
- F8.4 Resource caps against memory/FD exhaustion.

## Non-functional requirements

| # | Requirement | Target (first version) |
|---|-------------|------------------------|
| N1 | Added latency (p50) from the proxy within the same DC | < 0.5 ms |
| N2 | Added latency (p99) | < 2 ms |
| N3 | Throughput per instance (commodity 16-core) | ≥ 20 Gbit/s or ≥ 2 Mpps forwarded |
| N4 | Concurrent TCP connections per instance | ≥ 500,000 |
| N5 | Concurrent UDP sessions per instance | ≥ 1,000,000 |
| N6 | Config reload without packet loss for existing sessions | yes |
| N7 | Startup to "ready" | < 2 s |
| N8 | Memory per idle session | < 1 KB (UDP), < 4 KB (TCP incl. buffers) |
| N9 | Availability in an HA cluster | loss of one instance without a global drop |

Also: horizontally scalable (stateless w.r.t. persistent storage, session state local
only), Linux as the primary platform, container-friendly, runs without root after
setup (capabilities instead of root).

## Assumptions

- Backends are reachable over a trusted internal network. **Superseded by
  phase 14** (backend transport, built): see
  [11-backend-transport.md](11-backend-transport.md) for how a proxy reaches a
  backend that isn't on its local network (a WireGuard tunnel per origin). A
  same-network deployment needs none of that and this assumption still holds
  for it.
- The client reaches the proxy via DNS; the proxy does not need to cryptographically
  verify client identity (the game does that if needed).
- Volumetric L3/4 attacks are absorbed upstream (scrubbing/anycast).

## Open questions

- Must the proxy serve **multiple backends at once** per client (e.g. TCP control +
  UDP gameplay on separate instances)? → affects the session model.
- Do we need **cross-instance session sharing** (failover of a live UDP session to a
  different proxy instance)? First version: no.
- Is **QUIC-aware routing** (connection ID) required, or is opaque UDP enough?
