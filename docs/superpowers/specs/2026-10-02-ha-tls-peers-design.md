# TLS-capable HA peer addresses + readable HTTP errors — design

Date: 2026-10-02. Status: approved (owner asked for autonomous progress through the
superpowers stages). Second step of the "native TLS" umbrella in `HANDOVER.md`, after
`--ca-file` (`2026-10-02-custom-ca-design.md`).

## Problem

1. **HA replica traffic is pinned to plain HTTP.** `gsp-controller` builds
   `http://{addr}{path}` for Raft RPCs (`ha/network.rs`) and for forwarding a write to
   the leader (`ha/client.rs`), where `addr` is the `host:port` from `--ha-peers`. So
   replica-to-replica traffic cannot go through the TLS terminator docs/12 recommends —
   it must stay on a private network, protected only by `--ha-token` (a shared secret,
   not encryption).
2. **HANDOVER/docs/12 also list `/admin/adopt` as hard-coded.** That is wrong: adopt
   takes a full `parent_url` from the request body (`adopt.rs:120`), so it already
   works over `https://` (and trusts `--ca-file`). The only `http://` in `adopt.rs` is
   a test mock. Docs need correcting, no code change.
3. **HTTP errors hide their cause.** Most clients format a `reqwest::Error` with `{e}`;
   its `Display` omits the source chain, so a TLS/CA failure reads only "error sending
   request for url (…)". `controller_client` was fixed during the `--ca-file` work; the
   rest were deferred to a HANDOVER row.

## Goal and success criteria

- A `--ha-peers` entry may be a base URL, `id=https://host[:port]` (or `http://…`), and
  every replica-to-replica call to that peer uses it. The old `id=host:port` form keeps
  meaning plain HTTP, byte-for-byte as today.
- Proven end to end: three real `gsp-controller` replicas, each reachable by its peers
  **only** through a private-CA TLS terminator (`--ca-file` on every replica), elect a
  leader, accept a write sent to a **follower** (forwarded over HTTPS), and serve it from
  every replica.
- An invalid peer URL (other scheme, a path, a query) is a startup error naming the
  entry.
- Every outbound-HTTP error message in the five binaries includes the cause chain.
- docs/12 + HANDOVER stop claiming adopt is hard-coded.

Out of scope: TLS served by the controller itself; changing the address of a member of
an **already-bootstrapped** cluster (see Limits); mTLS between replicas.

## Approaches considered

1. **Per-peer base URL in `--ha-peers` (chosen).** Scheme travels with the address, so
   one replica can sit behind a terminator while another is still plain during a
   migration of a *new* cluster; no new flag; old form unchanged; `BasicNode.addr` stays
   a string, so the Raft log/snapshot format is unchanged.
2. **One `--ha-scheme http|https` flag.** Simpler to explain, but all-or-nothing, and an
   `https` scheme with a bare `host:port` hides where TLS terminates.
3. **Native TLS on the controller listener.** The umbrella's last piece; needs its own
   design (certificate loading/rotation). Not this slice.

## Design

### Peer addresses (`gsp-controller`)

New `ha::peer_url(addr: &str, path: &str) -> String`:
- `addr` contains `://` → `addr` with any trailing `/` trimmed, then `path`.
- otherwise → `http://{addr}{path}` (today's behaviour).

Both `network.rs` (Raft RPCs) and `client.rs::forward_to_leader` use it. Nothing else
builds peer URLs.

`parse_ha_peers` validates each URL-form entry with `reqwest::Url`: scheme `http` or
`https`, a host, path empty or `/`, no query/fragment/userinfo; anything else →
`--ha-peers entry "…": …` startup error. The `host:port` form is not newly validated
(no behaviour change for existing deployments).

`self_addr` (used to detect a stale self-forward) is still taken from the same
`--ha-peers` entry, so it stays comparable with the leader's `BasicNode.addr`.

### Limits

`--ha-peers` only bootstraps the cluster (`raft.initialize`); afterwards membership —
including each member's address — lives in the replicated log. Editing `--ha-peers` on
an existing cluster therefore does **not** change the addresses replicas use; moving a
running cluster from `host:port` to `https://` needs a membership change, which is not
built. Documented in docs/12 and as a HANDOVER follow-up.

### Readable errors (`gsp-http`)

New `gsp_http::error_chain(e: &(dyn std::error::Error + 'static)) -> String`: the
error's `Display` followed by each `source()`, joined with `": "`, skipping a source
whose text is already contained in the previous one (reqwest/hyper repeat themselves).
Every place in the five binaries that turns a `reqwest::Error` into text (`anyhow!`
messages, `tracing` fields) uses it. `controller_client` keeps its `anyhow::Context`
form (already chained).

## Testing

- `ha::peer_url` unit tests: `host:port` → `http://host:port/raft/vote`;
  `https://h:8443` and `https://h:8443/` → `https://h:8443/raft/vote`.
- `parse_ha_peers` unit tests: accepts both forms; rejects `ftp://`, a path, a query —
  each error names the entry.
- `gsp_http::error_chain` unit test: a request to the private-CA fixture server with a
  plain client yields text containing the certificate failure (e.g. `UnknownIssuer`),
  which plain `{e}` does not.
- `gsp-fleet-tests` `ha_tls.rs`: three controllers on plain loopback ports, each with a
  `tls_front`; `--ha-peers 1=https://localhost:<f1>,2=…,3=…`, `--ca-file` = test CA;
  retry `POST /config` on replica 1's plain port until a leader exists (503 → 2xx), then
  `POST /config` once to **each** replica's plain port — at least two of the three are
  followers, so forwarding to the leader's https URL is exercised whichever node leads —
  then `GET /config` on all three returns the same, latest revision. Without `--ca-file`
  the same cluster never elects a leader (writes stay 503 for a bounded wait) — proves
  replica traffic really crosses TLS with verification on.

## Docs

docs/12 (HA over TLS, the bootstrap-only limit, adopt correction); docs/10 / `--ha-peers`
help text (URL form); HANDOVER (drop/replace the "TLS for HA / adopt" and "terse reqwest
errors" rows, refresh "Resume here", fold in the deferred `--ca-file` review minors that
touch the same files).
