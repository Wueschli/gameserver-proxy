# IPv6 tunnel networks and IPv6 underlay

Date: 2026-10-03 · Status: designed, not built · Follows the
[tunnel address authority](2026-10-02-tunnel-address-authority-design.md) spec, whose
"Future work" lists IPv6 tunnel networks. Closes the IPv6 part of HANDOVER's
"Tunnel address authority — deferred pieces" row.

## Intent

The tunnel is mostly managed automatically, so the addresses *inside* it should come from
an IPv6 pool by default: the space is effectively unlimited, and a ULA network
(`fd00::/8`) cannot clash with an origin's own `10.x`/`192.168.x` LAN. Origins must still be
reachable when they have only an IPv4 address, and game servers that bind `0.0.0.0` only
must still work. Two independent things therefore change:

- **Tunnel network (inner addresses):** `--tunnel-network` accepts an IPv6 network as well
  as an IPv4 one. One family per controller; IPv6 is the documented default; IPv4 stays
  fully supported (for IPv4-only game servers, and so existing setups keep running untouched).
- **Underlay (outer WireGuard endpoints):** the public endpoints and the controller URL may
  be IPv4 or IPv6, independent of the tunnel's family. Any inner family over any outer
  family.

Client IPv6 addresses need neither change: they reach the origin over the PROXY protocol,
not as tunnel addresses.

Success = with `--tunnel-network fd49:89c1:4b5e:60::/64` an origin and two proxies come up
with no hand-picked addresses and carry traffic; the same works over an IPv6-only underlay;
an IPv4 tunnel network still passes every existing scenario; and restarting the controller
on a network that no longer contains a stored address is refused until the operator opts
in to re-addressing.

## Decisions made while designing (2026-10-03, with the owner)

- **Scope: both** inner and outer IPv6.
- **One address per peer, one family per controller.** Not dual stack: two addresses per
  peer would double every allocation, pinning, stickiness and release rule.
- **IPv6 is the default in docs and examples; IPv4 tunnel networks stay supported.** A game
  server that binds `0.0.0.0` only cannot be reached on an IPv6 tunnel address.
- **Extend the existing address book; no second book.** `Network` becomes family-aware,
  addresses become `IpAddr`; every rule of the address authority spec is unchanged.
- **A changed network is refused at startup** when stored addresses fall outside it, until
  the operator passes `--tunnel-readdress` (see "Changing the network"). Re-addressing
  interrupts traffic, so it must be deliberate and never the result of a mistyped flag.

## Non-goals

Dual-stack tunnels (two addresses per peer); hostnames in WireGuard endpoints (endpoints
stay `ip:port`); NAT64/DNS64 for IPv4-only game servers on an IPv6 tunnel; IPv6 for the
`--role slave` relay; auto-generating a ULA prefix. HANDOVER's separate
"`IPV6_TRANSPARENT` on musl" row is unrelated (it is about the proxy's listeners) and stays
open.

## Controller

### `--tunnel-network`

- IPv4: unchanged (`/16` to `/30`).
- IPv6: prefix `/64` to `/120`. `/64` is the normal subnet size; the upper bound leaves
  at least 254 hosts and keeps the host part within a `u64`, so the arithmetic matches
  IPv4's. Host bits are masked off, as for IPv4. A prefix shorter than `/64` is refused
  with a message saying the pool is a single subnet.
- Allocation is lowest-free starting at host 1 (`…::1`). The network address (`…::`, the
  subnet-router anycast address) and the all-ones address are excluded, mirroring IPv4's
  network and broadcast exclusions, so the capacity rule is the same for both families.
- **Pool cap.** Both families are capped at 65 534 allocated entries
  (`MAX_ENTRIES`). The existing `/16` cap was never about address space but about the
  linear scan under the book's mutex and sled's O(n) `Tree::len()`; an IPv6 `/64` keeps
  that limit as an entry count. `capacity` (in `GET /tunnel/addresses` and the `503`)
  becomes `min(host count, MAX_ENTRIES)`; the exhaustion error is the same.
- Pins: an IPv6 pin is accepted when the network is IPv6; a pin of the other family is
  `422` "outside the tunnel network" (same rule as an out-of-network pin today).
- **Pin-only mode** (no network) accepts pins of either family. Mixing families in one
  pin-only deployment is the operator's responsibility (peers of different families cannot
  route to each other); docs/11 says so. Pin-only host checks extend to IPv6: unspecified,
  loopback, multicast, and link-local (`fe80::/10`) are refused.

### Address book (`crates/gsp-controller/src/addresses.rs`)

- `Network` holds a family tag plus the network as a `u128` (IPv4 in the low 32 bits) and
  the prefix; `contains`, `is_host`, `capacity`, `host(n)` work for both. `Display` prints
  the canonical form (`fd49:89c1:4b5e:60::/64`).
- `Assignment.address`, `ClaimError` fields, `claim`'s `requested`, `release`'s return and
  `expand_backends`' `own` become `IpAddr`.
- **Storage is unchanged.** `Assignment` is stored as JSON, and serde writes both
  `Ipv4Addr` and `IpAddr` as the same string, so existing records decode. The
  `by_address` key stays the address's raw octets: 4 bytes for IPv4, 16 for IPv6, so keys
  of different families never collide and existing databases open unchanged. A unit test
  opens a database written with today's IPv4 code.

### Registrations and backends

- `PeerRegistration::validate` / `ProxyRegistration::validate` accept an IPv6
  `tunnel_address`; `requested_address()` returns `IpAddr`.
- Backends with an IPv6 host use the bracketed `SocketAddr` form, `[fd49:…::2]:25565`;
  the `:port` shorthand expands to it. The "backend host must equal the registrant's own
  address" rule is unchanged (an IPv4 backend on an IPv6 registrant is `422`).
- `endpoint` validation already parses a `SocketAddr`, so `[2001:db8::7]:51820` is valid;
  add tests to pin that.
- Wire format: unchanged. `tunnel_address` and `tunnel_network` are strings and may now
  hold IPv6 text.

### Changing the network (`--tunnel-readdress`)

At startup, when a network is configured, the controller scans the book for entries whose
address is not a host of the network (other family, outside the prefix, or now the
network/all-ones address):

- **None:** start as usual.
- **Some, without `--tunnel-readdress`:** refuse to start. The error names the network, the
  count and up to ten `role name address` entries, and says to either restore the previous
  `--tunnel-network`, or pass `--tunnel-readdress` to move these peers, or `DELETE` them
  (after restarting on the previous network).
- **Some, with `--tunnel-readdress`:** start, log one `WARN` with the same summary, and on
  each such owner's next registration release its old address and claim as a new owner
  (lowest free, or its pin if the pin is valid). Logged at `info` per owner, like any
  allocation. The new registration replaces the old one in the log, so subscribers swap the
  peer's route (the existing remove-then-configure path, which boringtun needs). The
  owner's running process logs the existing "controller now assigns a different address"
  error and keeps running; its restart applies the new address. A pinned owner whose pin
  is now outside the network gets `422` until its pin is changed.
- The flag is harmless when nothing is outside the network, so it can be left on during a
  migration and removed afterwards. Without a network (pin-only) it is a startup error, as
  there is nothing to re-address into.

This also resolves the HANDOVER minor "stored addresses are not re-validated if
`--tunnel-network` later changes".

## Clients (`gsp-agent`, `gsp --tunnel-*`)

- **Routes:** every peer is a host route built with `IpAddrMask::host(ip)`, which is `/32`
  for IPv4 and `/128` for IPv6: `gsp`'s `tunnel_client::to_wg_peer`, `gsp-agent`'s
  `proxy_subscribe` peer builder, and the manual `--peer-*` peer in `gsp-agent`'s `main.rs`.
- **Flags:** `--address` (agent), `--tunnel-address` (proxy) and `--peer-address` (agent)
  accept IPv6 (`fd49:89c1:4b5e:60::5/64`, bare `fd49:89c1:4b5e:60::9` for `--peer-address`).
  The IPv4-only parses in `gsp-agent/src/main.rs` and `gsp/src/main.rs` become `IpAddr`.
- **Address comparison:** both `address_change` functions (`gsp-agent/src/register.rs`,
  `gsp/src/proxy_register.rs`) compare strings today. With IPv6 a pin written as
  `fd49:0::5` and the controller's canonical `fd49::5` are the same address, so they parse
  both sides and compare `IpAddr`s.
- **Interface:** brought up as `<address>/<network prefix>`, as today. The saved
  `tunnel-address` file holds whichever CIDR it was given.
- **Data plane:** unchanged. The proxy dials `[fd49:…::2]:25565` like any `SocketAddr`.
- **Underlay:** `--endpoint`, `--tunnel-endpoint` and `--peer-endpoint` accept
  `[v6]:port`; `defguard_wireguard_rs` resolves endpoints with `to_socket_addrs`, which
  handles both. A controller URL such as `http://[fd99::1]:7070` is passed through as given.

### To verify in the lab rather than assume

- **Duplicate address detection.** The kernel holds a new IPv6 address "tentative" for about
  a second, and binding to it fails meanwhile. Linux skips DAD on `IFF_NOARP` interfaces,
  which WireGuard and TUN devices are (inferred from the kernel source, not yet observed).
  If an e2e run shows a tentative address, add `IFA_F_NODAD` (or wait for the address to
  leave the tentative state) in the interface setup of both binaries.
- **MTU.** Both binaries leave the MTU at the backend default (1420), which already leaves
  room for WireGuard's 80-byte overhead over an IPv6 underlay and is above IPv6's 1280
  minimum. Confirm boringtun's default matches.
- **boringtun's listening socket** accepts IPv6 peers (an IPv6 underlay scenario on the
  boringtun backend proves it).
- **Containers.** Docker can start containers with `net.ipv6.conf.all.disable_ipv6=1` when
  their network has no IPv6. The Compose tunnel overlay sets
  `sysctls: net.ipv6.conf.all.disable_ipv6: "0"` on the tunnel services; the Compose
  tunnel path is lint-only in CI, so this stays "verified in CI only" like the rest of
  `deploy/`. The Kubernetes DaemonSet example gets the same note.

## HA interaction (parallel spec: HA-replicated address allocation)

A separate thread is designing Raft replication of the peer registries and this address
book. This spec only needs the replicated book to be **family-agnostic**, which it is by
construction if it replicates `Assignment` (JSON) and keys by raw octets as above. Two
points for that spec to carry:

- Every controller node must run with the **same** `--tunnel-network` (family and prefix);
  a node that allocates from a different network would hand out addresses the others treat
  as outside. How a mismatch is detected belongs to the HA spec.
- The `--tunnel-readdress` scan and the per-owner re-address must run as replicated writes
  (on the leader), not per node.

Whichever lands second adapts to the other; neither blocks the other.

## Repository changes

- Code: `gsp-controller` `addresses.rs`, `addresses/api.rs`, `peers.rs`, `proxy_peers.rs`,
  `peers/api.rs`, `proxy_peers/api.rs`, `main.rs` (the new flag and startup scan);
  `gsp-agent` `main.rs`, `register.rs`, `proxy_subscribe.rs`; `gsp` `main.rs`,
  `proxy_register.rs`, `tunnel_client.rs`.
- Tests: `gsp-fleet-tests` `src/netns.rs` (IPv6 on the lab veths, with `nodad` so they are
  usable at once), `src/tunnel.rs` (the tunnel network becomes a parameter, IPv6 by default),
  `tests/tunnel.rs`, `tests/tunnel_addresses.rs`.
- Deploy: `deploy/compose/compose.tunnel.yml` (`--tunnel-network=fd49:89c1:4b5e:60::/64`,
  the sysctl), `deploy/lint.sh` (expects the IPv6 network), `deploy/README.md`,
  `deploy/k8s/50-gsp-daemonset.yaml` comments if they mention the network.
- Docs: `docs/11` ("Address authority" gains IPv6, the underlay note and
  `--tunnel-readdress`), `docs/12` (tunnel section: how to generate a random ULA
  `fdXX:XXXX:XXXX::/48` and pick a `/64` from it, and when to choose IPv4 instead),
  `docs/08`, `README.md`, `AGENTS.md` (commands), `HANDOVER.md` (drop IPv6 from the
  deferred row and the re-validation minor), `docs/superpowers/README.md` (index row).

## Testing

TDD throughout; each layer's tests fail first.

1. **Address book (unit):** IPv6 parse (`/64` and `/120` accepted, `/63` and `/121`
   refused, host bits masked, canonical display); lowest-free from `::1`; network and
   all-ones addresses skipped; `capacity` is `min(hosts, MAX_ENTRIES)` and exhaustion at
   the cap; pin of the other family refused; pin-only IPv6 host checks (link-local,
   loopback, multicast, unspecified); a database written by the IPv4-only code reopens and
   keeps its entries; 64 concurrent IPv6 registrations never share an address.
2. **Startup scan:** entries outside the network refuse startup and are named; with
   `--tunnel-readdress` startup succeeds, the next registration of an affected owner gets a
   fresh address in the new network and the old address is free; unaffected owners keep
   theirs; `--tunnel-readdress` without a network is refused.
3. **Controller HTTP (in-module and `tests/tunnel_addresses.rs`):** `POST` with an IPv6
   network returns an IPv6 address and network; `:port` expands to the bracketed form;
   an IPv4 backend on an IPv6 registrant is `422`; an IPv6 `endpoint` is accepted;
   `GET /tunnel/addresses` reports the capped capacity.
4. **Client unit:** host routes are `/128` for IPv6 and `/32` for IPv4 in all three peer
   builders; IPv6 `--address`/`--tunnel-address`/`--peer-address` parse; the saved address
   round-trips an IPv6 CIDR.
5. **End-to-end (`make tunnel-e2e`, both WireGuard backends in CI):** the lab's default
   tunnel network becomes IPv6 and every existing scenario passes on it; one scenario runs
   on an IPv4 tunnel network; a new scenario runs origin, proxy and controller over an
   IPv6-only underlay; a new scenario restarts the controller on a different network and
   checks the refusal, then `--tunnel-readdress` and an edge restart onto its new address.

## Build order

Five slices, each committed once green: (1) family-aware `Network` and `IpAddr` in the
address book, with the storage-compatibility test; (2) controller wiring: registration
validation, backends, the startup scan and `--tunnel-readdress`; (3) agent and proxy:
host routes and flag parsing; (4) lab IPv6 and the e2e scenarios, fixing whatever the lab
shows (DAD, MTU, boringtun); (5) deploy examples and docs. `make check` stays green
throughout; IPv4 behaviour is unchanged after every slice.

## Review focus

The `u128` arithmetic at the prefix bounds (`/64`, `/120`, `/16`, `/30`); the IPv4 storage
compatibility; the re-address path on boringtun (same pubkey, new route); DAD and MTU on
both backends; that a refused startup leaves no listener bound and no database change;
and the canonical IPv6 text form wherever an address is compared (the backend-host check
and both `address_change` functions must compare parsed addresses, not strings).

## Points the owner may want to change

- The `/64`–`/120` IPv6 prefix range and the 65 534-entry cap.
- Pin-only mode allowing mixed families instead of refusing the second family.
- The example network `fd49:89c1:4b5e:60::/64` (a randomly generated ULA prefix).
