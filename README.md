# game-server-proxy

A **game-agnostic game server reverse proxy**: a single entry point in front of
arbitrary game servers that transparently forwards TCP and UDP traffic to backend
instances — without knowing the game's protocol.

Typical goals: hide backend IPs (DDoS protection), port-/hostname-based routing to
many server instances, zero-downtime restarts (connection draining), central metrics
and access control.

## Planning documents

| File | Contents |
|------|----------|
| [docs/00-overview.md](docs/00-overview.md) | Goals, non-goals, use cases, glossary |
| [docs/01-requirements.md](docs/01-requirements.md) | Functional & non-functional requirements |
| [docs/02-architecture.md](docs/02-architecture.md) | Components, data plane / control plane, data flows |
| [docs/03-routing.md](docs/03-routing.md) | Routing strategies in detail |
| [docs/04-transport-and-client-ip.md](docs/04-transport-and-client-ip.md) | TCP/UDP handling, client-IP preservation, PROXY protocol |
| [docs/05-configuration.md](docs/05-configuration.md) | Configuration schema & examples |
| [docs/06-operations-observability.md](docs/06-operations-observability.md) | Metrics, logging, health checks, draining |
| [docs/07-security-ddos.md](docs/07-security-ddos.md) | Rate limiting, ACLs, DDoS mitigation |
| [docs/08-roadmap.md](docs/08-roadmap.md) | Phased implementation / milestones |
| [docs/09-technology-choices.md](docs/09-technology-choices.md) | Language, libraries, alternatives |

## Status

Planning phase only. No code yet.
