# Tunnel address authority — `gsp-controller` allocates tunnel addresses

Date: 2026-10-02 · Status: implemented (slices 1–6, 2026-10-02) · Roadmap item 3 of 3 (after `deploy/`
and the docs/12 TLS section). Resolves the "Tunnel-internal address collision/exhaustion"
open question in [`docs/11`](../../11-backend-transport.md) and the
`known_bug_two_proxies_cannot_share_one_origin` defect.

## Intent

Today every tunnel address is chosen by an operator and self-reported: an origin states
its own interface address and its fronted backends (`gsp-agent --address`, `--backends`),
a proxy states `--tunnel-address`. Nothing checks them against each other, so two peers
that copy the same example config get an undefined, last-write-wins route. Separately,
`ProxyRegistration` carries no tunnel address, so `gsp-agent` gives every proxy peer
`AllowedIPs = 0.0.0.0/0` (`crates/gsp-agent/src/proxy_subscribe.rs:58`) and a second
proxy on the same origin steals the first one's route.

Make `gsp-controller` the **address authority**: it hands out unique tunnel addresses at
registration time, enforces uniqueness for hand-picked ones, and publishes each peer's
address so every other peer can route to it with a `/32`.

Success = the previously ignored `known_bug_two_proxies_cannot_share_one_origin` passes
(both proxies carry traffic at once), a duplicate address is refused with a clear error,
and an operator can bring up an origin and a proxy without choosing any tunnel address.

## Decisions made while designing (2026-10-02)

- **Hybrid authority.** A registration may omit its address (the controller allocates a
  unique one) or request a specific one (granted only if free). Uniqueness is enforced
  either way. Existing hand-picked deployments keep working with a one-flag change.
- **Release is explicit, with a stale warning.** No automatic lease expiry.
- **Approach A:** an address table inside the controller's registration handlers, atomic
  with the registration. (Rejected: deriving addresses from the pubkey — collisions at a
  few hundred peers in a `/16`; a separate address service — allocation must be atomic
  with registration.)
- **Native TLS for the controller is wanted later** (separate item, see HANDOVER). New code
  must not hard-code `http://`; client calls take the base URL as given.

## Non-goals / Future work

Recorded in `HANDOVER.md` "Known follow-ups" so they are not lost; the owner may want
each later:

- **IPv6** tunnel networks (v1 is IPv4 only).
- **HA-replicated allocation.** The registries are not Raft-integrated and a lock-based
  allocator is correct only with a single writer, so `--tunnel-network` together with
  `--ha-peers` is refused at startup. Pin-only mode under `--ha-peers` is allowed but
  enforces uniqueness per controller node only (startup warning), as the registries are not replicated.
- **Automatic lease expiry / auto-release** (v1: explicit release + stale warning).
- **A `gsp-ui` view** of `GET /tunnel/addresses`.
- **Changing a live peer's address without a restart** (v1 logs the mismatch and keeps
  running on the old address).
- **`TunnelSource` dropping pool entries when an origin is deleted** (a `404` still means
  "keep last-known-good").

Also out of scope: relaying address state through `--role slave`, a metrics surface for
the address table, and per-pool subnet partitioning.

## Controller

### Configuration

- `gsp-controller --tunnel-network <CIDR>` (e.g. `10.60.0.0/16`) enables allocation.
  IPv4 only; the prefix must be `/30` or shorter. Invalid → startup error.
- Without it the controller runs in **pin-only mode**: pinned addresses are still checked
  for uniqueness, nothing is allocated, and a registration that omits its address is
  rejected (`422`).
- `--tunnel-stale-after <duration>` (default `14d`; `0` disables the warning).
- `--tunnel-network` together with `--ha-peers` is refused at startup with a message
  pointing at this spec.

### State

A new sled database `tunnel-addresses/` under `--data-dir` (one database per registry,
matching the existing pattern), with two trees:

- `by_owner`: `(role, name)` → `{ address, first_seen, last_seen }`, `role` ∈
  `origin | proxy`.
- `by_address`: `address` → `(role, name)`.

Names are unique per role; **addresses are unique globally**, because origins and proxies
share one tunnel network. Both registries hold one `Arc<AddressBook>` (new module
`crates/gsp-controller/src/addresses.rs`). A claim or allocation is one short critical
section plus one sled transaction across both trees, so a crash cannot leave them
disagreeing.

### Allocation and pinning rules (`POST /peers`, `POST /proxy-peers`)

1. Address omitted, owner already has one → the owner keeps it (**sticky**).
2. Address omitted, new owner, network configured → the **lowest free host address** in
   the network (network and broadcast addresses excluded). Network exhausted → `503`.
3. Address omitted, no network configured → `422` (pin-only mode).
4. Address pinned and free (and inside the network, if one is configured) → granted.
5. Address pinned and held by another owner → `409` naming that owner.
6. Address pinned outside the configured network → `422`.
7. Owner already has a **different** address (pinned or allocated) → `409`; it must be
   released first (below).
8. Every registration is idempotent: the periodic re-registration returns the same address
   and updates `last_seen`.

### Backends

`backends` entries are `host:port` as today, or the shorthand `:port` meaning "my tunnel
address plus this port"; the controller expands the shorthand when it stores the
registration. After expansion, **every backend host must equal the registrant's own tunnel
address** (else `422`). This is a deliberate tightening — nothing in the repo uses another
host — and it is what makes overlapping `AllowedIPs` impossible by construction.

### Wire changes (clean break, pre-1.0)

- `PeerRegistration` and `ProxyRegistration` gain `tunnel_address` (optional on request,
  always set once stored). Stored registrations and SSE events carry it.
- The `POST` response grows from `{revision}` to
  `{revision, tunnel_address, tunnel_network}` (`tunnel_network` absent in pin-only mode).
- The subscribe stream's event payload is either `{"revision":N,"registration":{…}}` as
  today, or a tombstone `{"revision":N,"removed":{"name":"…"}}` (see Release).

### Release

- `DELETE /peers/{name}` and `DELETE /proxy-peers/{name}`, behind each registry's existing
  bearer-token gate; unknown name → `404`.
- One transaction removes the current registration, frees the address in the book, and
  appends a tombstone to that registry's log.
- If the owner is still running it re-registers on its next tick and gets a **fresh**
  allocation (not sticky). To change an address: stop the process, `DELETE`, restart.

### Stale warning

- `last_seen` is updated on every registration.
- `GET /tunnel/addresses` (token-gated) returns `{ network, allocated, capacity, entries }`;
  each entry has `role`, `name`, `address`, `first_seen`, `last_seen`, `stale`.
- An entry is `stale` after `--tunnel-stale-after` without a registration. At startup and
  every 24 h the controller logs one `WARN` with the stale count and up to ten names.
  Nothing is freed automatically.

### Errors

JSON `{"error": "…"}` bodies as elsewhere: `409` (address held / owner has a different
address), `422` (invalid or out-of-network address, backend host mismatch, no address in
pin-only mode), `503` (network exhausted, with the network and counts), `404` (`DELETE` of
an unknown name). Allocation, pin and release are logged at `info`.

Trust model is unchanged: anyone holding the registry token is trusted; they can claim
free addresses but not take another owner's.

## Clients (`gsp-agent` and `gsp --tunnel-*`)

### Startup order (both)

1. Load or generate the WireGuard key (the public key exists before any interface does).
2. **One synchronous registration**, retried with backoff for ~30 s. The response gives
   `tunnel_address` and `tunnel_network`.
3. Bring the interface up as `<address>/<network prefix>`.
4. Start the periodic re-registration and the subscriptions, as today.

For the proxy, steps 1–3 happen before any listener binds, so "a failing `--tunnel-*`
leaves zero listeners bound" still holds (a registration failure is a failing `--tunnel-*`).

### Flags

`--address` (agent) and `--tunnel-address` (proxy) become optional. A CIDR such as
`10.60.0.2/24` pins that address (sent as `tunnel_address`; its prefix length is used for
the interface). Omitted → allocate. `--peer-pubkey` / `--peer-endpoint` on the agent gain
a required companion `--peer-address` so a manual pin also routes a `/32`.

### Persistence and offline behaviour

The assigned address is saved next to the key: `<data_dir>/tunnel-address` (agent) and
`<tunnel-key-file>.address` (proxy). Controller down at start with a saved address →
start anyway; the periodic registration reconciles later. Controller down with no saved
address → startup fails with a clear error. If a later registration returns a **different**
address than the running one, the process logs an error naming both and **keeps
running** (it must not kill live traffic over a registry change); a restart applies it.

### Routing (the bug fix)

- `gsp-agent` builds each proxy peer with `AllowedIPs = <proxy.tunnel_address>/32`
  instead of `0.0.0.0/0`.
- `gsp`'s `tunnel_client::to_wg_peer` uses `<origin.tunnel_address>/32` instead of deriving
  routes from the backend list, so an origin with no backends yet is still reachable.
- On a tombstone both remove the WireGuard peer (looking its pubkey up in `last_applied`)
  and drop the entry. This is the "gone for good" signal that did not exist before.

## Repository changes

- Code: new `crates/gsp-controller/src/addresses.rs`; changes to `peers.rs`,
  `peers/api.rs`, `proxy_peers.rs`, `proxy_peers/api.rs`, controller `main.rs`;
  `gsp-agent` `main.rs`, `register.rs`, `proxy_subscribe.rs`; `gsp` `main.rs`,
  `tunnel_client.rs`, `proxy_register.rs`.
- Docs: `docs/11` (the open question becomes a resolved "Address authority" section),
  `docs/08`, `docs/12` (tunnel section), `README.md`, `AGENTS.md` (module list, commands),
  `HANDOVER.md` (remove the known-bug and "Tunnel address authority" entries).
- Deploy: `deploy/compose/compose.tunnel.yml` switches to allocation (`--tunnel-network` on
  the controller, no hand-picked addresses); `deploy/lint.sh` checks follow.
- E2E: remove `--skip known_bug_` from the `tunnel-e2e` and `tunnel-e2e-ci` targets and
  rename/un-ignore the reproducer.

## Testing

TDD throughout; each layer's tests fail first.

1. **Address book (unit):** lowest-free allocation; stickiness; network/broadcast skipped;
   exhaustion; pin free / conflict / outside network / owner-has-a-different-address;
   release then re-allocate; persistence across a reopen (use the sled-reopen retry
   HANDOVER documents); 64 concurrent registrations never share an address; staleness with
   an injected clock; CIDR validation (IPv6 and prefixes longer than `/30` rejected).
2. **Controller HTTP (in-module):** `POST` returns address and network and is idempotent;
   `:port` expansion; backend-host mismatch → `422`; `DELETE` frees the address and emits a
   tombstone a catch-up subscriber receives; `GET /tunnel/addresses` with the stale flag;
   every new route requires the token; `--tunnel-network` + `--ha-peers` refused.
3. **Client unit:** agent peer for a proxy is `<addr>/32`; proxy peer for an origin is
   `<addr>/32`; saved address round-trips; address mismatch logs and continues; a tombstone
   removes the peer and clears `last_applied`.
4. **End-to-end (`make tunnel-e2e`, both WireGuard backends in CI):** the harness starts
   the controller with `--tunnel-network` and hand-picks no addresses; existing scenarios
   still pass; the un-ignored two-proxies test passes; a pinned collision between two
   agents is refused with a clear error; a restart keeps its address, including with the
   controller down.

## Build order

Six slices, each committed once green: (1) address book; (2) controller wiring — flags,
HA guard, allocation/pinning on `POST`, backend rules, `GET /tunnel/addresses`, stale log;
(3) `DELETE` + tombstones in both registries; (4) agent and proxy changes; (5) e2e harness
rewrite and new scenarios, un-ignore the bug test; (6) deploy examples and docs. Between
slices 2 and 4 the old clients send no address, so `make tunnel-e2e` is red then;
`make check` stays green throughout and slice 5 restores the e2e.

## Review focus

Restart stickiness; concurrent registration; tombstone replay on catch-up;
delete-then-re-register; the clock behind staleness; the sled reopen race; the interface
prefix changing from `/24` to the network's prefix (e.g. `/16`) in the netns lab; and
boringtun's panic on a same-pubkey `configure_peer`, which makes the removal path
backend-sensitive.

## Points the owner may want to change

- The `422` rule that restricts backend hosts to the registrant's own address.
- "Log and keep running" when the controller later reports a different address (vs.
  exiting).
- `503` for exhaustion, and the 14-day default stale threshold.
