# Security policy

## Reporting a vulnerability

Please do not open a public issue for a security problem. Report it privately through a
[GitHub security advisory](https://github.com/wayhouse-proxy/wayhouse/security/advisories/new)
on this repository. Include what you found, how to reproduce it, and which version or commit is affected.

In scope: the `wayhouse` proxy and its fleet components (controller, aggregator, admin GUI, WireGuard agent), the
host-side sandbox for sniffers and plugins, and the CI and release tooling here. Vulnerabilities in the official
sniffers or plugins belong in their own repositories: [sniffers](https://github.com/wayhouse-proxy/sniffers/security/advisories/new)
and [plugins](https://github.com/wayhouse-proxy/plugins/security/advisories/new).

Known open findings from the October 2026 review are listed in
[docs/security-review-2026-10.md](docs/security-review-2026-10.md). You do not need to report those again.

## Supported versions

wayhouse is pre-1.0 and under active development. Fixes land on `main` and ship in the next release; older releases are not patched.
