# Documentation

> [!NOTE]
> wayhouse is a work in progress. These documents describe the design the
> code implements, but details can still change. See the notice in the
> [top-level README](../README.md).

## Design documents (00–12)

The numbered chapters are the **source of truth for the design**: when the code and a
chapter disagree, the chapter wins and the code (or the chapter) gets fixed.

**Start here:** [00 Overview](00-overview.md) for the problem and the vocabulary, then
[02 Architecture](02-architecture.md) for how the pieces fit together.

| Read this                                                         | When you want to know                                                                                 |
| ----------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| [00 Overview](00-overview.md)                                     | What problem this solves, goals, non-goals, glossary                                                  |
| [01 Requirements](01-requirements.md)                             | Functional and non-functional requirements (the NFR latency budgets live here)                        |
| [02 Architecture](02-architecture.md)                             | Data plane vs. control plane, components, data flows                                                  |
| [03 Routing](03-routing.md)                                       | Route rules, matchers, balancers, sniffers, external resolvers                                        |
| [04 Transport and client IP](04-transport-and-client-ip.md)       | TCP/UDP handling, PROXY protocol, TPROXY transparent mode                                             |
| [05 Configuration](05-configuration.md)                           | The full config schema with examples                                                                  |
| [06 Operations and observability](06-operations-observability.md) | Metrics, logs, health checks, draining, admin and fleet endpoints, HA operations                      |
| [07 Security and DDoS](07-security-ddos.md)                       | Filters, rate limits, amplification guard, sniffer sandbox                                            |
| [08 Roadmap](08-roadmap.md)                                       | Phases 0–14 with their status, and what is left                                                       |
| [09 Technology choices](09-technology-choices.md)                 | Libraries, alternatives and the ADR table of locked decisions                                         |
| [10 Distributed control plane](10-distributed-control-plane.md)   | `wayhouse-controller`, `wayhouse-aggregator`, `wayhouse-ui`, Raft HA, canary rollout, regional health |
| [11 Backend transport](11-backend-transport.md)                   | WireGuard transport and `wayhouse-agent` for origins on other networks                                |
| [12 Deployment](12-deployment.md)                                 | Container images, many-port proxies under Docker and Kubernetes                                       |

## Feature specs and plans

[`superpowers/`](superpowers/) holds the design spec and the step-by-step
implementation plan written before each larger change since the roadmap phases were
finished. They are kept as a record of why things were built the way they were; see
[superpowers/README.md](superpowers/README.md).

## Elsewhere in the repository

- [`../deploy/README.md`](../deploy/README.md): reference images, Compose demo and
  Kubernetes manifests.
- [`../crates/sniffers/README.md`](../crates/sniffers/README.md): the WASM sniffer
  sniffer ABI and the first-party sniffers.
- [`../crates/wayhouse-bench/README.md`](../crates/wayhouse-bench/README.md): the latency / load
  harness.
- [`../crates/wayhouse-config/fuzz/README.md`](../crates/wayhouse-config/fuzz/README.md): fuzz
  targets.
- [`../HANDOVER.md`](../HANDOVER.md): current state, known follow-ups and gotchas.
