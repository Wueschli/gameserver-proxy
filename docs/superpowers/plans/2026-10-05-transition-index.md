# Transition to v0.1.0 and the Sniffer Registry: Plan Index

> **Terminology update (2026-10-07).** Leandro split the old "plugin" concept in two. **Sniffers** are the WASM protocol/hostname sniffer modules and live in [`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers). **Plugins** are integrations with other systems (e.g. the Pelican panel, #213) and live in [`wayhouse-proxy/plugins`](https://github.com/wayhouse-proxy/plugins); their design is still open. Wherever this document says "plugin" or "plugins repo" for a WASM sniffer module, read **sniffer** / **sniffers repo**. Code identifiers, crates and paths were renamed to "sniffer" in the same PR as this banner; older text below may still show the old names.


Execution order, dependencies and open items for the plans written on 2026-10-05 from `/mnt/project-files/wayhouse/transition-plan.md` (decisions recorded there). Every plan is written for a thread that has not seen this repository; implementation goes to separate threads, one per row below, in this order. Versions stay 0.x throughout.

## Plans and specs

| # | Plan | Issues | Phase | Depends on |
|---|---|---|---|---|
| 1 | [Release branching doc](2026-10-05-release-branching-doc.md) | #190 | 0 | none |
| 2 | [Docs overhaul](2026-10-05-docs-overhaul.md) | #187 | 0 | #1 (links to `RELEASING.md`); same thread as #1 |
| 3 | [Sniffer upload validation](2026-10-05-sniffer-upload-validation.md) | #171 | 0 | none |
| 4 | [Sniffers repo push check](2026-10-05-plugins-repo-push-check.md) | #183 prerequisite | 0 | none (tiny; any thread with the repo in scope) |
| 5 | [Versioning, changelog, release-please](2026-10-05-versioning-and-changelog.md) | #188 | 1 | #1 (shares `RELEASING.md`); #2 helps (link checker, CONTRIBUTING) |
| 6 | [Plugin ABI version](2026-10-05-plugin-abi-version.md) | #183 A | 1 | #3 (extends its validator) |
| 7 | [Protocol and config versions](2026-10-05-protocol-and-config-versions.md) | #185 A | 1 | none; touches many crates, merge early and alone |
| 8 | [rc.1, private packages, v0.1.0](2026-10-05-rc1-and-private-packages.md) | #177, #189 | 2 | #1 to #7 merged, CI healthy |
| 9 | [Registry crate and index](2026-10-05-registry-crate-and-index.md) | #183 B | 3 | #6, v0.1.0 |
| 10 | [Sniffer install and trust (UI)](2026-10-05-plugin-install-and-trust.md) | #183 C | 3 | #3, #9 |
| 11 | [Sniffers repo bootstrap and move](2026-10-05-plugins-repo-bootstrap-and-move.md) | #183 | 3 | #4, #6, #9, v0.1.0 |
| 12 | [Sniffer updates and rollback](2026-10-05-plugin-updates.md) | #184 | 3 | #3, #10 |
| 13 | [Component upgrades](2026-10-05-component-upgrades.md) | #185 B | 4 | #7, v0.1.0, #186 for the proxy runbook |

Specs written because none existed: [sniffer registry, install and updates](../specs/2026-10-05-plugin-registry-design.md) (#183, #184) and [component versioning and rolling upgrades](../specs/2026-10-05-component-versioning-design.md) (#185).

## Proposed order

1. **Wave 0, in parallel:** thread A = #1 then #2 (docs); thread B = #3; thread C = #4 (small, can ride along with any thread that has the sniffers repo in scope).
2. **Wave 1:** thread D = #5 (after #1 merges); thread E = #6 (after #3 merges); thread F = #7 (any time; merge it before other cross-crate work, one PR at a time as usual).
3. **Wave 2:** thread G = #8 once everything above is on main, the runners are healthy, and CI is green on the exact commit to tag. Includes the maintainer-assisted steps (tag push, package visibility, PAT).
4. **Wave 3 (after v0.1.0):** #9, then #11 Part A and #10 in parallel, then #11 Part B, then #12.
5. **Wave 4:** #186 (draining UDP worker drops new sessions), then #13.

About 12 implementation threads in total; per the project budget, run at most about 8 code threads per session, so Waves 0 and 1 are one session, Wave 2 plus #9 and #10 the next, the rest after that.

## Corrections to the transition plan found while planning

1. **rc.1 needs its own version commit.** `check_release.py` requires the tag to equal `v<workspace version>`, so `v0.1.0-rc.1` needs `version = "0.1.0-rc.1"` in `Cargo.toml` and `Cargo.lock`; the transition plan's "bump to 0.1.0 before rc.1" cannot produce that tag. Plan #8 does it as a manual commit; release-please then produces `0.1.0` with `release-as` for its first PR only.
2. **release-please PRs do not trigger CI** when created with `GITHUB_TOKEN`, and `ci-ok` is required. Plan #5 dispatches `ci.yml` on the release branch after the action runs; a personal access token secret is the fallback and would be a maintainer action.
3. **Inter-component protocols are HTTP/JSON, SSE and UDP gossip, not gRPC.** The only `.proto` is the external resolver contract (already versioned `v1`). Plan #7 uses a header, a gossip version byte, `schema_version` and a store marker instead of gRPC handshakes.
4. **The registry client lives in the UI backend, not on each proxy.** Proxies keep only the upload endpoint (re-validating every module); the UI backend fetches, verifies and fans the module out through the existing fleet upload. No outbound internet from the DDoS-exposed edge; one place for trust decisions. This replaces `POST /admin/sniffers/install` from the transition plan (spec section "Where the client lives").
5. **A bad `.wasm` already poisons every rescan today.** `SnifferLoader::scan` fails the whole scan on one uncompilable file, so one bad upload keeps the old sniffer set forever (issue #171's real impact). Plan #3 fixes the scan as well as the upload.
6. **Pinned instances must refuse the file, not just fail the scan.** With `settings.sniffers.modules` set, a stray unpinned `.wasm` makes the next proxy startup fatal. Plan #3 makes the proxy reject (`409`, nothing written) any upload or rollback the pins would reject; the UI shows the pin line to add. Automatic pin editing is out of scope for the first cut. (Found in review of PR #193.)
6b. **`schema_version` needs a bump rule.** Plan #7 and the versioning spec now define it as the minimum schema a document needs, with a `FIELD_SINCE` table, `deny_unknown_fields` kept, and a controller check against proxies' reported `max_config_schema`. Config is the one surface where proxies are upgraded before the controller.
7. **Issues #183 and #184** still cite "#171 invalid .wasm" and "#172 upload accepts any bytes"; per the corrected numbering #171 is the upload bug. Fix when implementation starts (comment or edit; assignment of issues happens then too).

## Maintainer-only items collected from the plans

- Import `ruleset-main.json` (already on the manual list).
- Minisign key pair for the official sniffer registry: generate offline, publish the public key (to be compiled into the UI), store the secret and its password as `MINISIGN_SECRET_KEY` and `MINISIGN_PASSWORD` secrets in `wayhouse-proxy/sniffers`. Optional in the first cut, so this can wait.
- Tag push rights for threads (or push `v0.1.0-rc.1` and `v0.1.0` yourself).
- Package visibility check on the six GHCR packages after the first tag (private and linked to the repo), and a classic PAT with `read:packages` for pulling.
- Sniffers repo: allow the CI bot to push `index.json` to its default branch, or accept the release-asset fallback described in plan #11 Task A4.
- Possibly a `RELEASE_PLEASE_TOKEN` secret if the dispatched CI run does not satisfy the `ci-ok` requirement on release PRs.

## CI note

GitHub Actions is degraded as of this writing; the docs-only PR carrying these plans may show no CI. It touches only Markdown, so nothing code-related is unverified.
