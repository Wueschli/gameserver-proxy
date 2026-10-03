# Feature specs and implementation plans

Working documents from development. Every larger change after the roadmap phases
followed the same flow: a **spec** in [`specs/`](specs/) agreed the design, a **plan**
in [`plans/`](plans/) broke it into small test-first steps, and the plan was then
executed one step at a time.

They are kept as a historical record, not as reference documentation. The numbered
chapters in [`docs/`](../) are the source of truth for the current design, and a spec
can be out of date where later work changed things. File names start with the date
the document was written.

| Feature | Spec | Plan | State |
|---------|------|------|-------|
| `deploy/`: container images, Compose and Kubernetes examples | [spec](specs/2026-10-01-deploy-design.md) | [plan](plans/2026-10-01-deploy.md) | Built; Docker steps verified in CI only |
| Phase 14 tunnel end-to-end test (rootless, network namespaces) | [spec](specs/2026-10-01-tunnel-e2e-design.md) | [plan](plans/2026-10-01-tunnel-e2e.md) | Built (`make tunnel-e2e`) |
| Custom CA support for the HTTP clients (`--ca-file`) | [spec](specs/2026-10-02-custom-ca-design.md) | [plan](plans/2026-10-02-custom-ca.md) | Built |
| TLS-capable HA peer addresses and readable HTTP errors | [spec](specs/2026-10-02-ha-tls-peers-design.md) | [plan](plans/2026-10-02-ha-tls-peers.md) | Built |
| Native TLS for `gsp-controller` | [spec](specs/2026-10-02-controller-native-tls-design.md) | [plan](plans/2026-10-02-controller-native-tls.md) | Built |
| Native TLS for `gsp-aggregator`, `gsp-ui` and the `gsp` admin API | [spec](specs/2026-10-02-native-tls-other-servers-design.md) | none | Built |
| Tunnel address authority (`gsp-controller` allocates tunnel addresses) | [spec](specs/2026-10-02-tunnel-address-authority-design.md) | [plan](plans/2026-10-02-tunnel-address-authority.md) | Built |
| IPv6 tunnel networks and underlay (`--tunnel-readdress`) | [spec](specs/2026-10-03-ipv6-tunnel-design.md) | [plan](plans/2026-10-03-ipv6-tunnel.md) | Built |
| TLS handshake flood limits | [spec](specs/2026-10-03-tls-handshake-limits-design.md) | none | Built |
| CI change detection from `cargo metadata` | [spec](specs/2026-10-03-ci-change-detection-design.md) | none | Built |
