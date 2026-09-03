# 00 – Overview

## Problem

Exposing game servers directly to the internet creates recurring problems:

- The real server IP is public and therefore directly exposed to DDoS.
- Many server instances (matches, lobbies, regions) require many ports/IPs and some
  external component that steers clients to the right instance.
- Restarts/deployments disconnect active players.
- Operations (metrics, connection logs, block lists) have to be rebuilt per game.

Existing solutions like BungeeCord/Velocity (Minecraft) or game-specific master servers
each solve this for **one** game only, and often for **one** transport protocol only.

## Goal

A reverse proxy for game servers that works **protocol-agnostically at layer 4**
(TCP + UDP) and optionally derives routing decisions from the first handshake packet
via **pluggable L7 plugins**. The core knows no game; game knowledge lives exclusively
in optional plugins or in external routing logic.

## Use cases

1. **IP protection / edge termination** – clients connect only to the proxy; backends
   sit in a private network. The proxy is the single DDoS-exposed point.
2. **Fan-out to many instances** – one public endpoint `play.example.com:7777`
   distributes to hundreds of match containers.
3. **Region/hostname routing** – `eu.example.com` and `us.example.com` on the same
   port, selected by SNI (TLS) or by the first packet.
4. **Matchmaker integration** – on connect, the proxy asks an external service
   (gRPC/HTTP): "Where does this player / this token belong?"
5. **Zero-downtime deployments** – a new backend version is registered, the old one is
   set to "draining"; existing sessions drain, new ones go to the new version.
6. **Blue/green & canary** for game server builds.
7. **Central operational view** – uniform metrics (connections, throughput, added RTT,
   errors) and connection logs across all games.
8. **Access control** – IP allow/deny lists, geo filters, rate limiting, per-backend
   connection caps.
9. **Fleet management** *(v2)* – one control plane + web UI to view and configure
   many proxy instances across regions from a single authoritative place, with
   revision history and persisted operator intent (see
   [10-distributed-control-plane.md](10-distributed-control-plane.md)).

## Non-goals

- **No game logic**, no matchmaking, no anti-cheat, no lobby system.
- **No parsing/rewriting of game packets** in the core (only optional plugins may read
  the first N bytes — never modify the running stream).
- **Not a NAT replacement** for arbitrary client-to-client traffic (no TURN/relay mesh).
- **Does not force transport encryption** where the game does not use it (optional
  TLS/DTLS wrapping is an add-on feature, not a requirement).
- **Not a replacement for an L3 scrubbing provider** against volumetric attacks — the
  proxy complements it, it does not replace it.
- **No cross-instance session state.** Instances are independent on the data path;
  HA is an anycast / L4-LB concern. The v2 control plane
  ([10](10-distributed-control-plane.md)) shares *config* and *health*, never
  sessions — a client rehashed to another instance reconnects.

## Guiding principles

- **Latency first.** Every extra hop costs RTT. The data path must be
  allocation-light, copy-light, and free of unnecessary serialization.
- **Agnostic by default, plugin as the exception.** Pure L4 forwarding must work
  without any plugin.
- **Separate control plane from data plane.** Config/backend changes must not block the
  data path.
- **Operations is a feature.** Health checks, draining, metrics, reload belong in the
  first version, not in "later".

## Glossary

| Term | Meaning |
|------|---------|
| **Listener** | Public socket (IP:port + protocol) where clients arrive. |
| **Route** | Rule that maps incoming connections to a backend pool. |
| **Backend / target** | A concrete game server instance (IP:port). |
| **Pool / upstream** | A set of equivalent backends with a selection strategy. |
| **Session** | For UDP: a logical connection, identified by the 4-tuple. |
| **Sniffer / L7 plugin** | Optional parser that extracts a routing hint from the first bytes (e.g. SNI, Minecraft handshake hostname). |
| **Draining** | Backend accepts no new sessions; existing ones drain. |
| **PROXY protocol** | Header (v1/v2) prepended to the connection carrying the real client address to the backend. |
| **TPROXY** | Linux kernel feature for transparent proxying that preserves the source IP. |
