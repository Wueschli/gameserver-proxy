# Tunnel Address Authority Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Status (checked 2026-10-03):** all 46 steps done. Every step is ticked after checking `main` (`10a0113`) for the files, tests and commits it names. "Run it to verify it fails" steps are ticked on the strength of the history (each task's tests and implementation landed), not re-run.
>
> - Landed 2026-10-02 in `375599d`..`bc7dba2`, follow-ups `de55f1c`, `b536e11`, `acfb577`, `6287359` (per-registry POST/DELETE serialisation, address-book flush, a wrong `gsp` flag name, an HA pin-only warning).
>
> - The deferred pieces of the address authority are tracked in HANDOVER's "Known follow-ups", not here.

**Goal:** `gsp-controller` allocates unique tunnel addresses (or grants requested ones), publishes each peer's address, and `gsp-agent` / `gsp --tunnel-*` route each other with `/32`s — fixing the multi-proxy `AllowedIPs 0.0.0.0/0` bug.

**Architecture:** One `AddressBook` (new `addresses` module, its own sled db) shared by the backend-peers and proxy-peers registries; `POST /peers` / `POST /proxy-peers` claim an address atomically with the registration and answer with it. Clients register *before* bringing their interface up, persist the answer, and build `/32` peers from the published addresses. `DELETE` releases an address and logs a tombstone subscribers act on.

**Tech Stack:** Rust (axum, sled, tokio, clap, reqwest, defguard_wireguard_rs), `cargo test`, the rootless `make tunnel-e2e` lab.

**Spec:** `docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md` (read it first; it is the authority for any conflict in this plan).

## Global Constraints

- Pre-1.0 clean break: no compatibility shims for the old wire shapes (spec: "Clean break").
- IPv4 only; `--tunnel-network` prefix must be `/30` or shorter.
- `--tunnel-network` together with `--ha-peers` is refused at startup.
- Addresses are unique **globally** (origins and proxies share one network); names are unique per role.
- Every backend host must equal the registrant's own tunnel address (after `:port` expansion), else `422`.
- Do not hard-code `http://` in any new client code: base URLs are used as given (native controller TLS is wanted later — see `memory/project_native-tls-controller-later`).
- No `unsafe`; no `#[allow]` without a one-line justification; metric names live in `metrics_defs.rs` (none are added here).
- AGENTS.md: run `cargo fmt --all` as its own step, then `make check`, before every commit; commit directly on `main` with the trailer `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>`; **do not push** unless the owner asks.
- Wire shapes are duplicated across binaries on purpose (existing precedent) — do not introduce a shared crate.
- The two registries are deliberately mirrored, not abstracted (existing precedent): apply each registry change to **both** `peers` and `proxy_peers`.

## Review Focus

Failure modes the spec implies that no single task's happy-path tests cover; each line names the test that pins it.

1. **A rejected registration still reserves an address.** The claim happens before the backend-host check, so a first registration with a bad backend returns `422` but owns an address. Pinned by Task 2's `a_rejected_registration_keeps_the_address_for_the_retry`.
2. **Old on-disk data.** Registry logs written before this change have no `tunnel_address`; they must still decode. Pinned by Task 2's `a_registration_from_an_older_log_still_decodes`.
3. **Tombstone replay on catch-up.** A subscriber starting from revision 0 sees an add *then* the removal for a deleted owner and must end with no peer. Pinned by Task 5/6's `plan` tests (`registered_then_removed_leaves_nothing`).
4. **Delete while the owner is still running.** It re-registers and is allocated afresh; it must not crash on the changed address. Pinned by Task 4's `re_registering_after_a_delete_is_allocated_afresh_not_sticky`, Task 5/6's `address_change_reports_only_a_real_difference` (the change is reported by `address_change`, never acted on) and Task 7's restart scenarios.
5. **Controller down at start.** A saved address must let the process start; no saved address must fail loudly before any listener binds. Pinned by Task 5/6's startup-source tests and Task 7's `an_edge_restarts_with_the_controller_down`.

## File Structure

| File | Responsibility |
|---|---|
| `crates/gsp-controller/src/addresses.rs` (new) | `Network`, `AddressBook` (claim/release/entries), `expand_backends`, `parse_duration`, `resolve_flags`, `is_stale` |
| `crates/gsp-controller/src/addresses/api.rs` (new) | `GET /tunnel/addresses`, `claim_error_response`, stale warning loop |
| `crates/gsp-controller/src/{peers,proxy_peers}.rs` | registration types gain `tunnel_address`; backend shorthand validation; tombstone helpers |
| `crates/gsp-controller/src/{peers,proxy_peers}/api.rs` | claim on `POST`, `DELETE`, tombstone event payload |
| `crates/gsp-controller/src/main.rs` | `--tunnel-network`, `--tunnel-stale-after`, HA guard, wiring |
| `crates/gsp-agent/src/{register,proxy_subscribe,address_store,main}.rs` | register-first startup, persisted address, `/32` proxy peers, tombstones |
| `crates/gsp/src/{proxy_register,tunnel_client,tunnel_address,main}.rs` | same for the proxy |
| `crates/gsp-fleet-tests/{src/lib.rs,src/tunnel.rs,tests/tunnel.rs,tests/tunnel_addresses.rs}` | harness + e2e scenarios |
| `deploy/**`, `docs/**`, `HANDOVER.md`, `AGENTS.md`, `README.md`, `Makefile` | examples and documentation |

---

### Task 1: The address book and its HTTP module

**Files:**
- Create: `crates/gsp-controller/src/addresses.rs`
- Create: `crates/gsp-controller/src/addresses/api.rs`
- Modify: `crates/gsp-controller/src/lib.rs` (add `pub mod addresses;` before `pub mod adopt;`)

**Interfaces:**
- Produces (used by Tasks 2–4):
  - `addresses::{Role, Network, Assignment, Entry, ClaimError, AddressBook, is_stale, now_secs, expand_backends, parse_duration}`
  - `AddressBook::open(&Path, Option<Network>) -> Result<Self, sled::Error>`; `claim(Role, &str, Option<Ipv4Addr>, u64) -> Result<Assignment, ClaimError>`; `release(Role, &str) -> Result<Option<Ipv4Addr>, ClaimError>`; `get`, `entries`, `allocated`, `network`
  - `addresses::api::{AddressesState, router, claim_error_response, warn_stale, stale_warning_loop}`

- [x] **Step 1: Write the failing tests**

Add `pub mod addresses;` to `lib.rs`. Create `crates/gsp-controller/src/addresses.rs` containing only `pub mod api;` followed by this test module:

```rust
pub mod api;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn book(network: Option<&str>) -> (AddressBook, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let net = network.map(|n| Network::parse(n).unwrap());
        (AddressBook::open(dir.path(), net).unwrap(), dir)
    }

    // sled releases its file lock on a background thread after a drop, so a
    // reopen right away can fail briefly (see HANDOVER "Known flakes").
    fn reopen(dir: &Path, network: Option<&str>) -> AddressBook {
        let net = network.map(|n| Network::parse(n).unwrap());
        for _ in 0..50 {
            if let Ok(b) = AddressBook::open(dir, net) {
                return b;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("could not reopen the address book");
    }

    #[test]
    fn network_parse_accepts_ipv4_and_masks_host_bits() {
        let n = Network::parse("10.60.0.7/16").unwrap();
        assert_eq!(n.to_string(), "10.60.0.0/16");
        assert_eq!(n.prefix(), 16);
        assert_eq!(n.capacity(), 65534);
    }

    #[test]
    fn network_parse_rejects_ipv6_bad_input_and_tiny_networks() {
        assert!(Network::parse("fd00::/64").unwrap_err().contains("IPv6"));
        assert!(Network::parse("10.60.0.0").is_err());
        assert!(Network::parse("10.60.0.0/x").is_err());
        assert!(Network::parse("10.60.0.0/31").is_err());
        assert!(Network::parse("10.60.0.0/32").is_err());
        assert!(Network::parse("10.60.0.0/30").is_ok());
    }

    #[test]
    fn network_host_excludes_network_and_broadcast() {
        let n = Network::parse("10.60.0.0/24").unwrap();
        assert!(!n.is_host(ip("10.60.0.0")));
        assert!(n.is_host(ip("10.60.0.1")));
        assert!(n.is_host(ip("10.60.0.254")));
        assert!(!n.is_host(ip("10.60.0.255")));
        assert!(!n.is_host(ip("10.61.0.1")));
    }

    #[test]
    fn allocation_hands_out_the_lowest_free_address_and_is_sticky() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        let a = b.claim(Role::Origin, "o1", None, 100).unwrap();
        let p = b.claim(Role::Proxy, "p1", None, 100).unwrap();
        assert_eq!(a.address, ip("10.60.0.1"));
        assert_eq!(p.address, ip("10.60.0.2"));
        // Re-registering returns the same address and bumps last_seen only.
        let again = b.claim(Role::Origin, "o1", None, 250).unwrap();
        assert_eq!(again.address, ip("10.60.0.1"));
        assert_eq!(again.first_seen, 100);
        assert_eq!(again.last_seen, 250);
    }

    #[test]
    fn the_same_name_in_two_roles_gets_two_addresses() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        let a = b.claim(Role::Origin, "x", None, 1).unwrap();
        let p = b.claim(Role::Proxy, "x", None, 1).unwrap();
        assert_ne!(a.address, p.address);
    }

    #[test]
    fn a_pinned_free_address_is_granted() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        let a = b
            .claim(Role::Origin, "o1", Some(ip("10.60.0.9")), 1)
            .unwrap();
        assert_eq!(a.address, ip("10.60.0.9"));
        // The allocator then skips it.
        let next = b.claim(Role::Origin, "o2", None, 1).unwrap();
        assert_eq!(next.address, ip("10.60.0.1"));
    }

    #[test]
    fn a_pin_held_by_another_owner_is_a_conflict_naming_the_holder() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim(Role::Origin, "o1", Some(ip("10.60.0.9")), 1)
            .unwrap();
        let err = b
            .claim(Role::Proxy, "p1", Some(ip("10.60.0.9")), 1)
            .unwrap_err();
        assert_eq!(
            err,
            ClaimError::Held {
                address: ip("10.60.0.9"),
                role: Role::Origin,
                name: "o1".into()
            }
        );
        assert!(err.to_string().contains("origin \"o1\""));
    }

    #[test]
    fn a_pin_outside_the_network_or_on_a_reserved_address_is_rejected() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        assert!(matches!(
            b.claim(Role::Origin, "o", Some(ip("10.61.0.1")), 1),
            Err(ClaimError::OutsideNetwork { .. })
        ));
        assert_eq!(
            b.claim(Role::Origin, "o", Some(ip("10.60.0.0")), 1),
            Err(ClaimError::NotHost(ip("10.60.0.0")))
        );
        assert_eq!(
            b.claim(Role::Origin, "o", Some(ip("10.60.0.255")), 1),
            Err(ClaimError::NotHost(ip("10.60.0.255")))
        );
    }

    #[test]
    fn an_owner_cannot_switch_addresses_without_releasing() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim(Role::Origin, "o1", None, 1).unwrap();
        let err = b
            .claim(Role::Origin, "o1", Some(ip("10.60.0.50")), 2)
            .unwrap_err();
        assert!(matches!(err, ClaimError::OwnerHasDifferent { .. }));
        // Asking for the address it already has is fine.
        assert!(b
            .claim(Role::Origin, "o1", Some(ip("10.60.0.1")), 3)
            .is_ok());
    }

    #[test]
    fn release_frees_the_address_for_the_next_owner() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim(Role::Origin, "o1", None, 1).unwrap();
        b.claim(Role::Origin, "o2", None, 1).unwrap();
        assert_eq!(
            b.release(Role::Origin, "o1").unwrap(),
            Some(ip("10.60.0.1"))
        );
        assert_eq!(b.release(Role::Origin, "o1").unwrap(), None);
        assert!(b.get(Role::Origin, "o1").unwrap().is_none());
        let n = b.claim(Role::Proxy, "p", None, 1).unwrap();
        assert_eq!(
            n.address,
            ip("10.60.0.1"),
            "the lowest free address is reused"
        );
    }

    #[test]
    fn an_exhausted_network_is_reported_with_counts() {
        let (b, _d) = book(Some("10.60.0.0/30")); // two hosts: .1 and .2
        b.claim(Role::Origin, "a", None, 1).unwrap();
        b.claim(Role::Origin, "b", None, 1).unwrap();
        match b.claim(Role::Origin, "c", None, 1).unwrap_err() {
            ClaimError::Exhausted {
                allocated,
                capacity,
                ..
            } => assert_eq!((allocated, capacity), (2, 2)),
            other => panic!("expected Exhausted, got {other:?}"),
        }
    }

    #[test]
    fn pin_only_mode_enforces_uniqueness_but_never_allocates() {
        let (b, _d) = book(None);
        assert_eq!(
            b.claim(Role::Origin, "o", None, 1),
            Err(ClaimError::NoNetwork)
        );
        b.claim(Role::Origin, "o", Some(ip("10.60.0.2")), 1)
            .unwrap();
        assert!(matches!(
            b.claim(Role::Proxy, "p", Some(ip("10.60.0.2")), 1),
            Err(ClaimError::Held { .. })
        ));
        assert_eq!(
            b.claim(Role::Proxy, "p", Some(ip("127.0.0.1")), 1),
            Err(ClaimError::NotHost(ip("127.0.0.1")))
        );
    }

    #[test]
    fn assignments_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let b = AddressBook::open(dir.path(), Network::parse("10.60.0.0/24").ok()).unwrap();
            b.claim(Role::Origin, "o1", None, 7).unwrap();
        }
        let b = reopen(dir.path(), Some("10.60.0.0/24"));
        let a = b.get(Role::Origin, "o1").unwrap().unwrap();
        assert_eq!(a.address, ip("10.60.0.1"));
        assert_eq!(a.first_seen, 7);
        // And the reverse index survived too: the address is still taken.
        assert!(matches!(
            b.claim(Role::Proxy, "p", Some(ip("10.60.0.1")), 8),
            Err(ClaimError::Held { .. })
        ));
    }

    #[test]
    fn concurrent_registrations_never_share_an_address() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        let b = Arc::new(b);
        let handles: Vec<_> = (0..64)
            .map(|i| {
                let b = b.clone();
                std::thread::spawn(move || {
                    b.claim(Role::Origin, &format!("o{i}"), None, 1)
                        .unwrap()
                        .address
                })
            })
            .collect();
        let mut got: Vec<Ipv4Addr> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        got.sort();
        got.dedup();
        assert_eq!(got.len(), 64, "every owner must get a distinct address");
        assert_eq!(b.allocated(), 64);
    }

    #[test]
    fn entries_lists_every_owner_with_its_role() {
        let (b, _d) = book(Some("10.60.0.0/24"));
        b.claim(Role::Origin, "o1", None, 1).unwrap();
        b.claim(Role::Proxy, "p1", None, 1).unwrap();
        let entries = b.entries().unwrap();
        let got: Vec<(Role, &str)> = entries.iter().map(|e| (e.role, e.name.as_str())).collect();
        assert_eq!(got, vec![(Role::Origin, "o1"), (Role::Proxy, "p1")]);
    }

    #[test]
    fn staleness_uses_the_injected_clock_and_zero_disables_it() {
        let a = Assignment {
            address: ip("10.60.0.1"),
            first_seen: 0,
            last_seen: 1000,
        };
        let day = Duration::from_secs(86400);
        assert!(!is_stale(&a, 1000 + 86400, day), "exactly at the limit");
        assert!(is_stale(&a, 1000 + 86401, day));
        assert!(!is_stale(&a, u64::MAX, Duration::ZERO), "0 disables");
    }

    #[test]
    fn expand_backends_resolves_the_port_shorthand_and_keeps_own_addresses() {
        let own = ip("10.60.0.5");
        let got = expand_backends(&[":25565".into(), "10.60.0.5:25566".into()], own).unwrap();
        assert_eq!(got, vec!["10.60.0.5:25565", "10.60.0.5:25566"]);
        assert!(expand_backends(&[], own).unwrap().is_empty());
    }

    #[test]
    fn expand_backends_rejects_other_hosts_and_malformed_entries() {
        let own = ip("10.60.0.5");
        let err = expand_backends(&["10.60.0.6:1".into()], own).unwrap_err();
        assert!(err.contains("registrant's own address"), "{err}");
        assert!(expand_backends(&[":notaport".into()], own).is_err());
        assert!(expand_backends(&["nonsense".into()], own).is_err());
        assert!(expand_backends(&["[::1]:1".into()], own).is_err());
    }

    #[test]
    fn parse_duration_understands_units_and_zero() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("12h").unwrap(), Duration::from_secs(43200));
        assert_eq!(
            parse_duration("14d").unwrap(),
            Duration::from_secs(14 * 86400)
        );
        assert!(parse_duration("14").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("14w").is_err());
    }
}
```

Create `crates/gsp-controller/src/addresses/api.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::addresses::{Network, Role};
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn state(token: Option<&str>) -> (AddressesState, Arc<AddressBook>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let book = Arc::new(
            AddressBook::open(dir.path(), Some(Network::parse("10.60.0.0/24").unwrap())).unwrap(),
        );
        let s = AddressesState::new(
            book.clone(),
            token.map(str::to_string),
            Duration::from_secs(86400),
        );
        (s, book, dir)
    }

    async fn get_json(app: Router, auth: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut req = HttpRequest::get("/tunnel/addresses");
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn the_table_lists_owners_with_counts_and_the_stale_flag() {
        let (s, book, _d) = state(None);
        // One fresh, one last seen long ago.
        book.claim(Role::Origin, "fresh", None, now_secs()).unwrap();
        book.claim(Role::Proxy, "old", None, 1).unwrap();
        let (status, body) = get_json(router(s), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["network"], "10.60.0.0/24");
        assert_eq!(body["allocated"], 2);
        assert_eq!(body["capacity"], 254);
        let entries = body["entries"].as_array().unwrap();
        let find = |n: &str| entries.iter().find(|e| e["name"] == n).unwrap();
        assert_eq!(find("fresh")["stale"], false);
        assert_eq!(find("old")["stale"], true);
        assert_eq!(find("old")["role"], "proxy");
        assert_eq!(find("fresh")["address"], "10.60.0.1");
    }

    #[tokio::test]
    async fn the_table_requires_the_bearer_token_when_one_is_configured() {
        let (s, _book, _d) = state(Some("secret"));
        let (status, _) = get_json(router(s.clone()), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = get_json(router(s), Some("Bearer secret")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn claim_errors_map_to_the_documented_statuses() {
        use std::net::Ipv4Addr;
        let a: Ipv4Addr = "10.60.0.1".parse().unwrap();
        let net = Network::parse("10.60.0.0/24").unwrap();
        let status = |e: ClaimError| claim_error_response(&e).status();
        assert_eq!(
            status(ClaimError::Held {
                address: a,
                role: Role::Origin,
                name: "x".into()
            }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(ClaimError::OwnerHasDifferent {
                role: Role::Origin,
                name: "x".into(),
                have: a,
                requested: a
            }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(ClaimError::OutsideNetwork {
                address: a,
                network: net
            }),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status(ClaimError::NotHost(a)),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status(ClaimError::NoNetwork),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status(ClaimError::Exhausted {
                network: net,
                allocated: 254,
                capacity: 254
            }),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(ClaimError::Storage("x".into())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn warn_stale_counts_only_stale_owners() {
        let (_s, book, _d) = state(None);
        book.claim(Role::Origin, "fresh", None, 100_000).unwrap();
        book.claim(Role::Origin, "old", None, 1).unwrap();
        assert_eq!(warn_stale(&book, Duration::from_secs(86400), 100_000), 1);
        assert_eq!(warn_stale(&book, Duration::ZERO, 100_000), 0);
    }
}
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gsp-controller addresses 2>&1 | tail -20`
Expected: compile errors such as ``cannot find type `AddressBook` in this scope`` / ``cannot find function `router```. (That is the RED: the types do not exist yet.)

- [x] **Step 3: Write the implementation**

Insert this at the very top of `crates/gsp-controller/src/addresses.rs` (above the `pub mod api;` line and the test module from Step 1):

```rust
//! Phase 14 tunnel address authority (`docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`):
//! `gsp-controller` allocates the tunnel-internal addresses that origins and
//! proxies used to pick (and self-report) by hand, and enforces that no two
//! peers ever hold the same one.
//!
//! One [`AddressBook`] is shared by the backend-peers and proxy-peers
//! registries, because origins and proxies share a single tunnel network.
//! Names are unique per [`Role`]; **addresses are unique globally**. Every
//! claim or release is one short critical section plus one `sled`
//! transaction across both trees, so a crash can never leave `by_owner` and
//! `by_address` disagreeing. A lock-based allocator is only correct with a
//! single writer, which is why `--tunnel-network` is refused together with
//! `--ha-peers` (see the spec's Future work).
//!
//! Time is passed in (`now`, unix seconds) rather than read here, so
//! staleness is testable without sleeping.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sled::transaction::Transactional;

/// Which registry an owner registered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Origin,
    Proxy,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Origin => "origin",
            Role::Proxy => "proxy",
        }
    }

    fn parse(s: &str) -> Option<Role> {
        match s {
            "origin" => Some(Role::Origin),
            "proxy" => Some(Role::Proxy),
            _ => None,
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The IPv4 network tunnel addresses are allocated from (`--tunnel-network`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    network: u32,
    prefix: u8,
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

impl Network {
    /// Parses `a.b.c.d/prefix`. IPv4 only; the prefix must leave at least two
    /// usable host addresses (`/30` or shorter). Host bits are masked off.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (addr, prefix) = s
            .split_once('/')
            .ok_or_else(|| format!("{s:?} is not in ip/prefix form (e.g. 10.60.0.0/16)"))?;
        if addr.contains(':') {
            return Err("IPv6 tunnel networks are not supported yet".into());
        }
        let ip: Ipv4Addr = addr
            .parse()
            .map_err(|e| format!("{addr:?} is not an IPv4 address: {e}"))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| format!("{prefix:?} is not a prefix length"))?;
        if prefix > 30 {
            return Err(format!(
                "prefix /{prefix} is too long: the network needs at least two usable host \
                 addresses (use /30 or shorter)"
            ));
        }
        Ok(Network {
            network: u32::from(ip) & mask(prefix),
            prefix,
        })
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    fn broadcast(&self) -> u32 {
        self.network | !mask(self.prefix)
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & mask(self.prefix) == self.network
    }

    /// Inside the network and neither its network nor broadcast address.
    pub fn is_host(&self, ip: Ipv4Addr) -> bool {
        self.contains(ip) && u32::from(ip) != self.network && u32::from(ip) != self.broadcast()
    }

    /// How many host addresses the network holds.
    pub fn capacity(&self) -> u64 {
        (1u64 << (32 - u32::from(self.prefix))) - 2
    }

    /// The `n`th host address, `n` in `1..=capacity()`.
    fn host(&self, n: u64) -> Ipv4Addr {
        Ipv4Addr::from(self.network + n as u32)
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", Ipv4Addr::from(self.network), self.prefix)
    }
}

/// One owner's address and when it was first/last seen (unix seconds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub address: Ipv4Addr,
    pub first_seen: u64,
    pub last_seen: u64,
}

/// An owner and its assignment, as listed by [`AddressBook::entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub role: Role,
    pub name: String,
    pub assignment: Assignment,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClaimError {
    #[error("address {address} is already held by {role} {name:?}")]
    Held {
        address: Ipv4Addr,
        role: Role,
        name: String,
    },
    #[error(
        "{role} {name:?} already has tunnel address {have}; release it first \
         (DELETE its registration) to use {requested}"
    )]
    OwnerHasDifferent {
        role: Role,
        name: String,
        have: Ipv4Addr,
        requested: Ipv4Addr,
    },
    #[error("address {address} is outside the tunnel network {network}")]
    OutsideNetwork { address: Ipv4Addr, network: Network },
    #[error("address {0} is not a usable tunnel host address")]
    NotHost(Ipv4Addr),
    #[error(
        "this controller has no --tunnel-network, so it cannot allocate an address: \
         request one explicitly"
    )]
    NoNetwork,
    #[error("tunnel network {network} is exhausted ({allocated}/{capacity} addresses allocated)")]
    Exhausted {
        network: Network,
        allocated: usize,
        capacity: u64,
    },
    #[error("tunnel address storage error: {0}")]
    Storage(String),
}

fn storage<E: std::fmt::Display>(e: E) -> ClaimError {
    ClaimError::Storage(e.to_string())
}

fn owner_key(role: Role, name: &str) -> Vec<u8> {
    format!("{role}/{name}").into_bytes()
}

fn parse_owner_key(key: &[u8]) -> Option<(Role, String)> {
    let s = std::str::from_utf8(key).ok()?;
    let (role, name) = s.split_once('/')?;
    Some((Role::parse(role)?, name.to_string()))
}

pub struct AddressBook {
    network: Option<Network>,
    by_owner: sled::Tree,
    by_address: sled::Tree,
    write: Mutex<()>,
    // Held so the database stays open for the trees' lifetime.
    _db: sled::Db,
}

impl AddressBook {
    /// Opens (creating if missing) the book under `dir`. `network` of `None`
    /// is pin-only mode: uniqueness is enforced but nothing is allocated.
    pub fn open(dir: &Path, network: Option<Network>) -> Result<Self, sled::Error> {
        let db = sled::open(dir)?;
        let by_owner = db.open_tree("by_owner")?;
        let by_address = db.open_tree("by_address")?;
        Ok(AddressBook {
            network,
            by_owner,
            by_address,
            write: Mutex::new(()),
            _db: db,
        })
    }

    pub fn network(&self) -> Option<Network> {
        self.network
    }

    /// Registers `(role, name)`'s address and returns it, applying the
    /// allocation and pinning rules of the spec: an existing owner keeps its
    /// address (idempotent re-registration), a new owner without a request
    /// gets the lowest free host address, a requested address is granted only
    /// if free and inside the network, and an owner can never silently switch
    /// to a different address.
    pub fn claim(
        &self,
        role: Role,
        name: &str,
        requested: Option<Ipv4Addr>,
        now: u64,
    ) -> Result<Assignment, ClaimError> {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let okey = owner_key(role, name);
        let existing = self.read_owner(&okey)?;

        let address = match (&existing, requested) {
            (Some(a), None) => a.address,
            (Some(a), Some(req)) if a.address == req => a.address,
            (Some(a), Some(req)) => {
                return Err(ClaimError::OwnerHasDifferent {
                    role,
                    name: name.to_string(),
                    have: a.address,
                    requested: req,
                })
            }
            (None, Some(req)) => {
                self.check_pin(req)?;
                req
            }
            (None, None) => self.allocate()?,
        };

        let assignment = Assignment {
            address,
            first_seen: existing.map(|a| a.first_seen).unwrap_or(now),
            last_seen: now,
        };
        let value = serde_json::to_vec(&assignment).map_err(storage)?;
        let addr_key = address.octets();
        (&self.by_owner, &self.by_address)
            .transaction(|(owner, addr)| {
                owner.insert(okey.as_slice(), value.as_slice())?;
                addr.insert(&addr_key[..], okey.as_slice())?;
                Ok::<(), sled::transaction::ConflictableTransactionError<()>>(())
            })
            .map_err(|e| ClaimError::Storage(format!("{e:?}")))?;
        Ok(assignment)
    }

    /// Frees `(role, name)`'s address. `Ok(None)` if it held none.
    pub fn release(&self, role: Role, name: &str) -> Result<Option<Ipv4Addr>, ClaimError> {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let okey = owner_key(role, name);
        let Some(existing) = self.read_owner(&okey)? else {
            return Ok(None);
        };
        let addr_key = existing.address.octets();
        (&self.by_owner, &self.by_address)
            .transaction(|(owner, addr)| {
                owner.remove(okey.as_slice())?;
                addr.remove(&addr_key[..])?;
                Ok::<(), sled::transaction::ConflictableTransactionError<()>>(())
            })
            .map_err(|e| ClaimError::Storage(format!("{e:?}")))?;
        Ok(Some(existing.address))
    }

    pub fn get(&self, role: Role, name: &str) -> Result<Option<Assignment>, ClaimError> {
        self.read_owner(&owner_key(role, name))
    }

    /// Every owner, ordered by key (role then name).
    pub fn entries(&self) -> Result<Vec<Entry>, ClaimError> {
        let mut out = Vec::new();
        for item in self.by_owner.iter() {
            let (key, value) = item.map_err(storage)?;
            let Some((role, name)) = parse_owner_key(&key) else {
                continue;
            };
            let assignment = serde_json::from_slice(&value).map_err(storage)?;
            out.push(Entry {
                role,
                name,
                assignment,
            });
        }
        Ok(out)
    }

    pub fn allocated(&self) -> usize {
        self.by_owner.len()
    }

    fn read_owner(&self, okey: &[u8]) -> Result<Option<Assignment>, ClaimError> {
        match self.by_owner.get(okey).map_err(storage)? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes).map_err(storage)?)),
            None => Ok(None),
        }
    }

    fn check_pin(&self, req: Ipv4Addr) -> Result<(), ClaimError> {
        match self.network {
            Some(net) => {
                if !net.contains(req) {
                    return Err(ClaimError::OutsideNetwork {
                        address: req,
                        network: net,
                    });
                }
                if !net.is_host(req) {
                    return Err(ClaimError::NotHost(req));
                }
            }
            None => {
                if req.is_unspecified()
                    || req.is_broadcast()
                    || req.is_multicast()
                    || req.is_loopback()
                {
                    return Err(ClaimError::NotHost(req));
                }
            }
        }
        if let Some(holder) = self.by_address.get(req.octets()).map_err(storage)? {
            if let Some((role, name)) = parse_owner_key(&holder) {
                return Err(ClaimError::Held {
                    address: req,
                    role,
                    name,
                });
            }
        }
        Ok(())
    }

    fn allocate(&self) -> Result<Ipv4Addr, ClaimError> {
        let net = self.network.ok_or(ClaimError::NoNetwork)?;
        for n in 1..=net.capacity() {
            let ip = net.host(n);
            if !self.by_address.contains_key(ip.octets()).map_err(storage)? {
                return Ok(ip);
            }
        }
        Err(ClaimError::Exhausted {
            network: net,
            allocated: self.by_owner.len(),
            capacity: net.capacity(),
        })
    }
}

/// True when `assignment` has not been seen for longer than `after`. A zero
/// `after` disables staleness.
pub fn is_stale(assignment: &Assignment, now: u64, after: Duration) -> bool {
    !after.is_zero() && now.saturating_sub(assignment.last_seen) > after.as_secs()
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolves a registration's `backends` against the registrant's own tunnel
/// address: `:port` means "my address plus this port", `host:port` is kept,
/// and every host must equal `own` (so a peer can only ever front its own
/// address and `AllowedIPs` can never overlap).
pub fn expand_backends(backends: &[String], own: Ipv4Addr) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(backends.len());
    for b in backends {
        let (host, port) = if let Some(port) = b.strip_prefix(':') {
            let port: u16 = port
                .parse()
                .map_err(|_| format!("backend {b:?} has an invalid port"))?;
            (own, port)
        } else {
            let sa: SocketAddr = b
                .parse()
                .map_err(|_| format!("backend {b:?} is not host:port or :port"))?;
            match sa.ip() {
                IpAddr::V4(ip) => (ip, sa.port()),
                IpAddr::V6(_) => return Err(format!("backend {b:?}: IPv6 is not supported yet")),
            }
        };
        if host != own {
            return Err(format!(
                "backend {b:?} is not on this registrant's tunnel address {own}: backends must \
                 use the registrant's own address (or the :port shorthand)"
            ));
        }
        out.push(format!("{host}:{port}"));
    }
    Ok(out)
}

/// Parses `0`, or a number with a unit: `30s`, `10m`, `12h`, `14d`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("{s:?} is not a duration like 30s, 10m, 12h or 14d"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return Err(format!("{s:?} is not a duration like 30s, 10m, 12h or 14d")),
    };
    Ok(Duration::from_secs(secs))
}
```

Insert this at the very top of `crates/gsp-controller/src/addresses/api.rs` (above the test module):

```rust
//! `GET /tunnel/addresses` — the address table, with a staleness flag — plus
//! the pieces both registries share: [`claim_error_response`] (the one place
//! `ClaimError` becomes an HTTP status) and the daily stale warning.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use super::{is_stale, now_secs, AddressBook, ClaimError};

#[derive(Clone)]
pub struct AddressesState {
    book: Arc<AddressBook>,
    /// Same posture as the registries: `None` leaves the API open.
    auth_token: Option<Arc<str>>,
    stale_after: Duration,
}

impl AddressesState {
    pub fn new(book: Arc<AddressBook>, auth_token: Option<String>, stale_after: Duration) -> Self {
        AddressesState {
            book,
            auth_token: auth_token.map(Arc::from),
            stale_after,
        }
    }
}

pub fn router(state: AddressesState) -> Router {
    Router::new()
        .route("/tunnel/addresses", get(list))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
        .with_state(state)
}

/// Mirrors `crate::peers::api::require_bearer`, typed against this state.
async fn require_bearer(State(state): State<AddressesState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.auth_token.as_deref() else {
        return next.run(req).await;
    };
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if token == expected => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

/// `409` address held / owner has a different one, `422` invalid address or no
/// network configured, `503` network exhausted, `500` storage.
pub fn claim_error_response(e: &ClaimError) -> Response {
    let status = match e {
        ClaimError::Held { .. } | ClaimError::OwnerHasDifferent { .. } => StatusCode::CONFLICT,
        ClaimError::OutsideNetwork { .. } | ClaimError::NotHost(_) | ClaimError::NoNetwork => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        ClaimError::Exhausted { .. } => StatusCode::SERVICE_UNAVAILABLE,
        ClaimError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(ErrorBody {
            error: e.to_string(),
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct EntryOut {
    role: super::Role,
    name: String,
    address: String,
    first_seen: u64,
    last_seen: u64,
    stale: bool,
}

#[derive(Serialize)]
struct ListOut {
    network: Option<String>,
    allocated: usize,
    capacity: Option<u64>,
    entries: Vec<EntryOut>,
}

async fn list(State(state): State<AddressesState>) -> Response {
    let entries = match state.book.entries() {
        Ok(e) => e,
        Err(e) => return claim_error_response(&e),
    };
    let now = now_secs();
    let out = ListOut {
        network: state.book.network().map(|n| n.to_string()),
        allocated: entries.len(),
        capacity: state.book.network().map(|n| n.capacity()),
        entries: entries
            .into_iter()
            .map(|e| EntryOut {
                stale: is_stale(&e.assignment, now, state.stale_after),
                role: e.role,
                name: e.name,
                address: e.assignment.address.to_string(),
                first_seen: e.assignment.first_seen,
                last_seen: e.assignment.last_seen,
            })
            .collect(),
    };
    Json(out).into_response()
}

/// Logs one `WARN` naming up to ten stale owners; returns how many there are.
pub fn warn_stale(book: &AddressBook, stale_after: Duration, now: u64) -> usize {
    let Ok(entries) = book.entries() else {
        return 0;
    };
    let stale: Vec<String> = entries
        .iter()
        .filter(|e| is_stale(&e.assignment, now, stale_after))
        .map(|e| format!("{}/{} ({})", e.role, e.name, e.assignment.address))
        .collect();
    if !stale.is_empty() {
        tracing::warn!(
            count = stale.len(),
            owners = ?stale.iter().take(10).collect::<Vec<_>>(),
            "tunnel addresses not re-registered for over {:?}; DELETE the registration to free one",
            stale_after
        );
    }
    stale.len()
}

/// Logs the stale warning now and then every 24 h. Does nothing when
/// `stale_after` is zero.
pub async fn stale_warning_loop(book: Arc<AddressBook>, stale_after: Duration) {
    if stale_after.is_zero() {
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(24 * 3600));
    loop {
        tick.tick().await; // the first tick is immediate
        warn_stale(&book, stale_after, now_secs());
    }
}
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gsp-controller addresses 2>&1 | tail -8`
Expected: `test result: ok. 23 passed; 0 failed`.

- [x] **Step 5: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy -p gsp-controller --all-targets -- -D warnings
git add crates/gsp-controller/src/addresses.rs crates/gsp-controller/src/addresses crates/gsp-controller/src/lib.rs
git commit -m "feat(controller): address book for tunnel address authority

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Allocate on registration (both registries)

**Files:**
- Modify: `crates/gsp-controller/src/peers.rs`, `crates/gsp-controller/src/proxy_peers.rs`
- Modify: `crates/gsp-controller/src/peers/api.rs`, `crates/gsp-controller/src/proxy_peers/api.rs`

**Interfaces:**
- Consumes: Task 1's `AddressBook`, `Role`, `expand_backends`, `now_secs`, `claim_error_response`.
- Produces:
  - `PeerRegistration { …, tunnel_address: Option<String> }` and `ProxyRegistration { …, tunnel_address: Option<String> }` (both `#[serde(default, skip_serializing_if = "Option::is_none")]`), each with `fn requested_address(&self) -> Option<Ipv4Addr>`.
  - `PeersState::new(store: Arc<Store>, auth_token: Option<String>, book: Arc<AddressBook>)`; `ProxyPeersState::new(...)` likewise.
  - `POST /peers` / `POST /proxy-peers` response: `{"revision": u64, "tunnel_address": "10.60.0.1", "tunnel_network": "10.60.0.0/16"}` (`tunnel_network` omitted in pin-only mode).
  - Stored registrations and SSE events carry `tunnel_address`; stored `backends` are expanded `ip:port`.

- [x] **Step 1: Write the failing tests**

In `peers.rs` tests, replace `valid()` so it sets `tunnel_address: None`, and add:

```rust
    #[test]
    fn backends_accept_the_port_shorthand() {
        let mut reg = valid();
        reg.backends = vec![":25565".into(), "10.60.0.2:25566".into()];
        assert!(reg.validate().is_ok());
        reg.backends = vec![":notaport".into()];
        assert!(reg.validate().is_err());
    }

    #[test]
    fn a_malformed_tunnel_address_fails_validation() {
        let mut reg = valid();
        reg.tunnel_address = Some("not-an-ip".into());
        assert!(reg.validate().is_err());
        reg.tunnel_address = Some("10.60.0.9".into());
        assert!(reg.validate().is_ok());
        assert_eq!(reg.requested_address(), Some("10.60.0.9".parse().unwrap()));
    }

    #[test]
    fn tunnel_address_is_omitted_from_json_when_absent() {
        let json = serde_json::to_string(&valid()).unwrap();
        assert!(!json.contains("tunnel_address"));
    }

    #[test]
    fn a_registration_from_an_older_log_still_decodes() {
        // Written before this field existed.
        let old = r#"{"name":"home","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","endpoint":null,"backends":["10.60.0.2:1"]}"#;
        let reg: PeerRegistration = serde_json::from_str(old).unwrap();
        assert_eq!(reg.tunnel_address, None);
    }
```

In `proxy_peers.rs` tests, set `tunnel_address: None` in `valid()` and add:

```rust
    #[test]
    fn a_malformed_tunnel_address_fails_validation() {
        let mut reg = valid();
        reg.tunnel_address = Some("not-an-ip".into());
        assert!(reg.validate().is_err());
        reg.tunnel_address = Some("10.60.0.9".into());
        assert!(reg.validate().is_ok());
        assert_eq!(reg.requested_address(), Some("10.60.0.9".parse().unwrap()));
    }

    #[test]
    fn tunnel_address_is_omitted_from_json_when_absent() {
        let json = serde_json::to_string(&valid()).unwrap();
        assert!(!json.contains("tunnel_address"));
    }

    #[test]
    fn a_registration_from_an_older_log_still_decodes() {
        // Written before this field existed.
        let old = r#"{"name":"edge-1","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","endpoint":"203.0.113.9:51820"}"#;
        let reg: ProxyRegistration = serde_json::from_str(old).unwrap();
        assert_eq!(reg.tunnel_address, None);
    }
```

In `peers/api.rs` tests: change the helpers and existing tests as follows, then add the new tests.

```rust
    use crate::addresses::{AddressBook, Network, Role};

    fn test_state() -> (PeersState, Arc<AddressBook>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("peers")).unwrap());
        let book = Arc::new(
            AddressBook::open(
                &dir.path().join("addresses"),
                Some(Network::parse("10.60.0.0/16").unwrap()),
            )
            .unwrap(),
        );
        (PeersState::new(store, None, book.clone()), book, dir)
    }

    async fn post(app: &Router, body: String) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(Request::post("/peers").body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn body_with(name: &str, backends: &[&str], tunnel_address: Option<&str>) -> String {
        let mut v = serde_json::json!({
            "name": name,
            "pubkey": KEY,
            "endpoint": "203.0.113.7:51820",
            "backends": backends,
        });
        if let Some(a) = tunnel_address {
            v["tunnel_address"] = serde_json::json!(a);
        }
        v.to_string()
    }
```

Update every existing test: `let (state, _dir) = test_state();` → `let (state, _book, _dir) = test_state();`; the two tests that build `PeersState::new(store, Some("secret".into()))` / `Store::open(dir.path())` directly must create a book the same way (`PeersState::new(store, Some("secret".into()), book)`); backends such as `"10.60.0.2:25565"` become the shorthand `":25565"`; in `a_second_registration_replaces_the_current_view_for_that_name` the expected value becomes `vec!["10.60.0.1:2"]` (the first allocation is `10.60.0.1`); and the two `PeerRegistration { … }` literals in the subscribe test gain `tunnel_address: None`.

Add the new tests:

```rust
    #[tokio::test]
    async fn a_registration_without_an_address_is_allocated_one_and_told_the_network() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, body) = post(&app, body_with("home", &[":25565"], None)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tunnel_address"], "10.60.0.1");
        assert_eq!(body["tunnel_network"], "10.60.0.0/16");
        assert!(body["revision"].as_u64().is_some());
    }

    #[tokio::test]
    async fn re_registering_returns_the_same_address_and_two_origins_get_distinct_ones() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (_, first) = post(&app, body_with("a", &[], None)).await;
        let (_, again) = post(&app, body_with("a", &[], None)).await;
        let (_, other) = post(&app, body_with("b", &[], None)).await;
        assert_eq!(first["tunnel_address"], again["tunnel_address"]);
        assert_ne!(first["tunnel_address"], other["tunnel_address"]);
    }

    #[tokio::test]
    async fn a_pinned_address_held_by_a_proxy_is_a_409_naming_the_holder() {
        let (state, book, _dir) = test_state();
        book.claim(Role::Proxy, "edge-1", Some("10.60.0.9".parse().unwrap()), 1)
            .unwrap();
        let app = router(state);
        let (status, body) = post(&app, body_with("home", &[], Some("10.60.0.9"))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body["error"].as_str().unwrap().contains("proxy \"edge-1\""));
    }

    #[tokio::test]
    async fn a_pin_outside_the_network_is_a_422() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, _) = post(&app, body_with("home", &[], Some("192.168.1.5"))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn the_port_shorthand_is_expanded_in_the_stored_registration() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        post(&app, body_with("home", &[":25565"], None)).await;
        let resp = app
            .oneshot(Request::get("/peers/home").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let reg: PeerRegistration = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reg.tunnel_address.as_deref(), Some("10.60.0.1"));
        assert_eq!(reg.backends, vec!["10.60.0.1:25565"]);
    }

    #[tokio::test]
    async fn a_backend_on_another_host_is_a_422() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, body) = post(&app, body_with("home", &["10.60.0.77:25565"], None)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body["error"].as_str().unwrap().contains("own address"));
    }

    #[tokio::test]
    async fn a_rejected_registration_keeps_the_address_for_the_retry() {
        // Review Focus 1: the claim precedes the backend check, so the owner
        // already holds an address — the corrected retry must get the same one.
        let (state, book, _dir) = test_state();
        let app = router(state);
        let (bad, _) = post(&app, body_with("home", &["10.60.0.77:1"], None)).await;
        assert_eq!(bad, StatusCode::UNPROCESSABLE_ENTITY);
        let held = book.get(Role::Origin, "home").unwrap().unwrap().address;
        let (ok, body) = post(&app, body_with("home", &[":1"], None)).await;
        assert_eq!(ok, StatusCode::OK);
        assert_eq!(body["tunnel_address"], held.to_string());
    }
```

In `proxy_peers/api.rs` tests, change `test_state()` and add the helpers and tests (the file already has `KEY` and `reg_body(name, endpoint)`):

```rust
    use crate::addresses::{AddressBook, Network, Role};

    fn test_state() -> (ProxyPeersState, Arc<AddressBook>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("proxy-peers")).unwrap());
        let book = Arc::new(
            AddressBook::open(
                &dir.path().join("addresses"),
                Some(Network::parse("10.60.0.0/16").unwrap()),
            )
            .unwrap(),
        );
        (ProxyPeersState::new(store, None, book.clone()), book, dir)
    }

    async fn post(app: &Router, body: String) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(Request::post("/proxy-peers").body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn body_with(name: &str, tunnel_address: Option<&str>) -> String {
        let mut v = serde_json::json!({
            "name": name,
            "pubkey": KEY,
            "endpoint": "203.0.113.9:51820",
        });
        if let Some(a) = tunnel_address {
            v["tunnel_address"] = serde_json::json!(a);
        }
        v.to_string()
    }

    #[tokio::test]
    async fn a_registration_without_an_address_is_allocated_one_and_told_the_network() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, body) = post(&app, body_with("edge-1", None)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tunnel_address"], "10.60.0.1");
        assert_eq!(body["tunnel_network"], "10.60.0.0/16");
    }

    #[tokio::test]
    async fn re_registering_returns_the_same_address_and_two_proxies_get_distinct_ones() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (_, first) = post(&app, body_with("a", None)).await;
        let (_, again) = post(&app, body_with("a", None)).await;
        let (_, other) = post(&app, body_with("b", None)).await;
        assert_eq!(first["tunnel_address"], again["tunnel_address"]);
        assert_ne!(first["tunnel_address"], other["tunnel_address"]);
    }

    #[tokio::test]
    async fn a_pinned_address_held_by_an_origin_is_a_409_naming_the_holder() {
        let (state, book, _dir) = test_state();
        book.claim(Role::Origin, "home", Some("10.60.0.9".parse().unwrap()), 1)
            .unwrap();
        let app = router(state);
        let (status, body) = post(&app, body_with("edge-1", Some("10.60.0.9"))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body["error"].as_str().unwrap().contains("origin \"home\""));
    }

    #[tokio::test]
    async fn a_pin_outside_the_network_is_a_422() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, _) = post(&app, body_with("edge-1", Some("192.168.1.5"))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn the_stored_registration_carries_the_allocated_address() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        post(&app, body_with("edge-1", None)).await;
        let resp = app
            .oneshot(Request::get("/proxy-peers/edge-1").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let reg: ProxyRegistration = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reg.tunnel_address.as_deref(), Some("10.60.0.1"));
    }
```

Update every existing proxy test: `let (state, _dir) = test_state();` → `let (state, _book, _dir) = test_state();`; the test building `ProxyPeersState::new(store, Some("secret".into()))` must open its own `Store` under `dir.path().join("proxy-peers")` and an `AddressBook` as above and pass the book; the two `ProxyRegistration { … }` literals in the subscribe test gain `tunnel_address: None`.

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gsp-controller peers 2>&1 | tail -25`
Expected: compile errors — ``no field `tunnel_address` on type `PeerRegistration` `` and ``this function takes 2 arguments but 3 arguments were supplied``.

- [x] **Step 3: Implement the registration types**

`peers.rs`: add the field and helper, and relax the backend check.

```rust
use std::net::Ipv4Addr;
```

```rust
    /// Backend addresses this origin fronts, reachable once its WireGuard peer
    /// is up. Entries are `host:port`, or the shorthand `:port` ("my tunnel
    /// address plus this port"), which the controller expands when it stores
    /// the registration (spec: Backends).
    #[serde(default)]
    pub backends: Vec<String>,
    /// This origin's tunnel-internal IPv4 address. Optional on request (omit to
    /// be allocated one, or give one to claim it); always set once stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_address: Option<String>,
```

```rust
    /// The address this registration asks for, if it names one.
    pub fn requested_address(&self) -> Option<Ipv4Addr> {
        self.tunnel_address.as_deref().and_then(|a| a.parse().ok())
    }
```

Replace the backends loop in `validate()` with:

```rust
        for b in &self.backends {
            let ok = match b.strip_prefix(':') {
                Some(port) => port.parse::<u16>().is_ok(),
                None => b.parse::<std::net::SocketAddr>().is_ok(),
            };
            if !ok {
                return Err(format!("backends entry {b:?} is not host:port or :port"));
            }
        }
        if let Some(a) = &self.tunnel_address {
            if a.parse::<Ipv4Addr>().is_err() {
                return Err(format!("tunnel_address {a:?} is not an IPv4 address"));
            }
        }
```

`proxy_peers.rs`: add the same `tunnel_address` field, the same `requested_address()` and the same `tunnel_address` check in `validate()` (there is no backends loop).

- [x] **Step 4: Implement the claim in `peers/api.rs`**

Imports: add `use crate::addresses::api::claim_error_response; use crate::addresses::{expand_backends, now_secs, AddressBook, Role};`.

State: add `book: Arc<AddressBook>` to `PeersState` and change `new`:

```rust
    pub fn new(store: Arc<Store>, auth_token: Option<String>, book: Arc<AddressBook>) -> Self {
        let (updates, _rx) = broadcast::channel(UPDATES_CAPACITY);
        let current = store
            .db()
            .open_tree("current")
            .expect("opening the peers current tree");
        PeersState {
            store,
            current,
            updates,
            auth_token: auth_token.map(Arc::from),
            book,
        }
    }
```

Response type:

```rust
#[derive(Serialize)]
struct SubmitResponse {
    revision: u64,
    tunnel_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tunnel_network: Option<String>,
}
```

Handler (replace the body after the existing `validate()` check):

```rust
async fn register(State(state): State<PeersState>, body: String) -> Response {
    let mut reg: PeerRegistration = match serde_json::from_str(&body) {
        Ok(reg) => reg,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorResponse {
                    error: format!("malformed peer registration: {e}"),
                }),
            )
                .into_response()
        }
    };
    if let Err(e) = reg.validate() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse { error: e }),
        )
            .into_response();
    }

    let assignment = match state
        .book
        .claim(Role::Origin, &reg.name, reg.requested_address(), now_secs())
    {
        Ok(a) => a,
        Err(e) => return claim_error_response(&e),
    };
    // Note: the claim above is kept even if the backends below are rejected —
    // the owner's corrected retry gets the same address (Review Focus 1).
    reg.backends = match expand_backends(&reg.backends, assignment.address) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorResponse { error: e }),
            )
                .into_response()
        }
    };
    reg.tunnel_address = Some(assignment.address.to_string());

    match state.register(&reg) {
        Ok(revision) => {
            tracing::info!(
                revision,
                name = %reg.name,
                address = %assignment.address,
                "registered a backend peer"
            );
            (
                StatusCode::OK,
                Json(SubmitResponse {
                    revision,
                    tunnel_address: assignment.address.to_string(),
                    tunnel_network: state.book.network().map(|n| n.to_string()),
                }),
            )
                .into_response()
        }
        Err(e) => store_error_response(e),
    }
}
```

In `proxy_peers/api.rs` add the same imports (without `expand_backends`), the same `book` field with `new(store, auth_token, book)`, the same `SubmitResponse`, and this handler:

```rust
async fn register(State(state): State<ProxyPeersState>, body: String) -> Response {
    let mut reg: ProxyRegistration = match serde_json::from_str(&body) {
        Ok(reg) => reg,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorResponse {
                    error: format!("malformed proxy registration: {e}"),
                }),
            )
                .into_response()
        }
    };
    if let Err(e) = reg.validate() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse { error: e }),
        )
            .into_response();
    }

    let assignment = match state
        .book
        .claim(Role::Proxy, &reg.name, reg.requested_address(), now_secs())
    {
        Ok(a) => a,
        Err(e) => return claim_error_response(&e),
    };
    reg.tunnel_address = Some(assignment.address.to_string());

    match state.register(&reg) {
        Ok(revision) => {
            tracing::info!(
                revision,
                name = %reg.name,
                address = %assignment.address,
                "registered a proxy peer"
            );
            (
                StatusCode::OK,
                Json(SubmitResponse {
                    revision,
                    tunnel_address: assignment.address.to_string(),
                    tunnel_network: state.book.network().map(|n| n.to_string()),
                }),
            )
                .into_response()
        }
        Err(e) => store_error_response(e),
    }
}
```

- [x] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p gsp-controller 2>&1 | tail -6`
Expected: `test result: ok.` with no failures (the controller binary will not compile until Task 3 — if `cargo test -p gsp-controller` fails to build `main.rs` because `PeersState::new` gained an argument, do the minimal fix there now: open an `AddressBook` at `args.data_dir.join("tunnel-addresses")` with `None` network and pass `book.clone()`; Task 3 replaces it with the real wiring).

- [x] **Step 6: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
git add -A crates/gsp-controller
git commit -m "feat(controller): allocate tunnel addresses on peer and proxy registration

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Controller flags, HA guard and wiring

**Files:**
- Modify: `crates/gsp-controller/src/addresses.rs` (add `resolve_flags`)
- Modify: `crates/gsp-controller/src/main.rs`
- Modify: `crates/gsp-fleet-tests/src/lib.rs` (add `spawn_controller_with`)
- Create: `crates/gsp-fleet-tests/tests/tunnel_addresses.rs`

**Interfaces:**
- Consumes: Tasks 1–2.
- Produces:
  - `addresses::resolve_flags(network: Option<&str>, stale_after: &str, ha_enabled: bool) -> Result<(Option<Network>, Duration), String>`
  - `gsp_fleet_tests::spawn_controller_with(data_dir: &Path, listen: &str, extra: &[String]) -> Result<Proc>` (`spawn_controller_on` delegates to it with `&[]`)
  - controller flags `--tunnel-network <CIDR>` and `--tunnel-stale-after <dur>` (default `14d`)

- [x] **Step 1: Write the failing tests**

Append to the `tests` module in `addresses.rs`:

```rust
    #[test]
    fn resolve_flags_defaults_to_pin_only_and_fourteen_days() {
        let (net, stale) = resolve_flags(None, "14d", false).unwrap();
        assert!(net.is_none());
        assert_eq!(stale, Duration::from_secs(14 * 86400));
    }

    #[test]
    fn resolve_flags_parses_the_network_and_the_duration() {
        let (net, stale) = resolve_flags(Some("10.60.0.0/16"), "0", false).unwrap();
        assert_eq!(net.unwrap().to_string(), "10.60.0.0/16");
        assert!(stale.is_zero());
    }

    #[test]
    fn resolve_flags_rejects_a_bad_network_a_bad_duration_and_ha() {
        assert!(resolve_flags(Some("nonsense"), "14d", false).is_err());
        assert!(resolve_flags(None, "soon", false).is_err());
        let err = resolve_flags(Some("10.60.0.0/16"), "14d", true).unwrap_err();
        assert!(err.contains("--ha-peers"), "{err}");
        // Pin-only mode (no network) with HA is allowed: nothing is allocated.
        assert!(resolve_flags(None, "14d", true).is_ok());
    }
```

Create `crates/gsp-fleet-tests/tests/tunnel_addresses.rs`:

```rust
//! The controller's address-authority surface over real HTTP (spec: Controller).
//! Plain `cargo test`: no namespaces needed.

use std::time::Duration;

use anyhow::Result;
use gsp_fleet_tests::{build_fleet_bins, free_port, spawn_controller_with, wait_http_up, Proc};
use serde_json::{json, Value};

const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

async fn controller(extra: &[&str]) -> Result<(Proc, tempfile::TempDir, String)> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let port = free_port()?;
    let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    let ctl = spawn_controller_with(dir.path(), &format!("127.0.0.1:{port}"), &extra)?;
    wait_http_up(
        &format!("http://127.0.0.1:{port}/healthz"),
        Duration::from_secs(10),
    )
    .await?;
    Ok((ctl, dir, format!("http://127.0.0.1:{port}")))
}

#[tokio::test]
async fn the_controller_allocates_refuses_conflicts_and_lists_the_table() -> Result<()> {
    let (_ctl, _dir, base) = controller(&["--tunnel-network", "10.60.0.0/16"]).await?;
    let http = reqwest::Client::new();

    let r: Value = http
        .post(format!("{base}/peers"))
        .json(&json!({"name": "o1", "pubkey": KEY, "backends": [":25565"]}))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(r["tunnel_address"], "10.60.0.1");
    assert_eq!(r["tunnel_network"], "10.60.0.0/16");

    let conflict = http
        .post(format!("{base}/proxy-peers"))
        .json(&json!({"name": "p1", "pubkey": KEY, "endpoint": "203.0.113.9:51820",
                      "tunnel_address": "10.60.0.1"}))
        .send()
        .await?;
    assert_eq!(conflict.status(), 409);
    let body: Value = conflict.json().await?;
    assert!(body["error"].as_str().unwrap().contains("origin \"o1\""));

    let reg: Value = http.get(format!("{base}/peers/o1")).send().await?.json().await?;
    assert_eq!(reg["backends"][0], "10.60.0.1:25565");

    let table: Value = http
        .get(format!("{base}/tunnel/addresses"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(table["allocated"], 1);
    assert_eq!(table["entries"][0]["name"], "o1");
    Ok(())
}

#[tokio::test]
async fn tunnel_network_together_with_ha_is_refused_at_startup() -> Result<()> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let port = free_port()?;
    let mut ctl = spawn_controller_with(
        dir.path(),
        &format!("127.0.0.1:{port}"),
        &[
            "--tunnel-network".into(),
            "10.60.0.0/16".into(),
            "--ha-node-id".into(),
            "1".into(),
            "--ha-peers".into(),
            "1=127.0.0.1:1".into(),
        ],
    )?;
    gsp_fleet_tests::wait_until(
        || {
            let code = ctl.exit_code();
            async move { Ok(code.is_some_and(|c| c != 0)) }
        },
        Duration::from_secs(10),
        "the controller to exit non-zero",
    )
    .await?;
    assert!(
        ctl.log().contains("--tunnel-network"),
        "the error should name the flag, got: {}",
        ctl.log()
    );
    Ok(())
}
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gsp-controller resolve_flags 2>&1 | tail -5` → ``cannot find function `resolve_flags` ``.
Run: `cargo test -p gsp-fleet-tests --test tunnel_addresses 2>&1 | tail -5` → ``unresolved import `gsp_fleet_tests::spawn_controller_with` ``.

- [x] **Step 3: Implement `resolve_flags`**

Add to `addresses.rs` (above the tests module):

```rust
/// Validates the controller's `--tunnel-*` flags together: parses the network
/// and the stale threshold, and refuses a network combined with HA (the
/// allocator is correct only with a single writer — see the module doc).
pub fn resolve_flags(
    network: Option<&str>,
    stale_after: &str,
    ha_enabled: bool,
) -> Result<(Option<Network>, Duration), String> {
    let stale = parse_duration(stale_after)
        .map_err(|e| format!("--tunnel-stale-after: {e}"))?;
    let net = match network {
        Some(n) => {
            if ha_enabled {
                return Err(
                    "--tunnel-network cannot be combined with --ha-peers: the address allocator \
                     needs a single writer and the registries are not replicated \
                     (see docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md)"
                        .into(),
                );
            }
            Some(Network::parse(n).map_err(|e| format!("--tunnel-network: {e}"))?)
        }
        None => None,
    };
    Ok((net, stale))
}
```

- [x] **Step 4: Implement the harness helper and the controller wiring**

`crates/gsp-fleet-tests/src/lib.rs` — replace `spawn_controller_on` with:

```rust
pub fn spawn_controller_on(data_dir: &Path, listen: &str) -> Result<Proc> {
    spawn_controller_with(data_dir, listen, &[])
}

/// Like [`spawn_controller_on`] with extra command-line arguments.
pub fn spawn_controller_with(data_dir: &Path, listen: &str, extra: &[String]) -> Result<Proc> {
    let mut args = vec![
        "--data-dir".to_string(),
        data_dir.display().to_string(),
        "--listen".to_string(),
        listen.to_string(),
    ];
    args.extend(extra.iter().cloned());
    Proc::spawn_in(None, "gsp-controller", &args)
}
```

`crates/gsp-controller/src/main.rs`:

Add to `Args` (after `ha_token`):

```rust
    /// IPv4 network tunnel addresses are allocated from, e.g. `10.60.0.0/16`.
    /// Omit for pin-only mode: requested addresses are checked for uniqueness
    /// but nothing is allocated. Cannot be combined with `--ha-peers`.
    #[arg(long)]
    tunnel_network: Option<String>,

    /// An allocated tunnel address not re-registered for this long is flagged
    /// `stale` in `GET /tunnel/addresses` and in a daily log warning. Units:
    /// s, m, h, d. `0` disables it.
    #[arg(long, default_value = "14d")]
    tunnel_stale_after: String,
```

After the existing `--ha-peers` / `--role slave` checks in `main()` (before `tracing_subscriber`), add:

```rust
    let (tunnel_network, stale_after) = gsp_controller::addresses::resolve_flags(
        args.tunnel_network.as_deref(),
        &args.tunnel_stale_after,
        !args.ha_peers.is_empty(),
    )
    .map_err(|e| anyhow::anyhow!(e))?;
```

Replace the two registry-state constructions and the router merge. After the `peers_dir` opening add the book:

```rust
    // The address book (spec: Controller/State) — its own sled database like
    // every other registry, shared by both peer registries below.
    let addresses_dir = args.data_dir.join("tunnel-addresses");
    let book = Arc::new(
        gsp_controller::addresses::AddressBook::open(&addresses_dir, tunnel_network)
            .map_err(|e| anyhow::anyhow!("opening the address book at {addresses_dir:?}: {e}"))?,
    );
    match tunnel_network {
        Some(n) => tracing::info!(network = %n, "tunnel address allocation enabled"),
        None => tracing::info!("no --tunnel-network: pin-only tunnel addresses (no allocation)"),
    }
```

```rust
    let peers_state = PeersState::new(peers_store, args.auth_token.clone(), book.clone());
```

```rust
    let proxy_peers_state =
        ProxyPeersState::new(proxy_peers_store, args.auth_token.clone(), book.clone());
```

Directly after the `proxy_peers_state` line (so it runs before `args.auth_token` is moved into `adopt_state` further down):

```rust
    let addresses_state = gsp_controller::addresses::api::AddressesState::new(
        book.clone(),
        args.auth_token.clone(),
        stale_after,
    );
    tokio::spawn(gsp_controller::addresses::api::stale_warning_loop(
        book.clone(),
        stale_after,
    ));
```

and add `.merge(gsp_controller::addresses::api::router(addresses_state))` after the `proxy_peers` router merge in `let mut app = Router::new()…`.

- [x] **Step 5: Run the tests to verify they pass**

```bash
cargo test -p gsp-controller resolve_flags 2>&1 | tail -4
cargo test -p gsp-fleet-tests --test tunnel_addresses 2>&1 | tail -6
```
Expected: `resolve_flags` 3 passed; the fleet test file `2 passed`.

- [x] **Step 6: Whole-task verification and commit**

```bash
cargo fmt --all && make check
git add -A crates
git commit -m "feat(controller): --tunnel-network, --tunnel-stale-after and the address table endpoint

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```
Expected: `make check` exits 0.

---
### Task 4: Release and tombstones (both registries)

**Files:**
- Modify: `crates/gsp-controller/src/peers.rs`, `crates/gsp-controller/src/peers/api.rs`
- Modify: `crates/gsp-controller/src/proxy_peers/api.rs`

**Interfaces:**
- Consumes: Task 1's `AddressBook::{get, release}`, Task 2's states.
- Produces:
  - `peers::tombstone_bytes(name: &str) -> Vec<u8>` and `peers::event_payload(revision: u64, bytes: &[u8]) -> serde_json::Value`, both `pub(crate)`, reused by `proxy_peers`.
  - `DELETE /peers/{name}` and `DELETE /proxy-peers/{name}` → `200 {"revision": N, "released": "10.60.0.1" | null}`, `404` if the name is unknown everywhere.
  - Subscribe event payload for a deletion: `{"revision":N,"removed":{"name":"…"}}`; for a registration unchanged: `{"revision":N,"registration":{…}}`.

- [x] **Step 1: Write the failing tests**

In `peers.rs` tests:

```rust
    #[test]
    fn event_payload_distinguishes_a_registration_from_a_tombstone() {
        let reg = serde_json::to_vec(&valid()).unwrap();
        let p = event_payload(3, &reg);
        assert_eq!(p["revision"], 3);
        assert_eq!(p["registration"]["name"], "home-origin");
        assert!(p.get("removed").is_none());

        let p = event_payload(4, &tombstone_bytes("home-origin"));
        assert_eq!(p["revision"], 4);
        assert_eq!(p["removed"]["name"], "home-origin");
        assert!(p.get("registration").is_none());
    }
```

In `peers/api.rs` tests (uses the helpers from Task 2):

```rust
    async fn delete(app: &Router, name: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::delete(format!("/peers/{name}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn delete_frees_the_address_and_removes_the_current_registration() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (_, a) = post(&app, body_with("a", &[], None)).await;
        assert_eq!(a["tunnel_address"], "10.60.0.1");

        let (status, body) = delete(&app, "a").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["released"], "10.60.0.1");

        let resp = app
            .clone()
            .oneshot(Request::get("/peers/a").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // The freed address is the lowest free one again.
        let (_, b) = post(&app, body_with("b", &[], None)).await;
        assert_eq!(b["tunnel_address"], "10.60.0.1");
    }

    #[tokio::test]
    async fn delete_of_an_unknown_name_is_404() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, _) = delete(&app, "nobody").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_retried_delete_after_a_half_finished_one_still_completes() {
        // The address is held but there is no current registration (a crash
        // between the tombstone and the release): the retry must succeed.
        let (state, book, _dir) = test_state();
        book.claim(Role::Origin, "ghost", None, 1).unwrap();
        let app = router(state);
        let (status, body) = delete(&app, "ghost").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["released"], "10.60.0.1");
        assert!(book.get(Role::Origin, "ghost").unwrap().is_none());
    }

    #[tokio::test]
    async fn re_registering_after_a_delete_is_allocated_afresh_not_sticky() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        post(&app, body_with("a", &[], None)).await; // .1
        post(&app, body_with("b", &[], None)).await; // .2
        delete(&app, "a").await;
        let (_, c) = post(&app, body_with("c", &[], None)).await;
        assert_eq!(c["tunnel_address"], "10.60.0.1", "c takes the freed address");
        let (_, a) = post(&app, body_with("a", &[], None)).await;
        assert_eq!(a["tunnel_address"], "10.60.0.3", "a no longer owns .1");
    }

    #[tokio::test]
    async fn delete_logs_a_tombstone_a_catch_up_subscriber_receives() {
        let (state, _book, _dir) = test_state();
        let app = router(state.clone());
        post(&app, body_with("home", &[":1"], None)).await;
        delete(&app, "home").await;

        let (tx, mut rx) = mpsc::channel(8);
        let updates = state.updates.subscribe();
        tokio::spawn(subscribe_worker(state.store.clone(), updates, 0, tx));
        let (r1, b1) = rx.recv().await.unwrap();
        let (r2, b2) = rx.recv().await.unwrap();
        assert!(r2 > r1);
        assert_eq!(
            crate::peers::event_payload(r1, &b1)["registration"]["name"],
            "home"
        );
        assert_eq!(
            crate::peers::event_payload(r2, &b2)["removed"]["name"],
            "home"
        );
    }

    #[tokio::test]
    async fn delete_requires_the_bearer_token_when_one_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("peers")).unwrap());
        let book = Arc::new(
            AddressBook::open(
                &dir.path().join("addresses"),
                Some(Network::parse("10.60.0.0/16").unwrap()),
            )
            .unwrap(),
        );
        let app = router(PeersState::new(store, Some("secret".into()), book));
        let resp = app
            .oneshot(Request::delete("/peers/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
```

In `proxy_peers/api.rs` tests (uses the helpers from Task 2):

```rust
    async fn delete(app: &Router, name: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::delete(format!("/proxy-peers/{name}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn delete_frees_the_address_and_removes_the_current_registration() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (_, a) = post(&app, body_with("a", None)).await;
        assert_eq!(a["tunnel_address"], "10.60.0.1");

        let (status, body) = delete(&app, "a").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["released"], "10.60.0.1");

        let resp = app
            .clone()
            .oneshot(Request::get("/proxy-peers/a").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let (_, b) = post(&app, body_with("b", None)).await;
        assert_eq!(b["tunnel_address"], "10.60.0.1");
    }

    #[tokio::test]
    async fn delete_of_an_unknown_name_is_404() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, _) = delete(&app, "nobody").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_logs_a_tombstone_a_catch_up_subscriber_receives() {
        let (state, _book, _dir) = test_state();
        let app = router(state.clone());
        post(&app, body_with("edge-1", None)).await;
        delete(&app, "edge-1").await;

        let (tx, mut rx) = mpsc::channel(8);
        let updates = state.updates.subscribe();
        tokio::spawn(subscribe_worker(state.store.clone(), updates, 0, tx));
        let (r1, b1) = rx.recv().await.unwrap();
        let (r2, b2) = rx.recv().await.unwrap();
        assert!(r2 > r1);
        assert_eq!(
            crate::peers::event_payload(r1, &b1)["registration"]["name"],
            "edge-1"
        );
        assert_eq!(
            crate::peers::event_payload(r2, &b2)["removed"]["name"],
            "edge-1"
        );
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gsp-controller 2>&1 | tail -15`
Expected: ``cannot find function `event_payload` `` / ``cannot find function `tombstone_bytes` `` and the delete tests failing to compile or returning `405 Method Not Allowed`.

- [x] **Step 3: Implement**

`peers.rs` (above `#[cfg(test)]`):

```rust
/// Log payload for a deleted registration. Deliberately not a
/// [`PeerRegistration`]: `current` never points at a tombstone, so only
/// subscribers (via [`event_payload`]) ever read one.
pub(crate) fn tombstone_bytes(name: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "removed": name }))
        .expect("a json! object always serializes")
}

/// The SSE `data:` payload for one log entry:
/// `{"revision":N,"registration":{…}}` for a registration,
/// `{"revision":N,"removed":{"name":"…"}}` for a tombstone.
pub(crate) fn event_payload(revision: u64, bytes: &[u8]) -> serde_json::Value {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null);
    match value.get("removed").and_then(|v| v.as_str()) {
        Some(name) => serde_json::json!({ "revision": revision, "removed": { "name": name } }),
        None => serde_json::json!({ "revision": revision, "registration": value }),
    }
}
```

`peers/api.rs`: add to `impl PeersState`:

```rust
    /// Logs a tombstone for `name` and drops it from `current`. The tombstone
    /// goes first: a crash between the two leaves a stale `current` entry that
    /// a retried `DELETE` removes — never a silently lost removal.
    fn remove(&self, name: &str) -> Result<u64, StoreError> {
        let revision = self.store.put(super::tombstone_bytes(name))?;
        self.current.remove(name.as_bytes())?;
        self.current.flush()?;
        let _ = self.updates.send(revision);
        Ok(revision)
    }
```

Routes: `.route("/peers/{name}", get(get_one).delete(delete_one))`.

Handler and response type:

```rust
#[derive(Serialize)]
struct DeleteResponse {
    revision: u64,
    released: Option<String>,
}

/// `DELETE /peers/{name}` — releases the origin's tunnel address and tells
/// subscribers (a tombstone) to drop its WireGuard peer. `404` only when the
/// name is unknown everywhere (no current registration *and* no address), so a
/// retry after a crash still completes.
async fn delete_one(State(state): State<PeersState>, Path(name): Path<String>) -> Response {
    let has_current = match state.current.contains_key(name.as_bytes()) {
        Ok(b) => b,
        Err(e) => return store_error_response(StoreError::from(e)),
    };
    let has_address = match state.book.get(Role::Origin, &name) {
        Ok(a) => a.is_some(),
        Err(e) => return claim_error_response(&e),
    };
    if !has_current && !has_address {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("no peer registered as {name:?}"),
            }),
        )
            .into_response();
    }
    let revision = match state.remove(&name) {
        Ok(r) => r,
        Err(e) => return store_error_response(e),
    };
    let released = match state.book.release(Role::Origin, &name) {
        Ok(a) => a,
        Err(e) => return claim_error_response(&e),
    };
    tracing::info!(revision, name = %name, address = ?released, "released a backend peer");
    (
        StatusCode::OK,
        Json(DeleteResponse {
            revision,
            released: released.map(|a| a.to_string()),
        }),
    )
        .into_response()
}
```

Replace the SSE mapping closure in `subscribe`:

```rust
    let events = ReceiverStream::new(rx).map(|(revision, bytes)| {
        Ok(Event::default().data(super::event_payload(revision, &bytes).to_string()))
    });
```

In `proxy_peers/api.rs` make the same three changes with these differences — `Role::Proxy`, the helpers imported as `use crate::peers::{event_payload, tombstone_bytes};`, route `.route("/proxy-peers/{name}", get(get_one).delete(delete_one))`:

```rust
    // in `impl ProxyPeersState`
    fn remove(&self, name: &str) -> Result<u64, StoreError> {
        let revision = self.store.put(tombstone_bytes(name))?;
        self.current.remove(name.as_bytes())?;
        self.current.flush()?;
        let _ = self.updates.send(revision);
        Ok(revision)
    }
```

```rust
#[derive(Serialize)]
struct DeleteResponse {
    revision: u64,
    released: Option<String>,
}

/// `DELETE /proxy-peers/{name}` — see `crate::peers::api::delete_one`.
async fn delete_one(State(state): State<ProxyPeersState>, Path(name): Path<String>) -> Response {
    let has_current = match state.current.contains_key(name.as_bytes()) {
        Ok(b) => b,
        Err(e) => return store_error_response(StoreError::from(e)),
    };
    let has_address = match state.book.get(Role::Proxy, &name) {
        Ok(a) => a.is_some(),
        Err(e) => return claim_error_response(&e),
    };
    if !has_current && !has_address {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("no proxy registered as {name:?}"),
            }),
        )
            .into_response();
    }
    let revision = match state.remove(&name) {
        Ok(r) => r,
        Err(e) => return store_error_response(e),
    };
    let released = match state.book.release(Role::Proxy, &name) {
        Ok(a) => a,
        Err(e) => return claim_error_response(&e),
    };
    tracing::info!(revision, name = %name, address = ?released, "released a proxy peer");
    (
        StatusCode::OK,
        Json(DeleteResponse {
            revision,
            released: released.map(|a| a.to_string()),
        }),
    )
        .into_response()
}
```

and in `subscribe` the same closure: `Ok(Event::default().data(event_payload(revision, &bytes).to_string()))`.

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gsp-controller 2>&1 | tail -6`
Expected: `test result: ok.`

- [x] **Step 5: Verify and commit**

```bash
cargo fmt --all && make check
git add -A crates/gsp-controller
git commit -m "feat(controller): DELETE a peer registration, free its address, log a tombstone

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 5: `gsp-agent` — register first, persist, `/32` proxy peers, tombstones

**Files:**
- Create: `crates/gsp-agent/src/address_store.rs`
- Modify: `crates/gsp-agent/src/register.rs`, `crates/gsp-agent/src/proxy_subscribe.rs`, `crates/gsp-agent/src/main.rs`

**Interfaces:**
- Consumes: Task 2/4 wire shapes.
- Produces (module-private to the binary):
  - `register::{Registration{name,pubkey,endpoint,backends,address:Option<String>}, Registered{revision,tunnel_address,tunnel_network}, RegisterError::{Rejected(String),Transient(anyhow::Error)}, register_once, register_with_retry, address_change, run}`
  - `address_store::{ip_of, interface_cidr, load, save, StartupAddress{cidr,source}, Source::{Controller,Saved}, resolve_startup}`
  - `proxy_subscribe`: `Event::{Registered,Removed}`, `Action`, `plan`, `/32` peers.
  - CLI: `--address` optional; new `--peer-address` (required together with `--peer-pubkey`/`--peer-endpoint`); `--backends` accepts `:port`.

- [x] **Step 1: Write the failing tests**

Create `crates/gsp-agent/src/address_store.rs` with only this test module (and `use super::*;`-style access to the items Step 3 adds):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::register::{RegisterError, Registered};

    fn registered(ip: &str, net: Option<&str>) -> Registered {
        Registered {
            revision: 1,
            tunnel_address: ip.into(),
            tunnel_network: net.map(str::to_string),
        }
    }

    #[test]
    fn ip_of_strips_the_prefix() {
        assert_eq!(ip_of("10.60.0.5/16"), "10.60.0.5");
        assert_eq!(ip_of("10.60.0.5"), "10.60.0.5");
    }

    #[test]
    fn the_networks_prefix_wins_over_a_pinned_one() {
        assert_eq!(
            interface_cidr("10.60.0.5", Some("10.60.0.0/16"), Some("10.60.0.5/24")).unwrap(),
            "10.60.0.5/16"
        );
    }

    #[test]
    fn pin_only_mode_uses_the_pinned_prefix_and_errors_without_one() {
        assert_eq!(
            interface_cidr("10.60.0.5", None, Some("10.60.0.5/24")).unwrap(),
            "10.60.0.5/24"
        );
        assert!(interface_cidr("10.60.0.5", None, None).is_err());
    }

    #[test]
    fn a_saved_address_round_trips_and_blank_or_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tunnel-address");
        assert_eq!(load(&path), None);
        save(&path, "10.60.0.5/16").unwrap();
        assert_eq!(load(&path).as_deref(), Some("10.60.0.5/16"));
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(load(&path), None);
    }

    #[test]
    fn startup_uses_the_controllers_answer_when_it_registers() {
        let s = resolve_startup(
            Ok(registered("10.60.0.5", Some("10.60.0.0/16"))),
            None,
            Some("10.60.0.9/16".into()),
        )
        .unwrap();
        assert_eq!(s.cidr, "10.60.0.5/16");
        assert!(matches!(s.source, Source::Controller));
    }

    #[test]
    fn startup_falls_back_to_the_saved_address_when_the_controller_is_unreachable() {
        let s = resolve_startup(
            Err(RegisterError::Transient(anyhow::anyhow!("connection refused"))),
            None,
            Some("10.60.0.5/16".into()),
        )
        .unwrap();
        assert_eq!(s.cidr, "10.60.0.5/16");
        assert!(matches!(s.source, Source::Saved));
    }

    #[test]
    fn startup_fails_without_a_saved_address_when_the_controller_is_unreachable() {
        let err = resolve_startup(
            Err(RegisterError::Transient(anyhow::anyhow!("connection refused"))),
            None,
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no saved tunnel address"), "{err:#}");
    }

    #[test]
    fn startup_never_falls_back_when_the_controller_refuses_the_registration() {
        // A 409 (address held) must not be papered over by a stale saved value.
        let err = resolve_startup(
            Err(RegisterError::Rejected("already held by origin \"x\"".into())),
            None,
            Some("10.60.0.5/16".into()),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("already held"), "{err:#}");
    }
}
```

In `register.rs`, replace the whole file's tests module with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Registration {
        Registration {
            name: "home".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: None,
            backends: vec![":25565".into()],
            address: None,
        }
    }

    #[test]
    fn optional_fields_are_omitted_from_the_body_when_absent() {
        let r = reg();
        let json = serde_json::to_string(&body(&r)).unwrap();
        assert!(!json.contains("endpoint"));
        assert!(!json.contains("tunnel_address"));
        assert!(json.contains("\"backends\":[\":25565\"]"));
    }

    #[test]
    fn a_pinned_address_and_an_endpoint_are_serialized() {
        let mut r = reg();
        r.endpoint = Some("203.0.113.7:51820".into());
        r.address = Some("10.60.0.2".into());
        let json = serde_json::to_string(&body(&r)).unwrap();
        assert!(json.contains("\"endpoint\":\"203.0.113.7:51820\""));
        assert!(json.contains("\"tunnel_address\":\"10.60.0.2\""));
    }

    #[test]
    fn address_change_reports_only_a_real_difference() {
        assert_eq!(address_change("10.60.0.5", "10.60.0.5"), None);
        let msg = address_change("10.60.0.5", "10.60.0.9").unwrap();
        assert!(msg.contains("10.60.0.5") && msg.contains("10.60.0.9"));
        assert!(msg.contains("restart"));
    }

    /// A one-shot HTTP server answering every request with a canned response.
    async fn canned(status_line: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_successful_registration_parses_the_assigned_address_and_network() {
        let url = canned(
            "200 OK",
            r#"{"revision":4,"tunnel_address":"10.60.0.5","tunnel_network":"10.60.0.0/16"}"#,
        )
        .await;
        let got = register_once(&reqwest::Client::new(), &url, None, &reg())
            .await
            .unwrap();
        assert_eq!(got.tunnel_address, "10.60.0.5");
        assert_eq!(got.tunnel_network.as_deref(), Some("10.60.0.0/16"));
    }

    #[tokio::test]
    async fn a_4xx_is_a_permanent_rejection_carrying_the_controllers_message() {
        let url = canned("409 Conflict", r#"{"error":"address 10.60.0.2 is already held by origin \"x\""}"#).await;
        match register_once(&reqwest::Client::new(), &url, None, &reg()).await {
            Err(RegisterError::Rejected(m)) => assert!(m.contains("already held"), "{m}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_5xx_or_a_refused_connection_is_transient() {
        let url = canned("503 Service Unavailable", r#"{"error":"exhausted"}"#).await;
        assert!(matches!(
            register_once(&reqwest::Client::new(), &url, None, &reg()).await,
            Err(RegisterError::Transient(_))
        ));
        assert!(matches!(
            register_once(&reqwest::Client::new(), "http://127.0.0.1:1", None, &reg()).await,
            Err(RegisterError::Transient(_))
        ));
    }

    #[tokio::test]
    async fn retrying_gives_up_on_a_permanent_rejection_immediately() {
        let url = canned("409 Conflict", r#"{"error":"held"}"#).await;
        let started = std::time::Instant::now();
        let err = register_with_retry(
            &reqwest::Client::new(),
            &url,
            None,
            &reg(),
            Duration::from_secs(30),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RegisterError::Rejected(_)));
        assert!(started.elapsed() < Duration::from_secs(5), "must not retry a 409");
    }
}
```

In `proxy_subscribe.rs` tests, replace the `ProxyRegistration { … }` literals with a helper and add the routing/event tests:

```rust
    fn reg(addr: Option<&str>) -> ProxyRegistration {
        ProxyRegistration {
            name: "edge-1".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: "203.0.113.9:51820".into(),
            tunnel_address: addr.map(str::to_string),
        }
    }

    #[test]
    fn to_wg_peer_routes_only_the_proxys_tunnel_address() {
        let peer = to_wg_peer(&reg(Some("10.60.0.3"))).unwrap();
        assert_eq!(peer.allowed_ips.len(), 1);
        assert_eq!(peer.allowed_ips[0].cidr, 32, "a host route, not 0.0.0.0/0");
        assert_eq!(peer.allowed_ips[0].ip.to_string(), "10.60.0.3");
        assert_eq!(peer.persistent_keepalive_interval, Some(25));
        assert!(peer.endpoint.is_some());
    }

    #[test]
    fn to_wg_peer_rejects_a_proxy_without_a_tunnel_address() {
        assert!(to_wg_peer(&reg(None)).is_err());
    }

    #[test]
    fn two_proxies_get_non_overlapping_routes() {
        // The known bug: both used to be 0.0.0.0/0, so the last one won.
        let a = to_wg_peer(&reg(Some("10.60.0.3"))).unwrap();
        let b = to_wg_peer(&reg(Some("10.60.0.4"))).unwrap();
        assert_ne!(a.allowed_ips[0].ip, b.allowed_ips[0].ip);
    }

    #[test]
    fn parses_a_registration_and_a_tombstone_event() {
        let event = r#"data: {"revision":1,"registration":{"name":"edge-1","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","endpoint":"203.0.113.9:51820","tunnel_address":"10.60.0.3"}}"#;
        match parse_sse_event(event).unwrap() {
            Event::Registered(r) => assert_eq!(r.tunnel_address.as_deref(), Some("10.60.0.3")),
            other => panic!("{other:?}"),
        }
        let gone = r#"data: {"revision":2,"removed":{"name":"edge-1"}}"#;
        assert_eq!(parse_sse_event(gone), Some(Event::Removed("edge-1".into())));
    }

    #[test]
    fn plan_skips_unchanged_reconciles_changed_and_removes_known() {
        let mut applied = HashMap::new();
        let r = reg(Some("10.60.0.3"));
        assert_eq!(plan(&applied, &Event::Registered(r.clone())), Action::Reconcile(&r));
        applied.insert(r.name.clone(), r.clone());
        assert_eq!(plan(&applied, &Event::Registered(r.clone())), Action::Skip);
        assert_eq!(
            plan(&applied, &Event::Removed("edge-1".into())),
            Action::Remove(r.pubkey.clone())
        );
        assert_eq!(plan(&applied, &Event::Removed("other".into())), Action::Skip);
    }

    #[test]
    fn registered_then_removed_leaves_nothing() {
        // Review Focus 3: a catch-up from revision 0 replays the add and then
        // the removal; applying both in order must end with no peer tracked.
        let mut applied = HashMap::new();
        let r = reg(Some("10.60.0.3"));
        let add = Event::Registered(r.clone());
        if let Action::Reconcile(reg) = plan(&applied, &add) {
            applied.insert(reg.name.clone(), reg.clone());
        }
        let del = Event::Removed("edge-1".into());
        if let Action::Remove(_) = plan(&applied, &del) {
            applied.remove("edge-1");
        }
        assert!(applied.is_empty());
    }
```

(Delete the old `parses_a_well_formed_data_event`, `to_wg_peer_builds_a_full_tunnel_route_with_keepalive` and the two literal-based reject tests; the keep-alive/malformed-JSON/missing-field tests stay but their `parse_sse_event(...)` assertions now compare against `Option<Event>` — `is_none()` still holds.)

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gsp-agent 2>&1 | tail -20`
Expected: compile errors — ``cannot find function `resolve_startup` ``, ``cannot find type `Registered` ``, ``no field `tunnel_address` ``.

- [x] **Step 3: Implement `address_store.rs`**

Put this above the test module:

```rust
//! The tunnel address this agent was last assigned, persisted next to its key
//! so a restart can come up while the controller is unreachable (spec:
//! "Persistence and offline behaviour"). The controller keeps an owner's
//! address sticky, so the saved value is still valid unless an operator
//! released it. Mirrored in `gsp::tunnel_address` rather than shared (the two
//! binaries have no common library; same precedent as `register.rs`).

use std::path::Path;

use anyhow::Context;

use crate::register::{RegisterError, Registered};

/// `10.60.0.5/16` → `10.60.0.5`.
pub fn ip_of(cidr: &str) -> &str {
    cidr.split_once('/').map(|(ip, _)| ip).unwrap_or(cidr)
}

/// The interface address: the assigned IP with the *network's* prefix, or — in
/// pin-only mode, when the controller reports no network — with the prefix of
/// the operator's pinned `--address`.
pub fn interface_cidr(
    assigned_ip: &str,
    network: Option<&str>,
    pinned_cidr: Option<&str>,
) -> anyhow::Result<String> {
    let source = match (network, pinned_cidr) {
        (Some(n), _) => n,
        (None, Some(p)) => p,
        (None, None) => anyhow::bail!(
            "the controller reported no tunnel_network and no --address was given whose \
             prefix could be used for the interface"
        ),
    };
    let prefix = source
        .rsplit_once('/')
        .map(|(_, p)| p)
        .with_context(|| format!("{source:?} has no /prefix"))?;
    Ok(format!("{assigned_ip}/{prefix}"))
}

pub fn load(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save(path: &Path, cidr: &str) -> anyhow::Result<()> {
    std::fs::write(path, format!("{cidr}\n"))
        .with_context(|| format!("saving the tunnel address to {}", path.display()))
}

#[derive(Debug)]
pub enum Source {
    Controller,
    Saved,
}

#[derive(Debug)]
pub struct StartupAddress {
    pub cidr: String,
    pub source: Source,
}

/// Decides the interface address at startup from the registration outcome:
/// the controller's answer wins; a *transient* failure falls back to the saved
/// address (or fails loudly if there is none); a *rejection* (409/422) never
/// falls back — a stale saved value must not mask an address conflict.
pub fn resolve_startup(
    outcome: Result<Registered, RegisterError>,
    pinned_cidr: Option<&str>,
    saved: Option<String>,
) -> anyhow::Result<StartupAddress> {
    match outcome {
        Ok(r) => Ok(StartupAddress {
            cidr: interface_cidr(&r.tunnel_address, r.tunnel_network.as_deref(), pinned_cidr)?,
            source: Source::Controller,
        }),
        Err(e @ RegisterError::Rejected(_)) => {
            Err(anyhow::Error::new(e).context("the controller refused this tunnel registration"))
        }
        Err(e) => match saved {
            Some(cidr) => Ok(StartupAddress {
                cidr,
                source: Source::Saved,
            }),
            None => Err(anyhow::Error::new(e).context(
                "could not register with the controller and there is no saved tunnel address \
                 to fall back on",
            )),
        },
    }
}
```

- [x] **Step 4: Implement `register.rs`**

Replace everything above the test module with:

```rust
//! Registers this origin with `gsp-controller`'s backend-peers registry
//! (`POST /peers`) — the wire shape is duplicated here rather than shared as a
//! library, the same precedent `controller_client`'s hand-parsed SSE and
//! `aggregator_client`'s duplicated `IngestPayload` already established.
//!
//! The controller is the tunnel address authority (spec
//! `docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`):
//! the answer carries this origin's `tunnel_address`, which startup needs
//! *before* the WireGuard interface can be brought up.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct PeerRegistration<'a> {
    name: &'a str,
    pubkey: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<&'a str>,
    backends: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    tunnel_address: Option<&'a str>,
}

/// What the controller answers to a successful `POST /peers`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Registered {
    pub revision: u64,
    pub tunnel_address: String,
    #[serde(default)]
    pub tunnel_network: Option<String>,
}

/// This origin's identity/facts as submitted on every registration.
#[derive(Clone)]
pub struct Registration {
    pub name: String,
    pub pubkey: String,
    pub endpoint: Option<String>,
    /// `host:port` or the `:port` shorthand (the controller expands it).
    pub backends: Vec<String>,
    /// A pinned tunnel address (bare IP); `None` asks the controller to allocate.
    pub address: Option<String>,
}

fn body(reg: &Registration) -> PeerRegistration<'_> {
    PeerRegistration {
        name: &reg.name,
        pubkey: &reg.pubkey,
        endpoint: reg.endpoint.as_deref(),
        backends: &reg.backends,
        tunnel_address: reg.address.as_deref(),
    }
}

#[derive(Debug)]
pub enum RegisterError {
    /// The controller understood and refused (4xx): retrying cannot help.
    Rejected(String),
    /// Transport trouble or a 5xx: worth retrying.
    Transient(anyhow::Error),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterError::Rejected(m) => f.write_str(m),
            RegisterError::Transient(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// One registration attempt.
pub async fn register_once(
    client: &reqwest::Client,
    controller_url: &str,
    token: Option<&str>,
    reg: &Registration,
) -> Result<Registered, RegisterError> {
    let url = format!("{}/peers", controller_url.trim_end_matches('/'));
    let mut req = client.post(&url).json(&body(reg));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.map_err(|e| {
        RegisterError::Transient(
            anyhow::Error::new(e).context(format!("registering with the controller at {url}")),
        )
    })?;
    let status = resp.status();
    if status.is_success() {
        return resp.json().await.map_err(|e| {
            RegisterError::Transient(
                anyhow::Error::new(e).context("parsing the controller's registration response"),
            )
        });
    }
    let text = resp.text().await.unwrap_or_default();
    let msg = format!("controller rejected registration ({status}): {text}");
    if status.is_client_error() {
        Err(RegisterError::Rejected(msg))
    } else {
        Err(RegisterError::Transient(anyhow::anyhow!(msg)))
    }
}

/// Retries transient failures with backoff until `budget` runs out; a
/// rejection (4xx) returns immediately.
pub async fn register_with_retry(
    client: &reqwest::Client,
    controller_url: &str,
    token: Option<&str>,
    reg: &Registration,
    budget: Duration,
) -> Result<Registered, RegisterError> {
    let deadline = Instant::now() + budget;
    let mut delay = Duration::from_millis(500);
    loop {
        match register_once(client, controller_url, token, reg).await {
            Ok(r) => return Ok(r),
            Err(e @ RegisterError::Rejected(_)) => return Err(e),
            Err(RegisterError::Transient(e)) => {
                if Instant::now() + delay >= deadline {
                    return Err(RegisterError::Transient(e));
                }
                tracing::warn!(error = %format!("{e:#}"), "controller not ready; retrying registration");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
            }
        }
    }
}

/// `Some(message)` when the controller now reports a different address than
/// the one this process is running with. Never fatal: a live interface must
/// not be torn down over a registry change — a restart applies the new one.
pub fn address_change(running: &str, reported: &str) -> Option<String> {
    (running != reported).then(|| {
        format!(
            "the controller now assigns tunnel address {reported} but this process is running \
             with {running}; keeping {running} — restart to apply the new address"
        )
    })
}

/// Re-registers every `interval` for as long as the process runs (a fixed
/// refresh: there is no "did anything change" signal to key off yet).
/// `running_address` is the bare IP the interface was brought up with.
pub async fn run(
    client: reqwest::Client,
    controller_url: String,
    token: Option<String>,
    reg: Registration,
    interval: Duration,
    running_address: String,
) {
    let mut warned = false;
    loop {
        match register_once(&client, &controller_url, token.as_deref(), &reg).await {
            Ok(r) => {
                tracing::info!(revision = r.revision, "registered with the controller");
                match address_change(&running_address, &r.tunnel_address) {
                    Some(msg) if !warned => {
                        tracing::error!("{msg}");
                        warned = true;
                    }
                    Some(_) => {}
                    None => warned = false,
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to register with the controller; will retry")
            }
        }
        tokio::time::sleep(interval).await;
    }
}
```

- [x] **Step 5: Implement the `proxy_subscribe.rs` changes**

Replace the struct, `to_wg_peer`, event parsing and the loop body:

```rust
/// One proxy's registration, exactly as `gsp_controller::proxy_peers::
/// ProxyRegistration` serializes it (duplicated wire shape).
#[derive(Debug, Clone, Deserialize, PartialEq)]
struct ProxyRegistration {
    name: String,
    pubkey: String,
    endpoint: String,
    #[serde(default)]
    tunnel_address: Option<String>,
}

/// Builds the WireGuard peer this registration implies: a **host route to the
/// proxy's own tunnel address** (`/32`) — never `0.0.0.0/0`, which let the
/// last-registered proxy steal every earlier proxy's route — with a keepalive,
/// since a proxy's endpoint is stable but this origin may still be behind NAT.
fn to_wg_peer(reg: &ProxyRegistration) -> anyhow::Result<Peer> {
    let key = Key::try_from(reg.pubkey.as_str())
        .map_err(|e| anyhow::anyhow!("proxy {:?} has an invalid pubkey: {e}", reg.name))?;
    let addr = reg.tunnel_address.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "proxy {:?} has no tunnel_address (is the controller up to date?)",
            reg.name
        )
    })?;
    let ip: std::net::Ipv4Addr = addr.parse().map_err(|e| {
        anyhow::anyhow!("proxy {:?} tunnel_address {addr:?} is invalid: {e}", reg.name)
    })?;
    let mut peer = Peer::new(key);
    peer.set_allowed_ips(vec![format!("{ip}/32")
        .parse()
        .expect("an IPv4 /32 always parses")]);
    peer.set_endpoint(&reg.endpoint).map_err(|e| {
        anyhow::anyhow!(
            "proxy {:?} endpoint {:?} is invalid: {e}",
            reg.name,
            reg.endpoint
        )
    })?;
    peer.persistent_keepalive_interval = Some(25);
    Ok(peer)
}

#[derive(Debug, PartialEq)]
enum Event {
    Registered(ProxyRegistration),
    Removed(String),
}

/// Parses one SSE event block — identical shape to
/// `gsp::tunnel_client::parse_sse_event`.
fn parse_sse_event(event: &str) -> Option<Event> {
    let data_line = event
        .split('\n')
        .find_map(|line| line.strip_prefix("data:"))?
        .trim_start();
    let payload: serde_json::Value = serde_json::from_str(data_line).ok()?;
    if let Some(name) = payload
        .get("removed")
        .and_then(|r| r.get("name"))
        .and_then(|n| n.as_str())
    {
        return Some(Event::Removed(name.to_string()));
    }
    serde_json::from_value(payload.get("registration")?.clone())
        .ok()
        .map(Event::Registered)
}

#[derive(Debug, PartialEq)]
enum Action<'a> {
    Skip,
    Reconcile(&'a ProxyRegistration),
    /// Remove the WireGuard peer with this pubkey.
    Remove(String),
}

/// What to do with `event` given what is already applied — pure, so the
/// catch-up replay (add, then removal) is testable without a WireGuard device.
fn plan<'a>(applied: &HashMap<String, ProxyRegistration>, event: &'a Event) -> Action<'a> {
    match event {
        Event::Registered(reg) if applied.get(&reg.name) == Some(reg) => Action::Skip,
        Event::Registered(reg) => Action::Reconcile(reg),
        Event::Removed(name) => match applied.get(name) {
            Some(old) => Action::Remove(old.pubkey.clone()),
            None => Action::Skip,
        },
    }
}

fn remove_peer(wg: &(dyn WireguardInterfaceApi + Send + Sync), name: &str, pubkey: &str) {
    match Key::try_from(pubkey) {
        Ok(key) => match wg.remove_peer(&key) {
            Ok(()) => tracing::info!(proxy = %name, "removed wireguard peer for a deleted proxy"),
            Err(e) => tracing::warn!(proxy = %name, error = %e, "failed to remove wireguard peer"),
        },
        Err(e) => tracing::error!(proxy = %name, error = %e, "cannot remove a peer with an invalid pubkey"),
    }
}
```

In `subscribe_once`, replace the `if let Some(reg) = parse_sse_event(&event) { … }` block with:

```rust
            if let Some(ev) = parse_sse_event(&event) {
                match plan(last_applied, &ev) {
                    Action::Skip => {}
                    Action::Reconcile(reg) => {
                        reconcile_peer(wg, reg);
                        last_applied.insert(reg.name.clone(), reg.clone());
                    }
                    Action::Remove(pubkey) => {
                        if let Event::Removed(name) = &ev {
                            remove_peer(wg, name, &pubkey);
                            last_applied.remove(name);
                        }
                    }
                }
            }
```

Update `reconcile_peer`'s log (it still prints `endpoint = %reg.endpoint`; also log `address = ?reg.tunnel_address`).

- [x] **Step 6: Implement the `main.rs` changes**

Add `mod address_store;` next to `mod keypair;`. Change the args:

```rust
    /// This interface's own tunnel-internal address as `ip/prefix`
    /// (e.g. `10.60.0.2/24`) to **pin** it. Omit to have the controller allocate
    /// one from its `--tunnel-network` (it is returned on registration and
    /// saved to `<data-dir>/tunnel-address`).
    #[arg(long)]
    address: Option<String>,

    /// Backend addresses this origin fronts: `host:port`, or the shorthand
    /// `:port` ("my tunnel address plus this port"). Every host must be this
    /// origin's own tunnel address.
    #[arg(long, value_delimiter = ',')]
    backends: Vec<String>,
```

```rust
    /// A manually-pinned edge proxy's WireGuard pubkey, in addition to whatever
    /// the proxy-peers registry supplies. Requires `--peer-endpoint` and
    /// `--peer-address`.
    #[arg(long, requires_all = ["peer_endpoint", "peer_address"])]
    peer_pubkey: Option<String>,

    /// The edge proxy's public `ip:port` to dial.
    #[arg(long, requires = "peer_pubkey")]
    peer_endpoint: Option<String>,

    /// The pinned proxy's tunnel address (bare IPv4), routed as a `/32`.
    #[arg(long, requires = "peer_pubkey")]
    peer_address: Option<String>,
```

Replace the backends validation loop and everything from `let address: IpAddrMask = …` through the end of the `bring_up_with` call (current lines 131–180) with:

```rust
    for b in &args.backends {
        let ok = match b.strip_prefix(':') {
            Some(port) => port.parse::<u16>().is_ok(),
            None => b.parse::<SocketAddr>().is_ok(),
        };
        anyhow::ensure!(ok, "--backends entry {b:?} is not host:port or :port");
    }

    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("creating data dir {:?}", args.data_dir))?;
    let key_path = args.data_dir.join("private.key");
    let private_key = keypair::load_or_generate(&key_path)?;
    let pubkey = private_key.public_key().to_string();
    tracing::info!(pubkey = %pubkey, key_path = %key_path.display(), "wireguard identity ready");

    // The controller is the address authority: register BEFORE the interface
    // exists, because the answer is the interface's address.
    let pinned_cidr = args.address.as_deref();
    if let Some(c) = pinned_cidr {
        address_store::ip_of(c)
            .parse::<std::net::Ipv4Addr>()
            .with_context(|| format!("--address {c:?} must be an IPv4 ip/prefix"))?;
    }
    let reg = register::Registration {
        name: args.name.clone(),
        pubkey: pubkey.clone(),
        endpoint: args.endpoint.clone(),
        backends: args.backends.clone(),
        address: pinned_cidr.map(|c| address_store::ip_of(c).to_string()),
    };
    let client = reqwest::Client::new();
    let addr_path = args.data_dir.join("tunnel-address");
    let outcome = register::register_with_retry(
        &client,
        &args.controller_url,
        args.controller_token.as_deref(),
        &reg,
        Duration::from_secs(30),
    )
    .await;
    let start = address_store::resolve_startup(outcome, pinned_cidr, address_store::load(&addr_path))?;
    match start.source {
        address_store::Source::Controller => address_store::save(&addr_path, &start.cidr)?,
        address_store::Source::Saved => tracing::warn!(
            address = %start.cidr,
            "controller unreachable; starting with the last saved tunnel address"
        ),
    }
    tracing::info!(address = %start.cidr, "tunnel address ready");
    let address: IpAddrMask = start
        .cidr
        .parse()
        .map_err(|e| anyhow::anyhow!("tunnel address {:?} is not a valid ip/cidr: {e}", start.cidr))?;

    let mut peers = Vec::new();
    if let (Some(peer_pubkey), Some(peer_endpoint), Some(peer_address)) =
        (&args.peer_pubkey, &args.peer_endpoint, &args.peer_address)
    {
        let public_key: Key = peer_pubkey
            .parse()
            .map_err(|e| anyhow::anyhow!("--peer-pubkey {peer_pubkey:?} is not valid: {e}"))?;
        let endpoint: SocketAddr = peer_endpoint
            .parse()
            .with_context(|| format!("--peer-endpoint {peer_endpoint:?} is not a valid ip:port"))?;
        let peer_ip: std::net::Ipv4Addr = peer_address
            .parse()
            .with_context(|| format!("--peer-address {peer_address:?} is not an IPv4 address"))?;
        let mut peer = Peer::new(public_key);
        peer.endpoint = Some(endpoint);
        // A host route to the pinned proxy's own tunnel address — never
        // 0.0.0.0/0, which would collide with every other proxy's route.
        peer.allowed_ips = vec![format!("{peer_ip}/32").parse().unwrap()];
        peer.persistent_keepalive_interval = Some(25);
        tracing::info!(pubkey = %peer_pubkey, endpoint = %endpoint, "peering with the edge proxy");
        peers.push(peer);
    }

    let wg: Arc<dyn defguard_wireguard_rs::WireguardInterfaceApi + Send + Sync> = Arc::from(
        interface::bring_up_with(
            &args.iface,
            &private_key,
            args.listen_port,
            address,
            peers,
            args.userspace,
        )
        .context("bringing up the local WireGuard interface")?,
    );
    tracing::info!(iface = %args.iface, port = args.listen_port, "wireguard interface up");
```

Replace the `register::run` spawn:

```rust
    tokio::spawn(register::run(
        client,
        args.controller_url.clone(),
        args.controller_token.clone(),
        reg,
        Duration::from_secs(args.register_interval_sec),
        address_store::ip_of(&start.cidr).to_string(),
    ));
```

Remove the now-unused `let client = reqwest::Client::new();` that preceded the old spawn.

- [x] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p gsp-agent 2>&1 | tail -8`
Expected: `test result: ok.` (all address_store, register and proxy_subscribe tests).

- [x] **Step 8: Verify and commit**

```bash
cargo fmt --all && make check
git add -A crates/gsp-agent
git commit -m "feat(agent): register for a tunnel address first, route proxies as /32, honour tombstones

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```
Expected: `make check` exits 0 (the e2e lab is not part of it and is still red until Task 7).

---

### Task 6: `gsp --tunnel-*` — the same for the proxy

**Files:**
- Create: `crates/gsp/src/tunnel_address.rs`
- Modify: `crates/gsp/src/proxy_register.rs`, `crates/gsp/src/tunnel_client.rs`, `crates/gsp/src/main.rs`, `crates/gsp/Cargo.toml` (`tempfile.workspace = true` under `[dev-dependencies]`)

**Interfaces:**
- Consumes: Task 2/4 wire shapes.
- Produces: the proxy-side mirrors of Task 5's modules — `proxy_register::{Registration{name,pubkey,endpoint:String,address:Option<String>}, Registered, RegisterError, register_once, register_with_retry, address_change, run}`, `tunnel_address::{ip_of, interface_cidr, load, save, StartupAddress, Source, resolve_startup}`, and in `tunnel_client`: `Event`, `Action`, `plan`, `/32` origin peers, tombstone handling. CLI: `--tunnel-address` optional (`ip/prefix` pins; omit to allocate).

- [x] **Step 1: Write the failing tests**

Create `crates/gsp/src/tunnel_address.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy_register::{RegisterError, Registered};

    fn registered(ip: &str, net: Option<&str>) -> Registered {
        Registered {
            revision: 1,
            tunnel_address: ip.into(),
            tunnel_network: net.map(str::to_string),
        }
    }

    #[test]
    fn ip_of_strips_the_prefix() {
        assert_eq!(ip_of("10.60.0.5/16"), "10.60.0.5");
        assert_eq!(ip_of("10.60.0.5"), "10.60.0.5");
    }

    #[test]
    fn the_networks_prefix_wins_over_a_pinned_one() {
        assert_eq!(
            interface_cidr("10.60.0.5", Some("10.60.0.0/16"), Some("10.60.0.5/24")).unwrap(),
            "10.60.0.5/16"
        );
    }

    #[test]
    fn pin_only_mode_uses_the_pinned_prefix_and_errors_without_one() {
        assert_eq!(
            interface_cidr("10.60.0.5", None, Some("10.60.0.5/24")).unwrap(),
            "10.60.0.5/24"
        );
        assert!(interface_cidr("10.60.0.5", None, None).is_err());
    }

    #[test]
    fn a_saved_address_round_trips_and_blank_or_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tunnel-address");
        assert_eq!(load(&path), None);
        save(&path, "10.60.0.5/16").unwrap();
        assert_eq!(load(&path).as_deref(), Some("10.60.0.5/16"));
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(load(&path), None);
    }

    #[test]
    fn startup_uses_the_controllers_answer_when_it_registers() {
        let s = resolve_startup(
            Ok(registered("10.60.0.5", Some("10.60.0.0/16"))),
            None,
            Some("10.60.0.9/16".into()),
        )
        .unwrap();
        assert_eq!(s.cidr, "10.60.0.5/16");
        assert!(matches!(s.source, Source::Controller));
    }

    #[test]
    fn startup_falls_back_to_the_saved_address_when_the_controller_is_unreachable() {
        let s = resolve_startup(
            Err(RegisterError::Transient(anyhow::anyhow!("connection refused"))),
            None,
            Some("10.60.0.5/16".into()),
        )
        .unwrap();
        assert_eq!(s.cidr, "10.60.0.5/16");
        assert!(matches!(s.source, Source::Saved));
    }

    #[test]
    fn startup_fails_without_a_saved_address_when_the_controller_is_unreachable() {
        let err = resolve_startup(
            Err(RegisterError::Transient(anyhow::anyhow!("connection refused"))),
            None,
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no saved tunnel address"), "{err:#}");
    }

    #[test]
    fn startup_never_falls_back_when_the_controller_refuses_the_registration() {
        // A 409 (address held) must not be papered over by a stale saved value.
        let err = resolve_startup(
            Err(RegisterError::Rejected("already held by origin \"x\"".into())),
            None,
            Some("10.60.0.5/16".into()),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("already held"), "{err:#}");
    }
}
```

In `proxy_register.rs`, replace the tests module with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Registration {
        Registration {
            name: "edge-1".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: "203.0.113.9:51820".into(),
            address: None,
        }
    }

    #[test]
    fn the_tunnel_address_is_omitted_from_the_body_when_not_pinned() {
        let json = serde_json::to_string(&body(&reg())).unwrap();
        assert!(!json.contains("tunnel_address"));
        assert!(json.contains("\"endpoint\":\"203.0.113.9:51820\""));
    }

    #[test]
    fn a_pinned_address_is_serialized() {
        let mut r = reg();
        r.address = Some("10.60.0.3".into());
        let json = serde_json::to_string(&body(&r)).unwrap();
        assert!(json.contains("\"tunnel_address\":\"10.60.0.3\""));
    }

    #[test]
    fn address_change_reports_only_a_real_difference() {
        assert_eq!(address_change("10.60.0.5", "10.60.0.5"), None);
        let msg = address_change("10.60.0.5", "10.60.0.9").unwrap();
        assert!(msg.contains("10.60.0.5") && msg.contains("10.60.0.9"));
        assert!(msg.contains("restart"));
    }

    /// A one-shot HTTP server answering every request with a canned response.
    async fn canned(status_line: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_successful_registration_parses_the_assigned_address_and_network() {
        let url = canned(
            "200 OK",
            r#"{"revision":4,"tunnel_address":"10.60.0.3","tunnel_network":"10.60.0.0/16"}"#,
        )
        .await;
        let got = register_once(&reqwest::Client::new(), &url, None, &reg())
            .await
            .unwrap();
        assert_eq!(got.tunnel_address, "10.60.0.3");
        assert_eq!(got.tunnel_network.as_deref(), Some("10.60.0.0/16"));
    }

    #[tokio::test]
    async fn a_4xx_is_a_permanent_rejection_carrying_the_controllers_message() {
        let url = canned("409 Conflict", r#"{"error":"address 10.60.0.2 is already held by origin \"x\""}"#).await;
        match register_once(&reqwest::Client::new(), &url, None, &reg()).await {
            Err(RegisterError::Rejected(m)) => assert!(m.contains("already held"), "{m}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_5xx_or_a_refused_connection_is_transient() {
        let url = canned("503 Service Unavailable", r#"{"error":"exhausted"}"#).await;
        assert!(matches!(
            register_once(&reqwest::Client::new(), &url, None, &reg()).await,
            Err(RegisterError::Transient(_))
        ));
        assert!(matches!(
            register_once(&reqwest::Client::new(), "http://127.0.0.1:1", None, &reg()).await,
            Err(RegisterError::Transient(_))
        ));
    }

    #[tokio::test]
    async fn retrying_gives_up_on_a_permanent_rejection_immediately() {
        let url = canned("409 Conflict", r#"{"error":"held"}"#).await;
        let started = std::time::Instant::now();
        let err = register_with_retry(
            &reqwest::Client::new(),
            &url,
            None,
            &reg(),
            Duration::from_secs(30),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RegisterError::Rejected(_)));
        assert!(started.elapsed() < Duration::from_secs(5), "must not retry a 409");
    }
}
```

In `tunnel_client.rs` tests: add `tunnel_address: None` to the existing `PeerRegistration { … }` literals, delete `to_wg_peer_builds_host_allowed_ips_from_backends` and `to_wg_peer_rejects_a_malformed_backend_address` (backends no longer drive routing), and add:

```rust
    fn reg(addr: Option<&str>) -> PeerRegistration {
        PeerRegistration {
            name: "home".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: None,
            backends: vec![],
            tunnel_address: addr.map(str::to_string),
        }
    }

    #[test]
    fn to_wg_peer_routes_the_origins_tunnel_address_as_a_host_route() {
        // Even with no backends yet the origin is reachable at its address.
        let peer = to_wg_peer(&reg(Some("10.60.0.5"))).unwrap();
        assert_eq!(peer.allowed_ips.len(), 1);
        assert_eq!(peer.allowed_ips[0].cidr, 32);
        assert_eq!(peer.allowed_ips[0].ip.to_string(), "10.60.0.5");
    }

    #[test]
    fn to_wg_peer_rejects_an_origin_without_a_tunnel_address() {
        assert!(to_wg_peer(&reg(None)).is_err());
    }

    #[test]
    fn parses_a_registration_and_a_tombstone_event() {
        let event = r#"data: {"revision":1,"registration":{"name":"home","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","endpoint":"203.0.113.7:51820","backends":["10.60.0.2:1"],"tunnel_address":"10.60.0.2"}}"#;
        assert!(matches!(parse_sse_event(event), Some(Event::Registered(_))));
        let gone = r#"data: {"revision":2,"removed":{"name":"home"}}"#;
        assert_eq!(parse_sse_event(gone), Some(Event::Removed("home".into())));
    }

    #[test]
    fn plan_skips_unchanged_reconciles_changed_and_removes_known() {
        let mut applied = std::collections::HashMap::new();
        let r = reg(Some("10.60.0.5"));
        assert_eq!(plan(&applied, &Event::Registered(r.clone())), Action::Reconcile(&r));
        applied.insert(r.name.clone(), r.clone());
        assert_eq!(plan(&applied, &Event::Registered(r.clone())), Action::Skip);
        assert_eq!(
            plan(&applied, &Event::Removed("home".into())),
            Action::Remove(r.pubkey.clone())
        );
        assert_eq!(plan(&applied, &Event::Removed("other".into())), Action::Skip);
    }

    #[test]
    fn registered_then_removed_leaves_nothing() {
        // Review Focus 3: a catch-up from revision 0 replays the add and then
        // the removal; applying both in order must end with no peer tracked.
        let mut applied = std::collections::HashMap::new();
        let r = reg(Some("10.60.0.5"));
        if let Action::Reconcile(reg) = plan(&applied, &Event::Registered(r)) {
            applied.insert(reg.name.clone(), reg.clone());
        }
        if let Action::Remove(_) = plan(&applied, &Event::Removed("home".into())) {
            applied.remove("home");
        }
        assert!(applied.is_empty());
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gsp tunnel 2>&1 | tail -15` and `cargo test -p gsp proxy_register 2>&1 | tail -10`
Expected: compile errors (missing `Registered`, `resolve_startup`, `Event`, the `tunnel_address` field).

- [x] **Step 3: Implement the mirrors**

`crates/gsp/src/tunnel_address.rs` — put this above the test module:

```rust
//! The tunnel address this proxy was last assigned, persisted next to its key
//! so a restart can come up while the controller is unreachable (spec:
//! "Persistence and offline behaviour"). The controller keeps an owner's
//! address sticky, so the saved value is still valid unless an operator
//! released it. Mirrored from `gsp-agent::address_store` rather than shared (the two
//! binaries have no common library; same precedent as `proxy_register.rs`).

use std::path::Path;

use anyhow::Context;

use crate::proxy_register::{RegisterError, Registered};

/// `10.60.0.5/16` → `10.60.0.5`.
pub fn ip_of(cidr: &str) -> &str {
    cidr.split_once('/').map(|(ip, _)| ip).unwrap_or(cidr)
}

/// The interface address: the assigned IP with the *network's* prefix, or — in
/// pin-only mode, when the controller reports no network — with the prefix of
/// the operator's pinned `--address`.
pub fn interface_cidr(
    assigned_ip: &str,
    network: Option<&str>,
    pinned_cidr: Option<&str>,
) -> anyhow::Result<String> {
    let source = match (network, pinned_cidr) {
        (Some(n), _) => n,
        (None, Some(p)) => p,
        (None, None) => anyhow::bail!(
            "the controller reported no tunnel_network and no --address was given whose \
             prefix could be used for the interface"
        ),
    };
    let prefix = source
        .rsplit_once('/')
        .map(|(_, p)| p)
        .with_context(|| format!("{source:?} has no /prefix"))?;
    Ok(format!("{assigned_ip}/{prefix}"))
}

pub fn load(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save(path: &Path, cidr: &str) -> anyhow::Result<()> {
    std::fs::write(path, format!("{cidr}\n"))
        .with_context(|| format!("saving the tunnel address to {}", path.display()))
}

#[derive(Debug)]
pub enum Source {
    Controller,
    Saved,
}

#[derive(Debug)]
pub struct StartupAddress {
    pub cidr: String,
    pub source: Source,
}

/// Decides the interface address at startup from the registration outcome:
/// the controller's answer wins; a *transient* failure falls back to the saved
/// address (or fails loudly if there is none); a *rejection* (409/422) never
/// falls back — a stale saved value must not mask an address conflict.
pub fn resolve_startup(
    outcome: Result<Registered, RegisterError>,
    pinned_cidr: Option<&str>,
    saved: Option<String>,
) -> anyhow::Result<StartupAddress> {
    match outcome {
        Ok(r) => Ok(StartupAddress {
            cidr: interface_cidr(&r.tunnel_address, r.tunnel_network.as_deref(), pinned_cidr)?,
            source: Source::Controller,
        }),
        Err(e @ RegisterError::Rejected(_)) => {
            Err(anyhow::Error::new(e).context("the controller refused this tunnel registration"))
        }
        Err(e) => match saved {
            Some(cidr) => Ok(StartupAddress {
                cidr,
                source: Source::Saved,
            }),
            None => Err(anyhow::Error::new(e).context(
                "could not register with the controller and there is no saved tunnel address \
                 to fall back on",
            )),
        },
    }
}
```

`crates/gsp/src/proxy_register.rs` — replace everything above the tests module with:

```rust
//! Registers this proxy with `gsp-controller`'s proxy-peers registry
//! (`POST /proxy-peers`) — the mirror image of `gsp-agent::register`,
//! duplicated rather than shared for the same reason: no library crate sits
//! between these two binaries.
//!
//! Exists so every origin's `gsp-agent` can learn about every edge proxy by
//! subscribing to that registry. The controller is also the tunnel address
//! authority (spec
//! `docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`): the
//! answer carries this proxy's `tunnel_address`, which startup needs *before*
//! the WireGuard interface can be brought up.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct ProxyRegistration<'a> {
    name: &'a str,
    pubkey: &'a str,
    endpoint: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tunnel_address: Option<&'a str>,
}

/// What the controller answers to a successful `POST /proxy-peers`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Registered {
    pub revision: u64,
    pub tunnel_address: String,
    #[serde(default)]
    pub tunnel_network: Option<String>,
}

/// This proxy's identity/facts as submitted on every registration.
#[derive(Clone)]
pub struct Registration {
    pub name: String,
    pub pubkey: String,
    /// This proxy's public dial-out address (`ip:port`) — required for a proxy.
    pub endpoint: String,
    /// A pinned tunnel address (bare IP); `None` asks the controller to allocate.
    pub address: Option<String>,
}

fn body(reg: &Registration) -> ProxyRegistration<'_> {
    ProxyRegistration {
        name: &reg.name,
        pubkey: &reg.pubkey,
        endpoint: &reg.endpoint,
        tunnel_address: reg.address.as_deref(),
    }
}

#[derive(Debug)]
pub enum RegisterError {
    /// The controller understood and refused (4xx): retrying cannot help.
    Rejected(String),
    /// Transport trouble or a 5xx: worth retrying.
    Transient(anyhow::Error),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterError::Rejected(m) => f.write_str(m),
            RegisterError::Transient(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// One registration attempt.
pub async fn register_once(
    client: &reqwest::Client,
    controller_url: &str,
    token: Option<&str>,
    reg: &Registration,
) -> Result<Registered, RegisterError> {
    let url = format!("{}/proxy-peers", controller_url.trim_end_matches('/'));
    let mut req = client.post(&url).json(&body(reg));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.map_err(|e| {
        RegisterError::Transient(
            anyhow::Error::new(e).context(format!("registering with the controller at {url}")),
        )
    })?;
    let status = resp.status();
    if status.is_success() {
        return resp.json().await.map_err(|e| {
            RegisterError::Transient(
                anyhow::Error::new(e).context("parsing the controller's registration response"),
            )
        });
    }
    let text = resp.text().await.unwrap_or_default();
    let msg = format!("controller rejected proxy registration ({status}): {text}");
    if status.is_client_error() {
        Err(RegisterError::Rejected(msg))
    } else {
        Err(RegisterError::Transient(anyhow::anyhow!(msg)))
    }
}

/// Retries transient failures with backoff until `budget` runs out; a
/// rejection (4xx) returns immediately.
pub async fn register_with_retry(
    client: &reqwest::Client,
    controller_url: &str,
    token: Option<&str>,
    reg: &Registration,
    budget: Duration,
) -> Result<Registered, RegisterError> {
    let deadline = Instant::now() + budget;
    let mut delay = Duration::from_millis(500);
    loop {
        match register_once(client, controller_url, token, reg).await {
            Ok(r) => return Ok(r),
            Err(e @ RegisterError::Rejected(_)) => return Err(e),
            Err(RegisterError::Transient(e)) => {
                if Instant::now() + delay >= deadline {
                    return Err(RegisterError::Transient(e));
                }
                tracing::warn!(error = %format!("{e:#}"), "controller not ready; retrying registration");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
            }
        }
    }
}

/// `Some(message)` when the controller now reports a different address than
/// the one this process is running with. Never fatal: a live interface must
/// not be torn down over a registry change — a restart applies the new one.
pub fn address_change(running: &str, reported: &str) -> Option<String> {
    (running != reported).then(|| {
        format!(
            "the controller now assigns tunnel address {reported} but this process is running \
             with {running}; keeping {running} — restart to apply the new address"
        )
    })
}

/// Re-registers every `interval` for as long as the process runs (a fixed
/// refresh: there is no "did anything change" signal to key off yet).
/// `running_address` is the bare IP the interface was brought up with.
pub async fn run(
    client: reqwest::Client,
    controller_url: String,
    token: Option<String>,
    reg: Registration,
    interval: Duration,
    running_address: String,
) {
    let mut warned = false;
    loop {
        match register_once(&client, &controller_url, token.as_deref(), &reg).await {
            Ok(r) => {
                tracing::info!(revision = r.revision, "registered as a proxy peer with the controller");
                match address_change(&running_address, &r.tunnel_address) {
                    Some(msg) if !warned => {
                        tracing::error!("{msg}");
                        warned = true;
                    }
                    Some(_) => {}
                    None => warned = false,
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to register as a proxy peer; will retry")
            }
        }
        tokio::time::sleep(interval).await;
    }
}
```

`crates/gsp/src/tunnel_client.rs` — replace the `PeerRegistration` struct and `to_wg_peer`:

```rust
/// One origin's registration, exactly as `gsp-controller::peers::
/// PeerRegistration` serializes it (duplicated wire shape — same precedent
/// `controller_client`'s hand-parsed SSE and `aggregator_client`'s duplicated
/// `IngestPayload` already established).
#[derive(Debug, Clone, Deserialize, PartialEq)]
struct PeerRegistration {
    name: String,
    pubkey: String,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    backends: Vec<String>,
    #[serde(default)]
    tunnel_address: Option<String>,
}

/// Builds the WireGuard peer this registration implies: a **host route to the
/// origin's own tunnel address** (`/32`) — the controller guarantees it is
/// unique and that every backend lives on it — and `endpoint` only when the
/// registration carries one (the common-case-absent field, not a bug).
fn to_wg_peer(reg: &PeerRegistration) -> anyhow::Result<Peer> {
    let key = Key::try_from(reg.pubkey.as_str())
        .map_err(|e| anyhow::anyhow!("origin {:?} has an invalid pubkey: {e}", reg.name))?;
    let addr = reg.tunnel_address.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "origin {:?} has no tunnel_address (is the controller up to date?)",
            reg.name
        )
    })?;
    let ip: std::net::Ipv4Addr = addr.parse().map_err(|e| {
        anyhow::anyhow!("origin {:?} tunnel_address {addr:?} is invalid: {e}", reg.name)
    })?;
    let mut peer = Peer::new(key);
    peer.set_allowed_ips(vec![IpAddrMask::host(std::net::IpAddr::V4(ip))]);
    if let Some(endpoint) = &reg.endpoint {
        peer.set_endpoint(endpoint).map_err(|e| {
            anyhow::anyhow!(
                "origin {:?} endpoint {endpoint:?} is invalid: {e}",
                reg.name
            )
        })?;
    }
    Ok(peer)
}
```

Replace the existing `parse_sse_event` with the following (it also adds the event, plan and removal helpers; `HashMap` is `std::collections::HashMap`, already used by `run`):

```rust
#[derive(Debug, PartialEq)]
enum Event {
    Registered(PeerRegistration),
    Removed(String),
}

/// Parses one SSE event block — identical shape to
/// `gsp-agent::proxy_subscribe::parse_sse_event`.
fn parse_sse_event(event: &str) -> Option<Event> {
    let data_line = event
        .split('\n')
        .find_map(|line| line.strip_prefix("data:"))?
        .trim_start();
    let payload: serde_json::Value = serde_json::from_str(data_line).ok()?;
    if let Some(name) = payload
        .get("removed")
        .and_then(|r| r.get("name"))
        .and_then(|n| n.as_str())
    {
        return Some(Event::Removed(name.to_string()));
    }
    serde_json::from_value(payload.get("registration")?.clone())
        .ok()
        .map(Event::Registered)
}

#[derive(Debug, PartialEq)]
enum Action<'a> {
    Skip,
    Reconcile(&'a PeerRegistration),
    /// Remove the WireGuard peer with this pubkey.
    Remove(String),
}

/// What to do with `event` given what is already applied — pure, so the
/// catch-up replay (add, then removal) is testable without a WireGuard device.
fn plan<'a>(applied: &HashMap<String, PeerRegistration>, event: &'a Event) -> Action<'a> {
    match event {
        Event::Registered(reg) if applied.get(&reg.name) == Some(reg) => Action::Skip,
        Event::Registered(reg) => Action::Reconcile(reg),
        Event::Removed(name) => match applied.get(name) {
            Some(old) => Action::Remove(old.pubkey.clone()),
            None => Action::Skip,
        },
    }
}

fn remove_peer(wg: &(dyn WireguardInterfaceApi + Send + Sync), name: &str, pubkey: &str) {
    match Key::try_from(pubkey) {
        Ok(key) => match wg.remove_peer(&key) {
            Ok(()) => tracing::info!(origin = %name, "removed wireguard peer for a deleted origin"),
            Err(e) => tracing::warn!(origin = %name, error = %e, "failed to remove wireguard peer"),
        },
        Err(e) => tracing::error!(origin = %name, error = %e, "cannot remove a peer with an invalid pubkey"),
    }
}
```

In `subscribe_once`, replace the `if let Some(reg) = parse_sse_event(&event) { … }` block with:

```rust
            if let Some(ev) = parse_sse_event(&event) {
                match plan(last_applied, &ev) {
                    Action::Skip => {}
                    Action::Reconcile(reg) => {
                        reconcile_peer(wg, reg);
                        last_applied.insert(reg.name.clone(), reg.clone());
                    }
                    Action::Remove(pubkey) => {
                        if let Event::Removed(name) = &ev {
                            remove_peer(wg, name, &pubkey);
                            last_applied.remove(name);
                        }
                    }
                }
            }
```
(`std::collections::HashMap` is spelled out in this file; import it with `use std::collections::HashMap;` at the top so `plan`'s signature compiles.)

`reconcile_peer`'s log line: replace `backends = ?reg.backends` with `address = ?reg.tunnel_address, backends = ?reg.backends`.

`crates/gsp/Cargo.toml`: add `tempfile.workspace = true` under `[dev-dependencies]`.

`crates/gsp/src/main.rs`:

`main.rs` (gsp): in the `Args` doc for `--tunnel-address` say it is optional (`ip/prefix` pins, omit to allocate); in `async_main` delete the `.ok_or_else(|| anyhow!("--tunnel-iface requires --tunnel-address"))?` and set `TunnelConfig.address: Option<String>`; add `mod tunnel_address;`. Replace the `Some(tc) => { … }` arm of `let tunnel = match tunnel_config` with:

```rust
        Some(tc) => {
            let private_key = tunnel_client::load_or_generate_key(&tc.key_file)
                .with_context(|| format!("loading tunnel key from {:?}", tc.key_file))?;
            let pubkey = private_key.public_key().to_string();

            // The controller is the address authority: register BEFORE the
            // interface exists (the answer is its address) and before any
            // listener binds — a failure here is a failing `--tunnel-*`.
            let pinned_cidr = tc.address.clone();
            if let Some(c) = pinned_cidr.as_deref() {
                tunnel_address::ip_of(c)
                    .parse::<std::net::Ipv4Addr>()
                    .with_context(|| format!("--tunnel-address {c:?} must be an IPv4 ip/prefix"))?;
            }
            let reg = proxy_register::Registration {
                name: tc.name.clone(),
                pubkey,
                endpoint: tc.endpoint.clone(),
                address: pinned_cidr
                    .as_deref()
                    .map(|c| tunnel_address::ip_of(c).to_string()),
            };
            let client = reqwest::Client::new();
            let addr_path = {
                let mut p = tc.key_file.clone().into_os_string();
                p.push(".address");
                PathBuf::from(p)
            };
            let outcome = proxy_register::register_with_retry(
                &client,
                &tc.controller_url,
                tc.controller_token.as_deref(),
                &reg,
                Duration::from_secs(30),
            )
            .await;
            let start = tunnel_address::resolve_startup(
                outcome,
                pinned_cidr.as_deref(),
                tunnel_address::load(&addr_path),
            )?;
            match start.source {
                tunnel_address::Source::Controller => {
                    tunnel_address::save(&addr_path, &start.cidr)?
                }
                tunnel_address::Source::Saved => tracing::warn!(
                    address = %start.cidr,
                    "controller unreachable; starting with the last saved tunnel address"
                ),
            }
            let address: defguard_wireguard_rs::net::IpAddrMask =
                start.cidr.parse().map_err(|e| {
                    anyhow::anyhow!("tunnel address {:?} is invalid: {e}", start.cidr)
                })?;
            let wg: Arc<dyn defguard_wireguard_rs::WireguardInterfaceApi + Send + Sync> =
                Arc::from(tunnel_client::bring_up(
                    &tc.iface,
                    &private_key,
                    tc.listen_port,
                    address,
                    tc.userspace,
                )?);
            tracing::info!(
                iface = %tc.iface,
                port = tc.listen_port,
                address = %start.cidr,
                pubkey = %private_key.public_key(),
                controller = %tc.controller_url,
                "wireguard tunnel interface up; subscribing to backend-peers updates"
            );
            let task = tokio::spawn(tunnel_client::run(
                tc.controller_url.clone(),
                tc.controller_token.clone(),
                wg.clone(),
            ));
            // Register ourselves (periodically) so every origin's `gsp-agent`
            // can peer with us — the mirror image of `task` above.
            let register_task = tokio::spawn(proxy_register::run(
                client,
                tc.controller_url,
                tc.controller_token,
                reg,
                tc.register_interval,
                tunnel_address::ip_of(&start.cidr).to_string(),
            ));
            Some((task, register_task, wg))
        }
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gsp 2>&1 | tail -8`
Expected: `test result: ok.`

- [x] **Step 5: Verify and commit**

```bash
cargo fmt --all && make check
git add -A crates/gsp
git commit -m "feat(gsp): register for a tunnel address before bringing the tunnel up, route origins as /32

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```
Expected: `make check` exits 0.

---
### Task 7: End-to-end — harness, the un-ignored bug test, and new scenarios

**Files:**
- Modify: `crates/gsp-fleet-tests/src/tunnel.rs`, `crates/gsp-fleet-tests/tests/tunnel.rs`
- Modify: `Makefile` (`tunnel-e2e`, `tunnel-e2e-ci`)

**Interfaces:**
- Consumes: Tasks 1–6 (a controller with `--tunnel-network`, agents/proxies that need no hand-picked addresses), `gsp_fleet_tests::{spawn_controller_with, port_is_down, wait_until, wait_http_up}`.
- Produces on `TunnelLab`:
  - `start_edge(&mut self, name: &str, pinned_pubkey: Option<&str>) -> Result<usize>` (the `tunnel_last_octet` argument is gone)
  - `origin_ip(&self) -> Ipv4Addr`, `proxy_address(&self, name: &str) -> Result<String>`, `proxy_last_seen(&self, name: &str) -> Result<u64>`
  - `stop_controller(&mut self)`, `start_controller(&mut self)`, `restart_edge(&mut self, idx: usize)`, `wait_roundtrip_after_restart(&self, edge: usize)`, `agent_refused(&mut self, name: &str, address_cidr: &str) -> Result<String>`

- [x] **Step 1: Write the failing scenarios**

In `tests/tunnel.rs`: change the import to `use gsp_fleet_tests::{spawn_controller_on, wait_http_up, wait_until};`; change **every** `t.start_edge("edge-N", <octet>, X)` call to `t.start_edge("edge-N", X)` (seven call sites); delete the sentence "(That the *first* proxy keeps working alongside it is NOT asserted here — it doesn't today; see `known_bug_two_proxies_cannot_share_one_origin`.)" from scenario 3's doc comment.

Rename `known_bug_two_proxies_cannot_share_one_origin` to `two_proxies_share_one_origin`, remove its `#[ignore = "KNOWN BUG …"]` attribute in favour of the standard `#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]`, and replace its doc comment with:

```rust
/// Scenario 5 — the multi-proxy fix. Two proxies share one origin and both
/// must carry traffic at the same time. Before the address authority the agent
/// gave every proxy `AllowedIPs = 0.0.0.0/0`, so the proxy that registered last
/// stole the earlier one's route and its connections timed out.
```
(keep the body unchanged).

Append the new scenarios:

```rust
/// Scenario 6 — a hand-picked address another peer already holds is refused
/// with a clear error, and the holder is unaffected.
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn a_pinned_address_collision_is_refused() -> Result<()> {
    ensure_built();
    let mut t = TunnelLab::new().await?;
    t.start_origin(true).await?;
    let taken = t.origin_ip();
    let log = t.agent_refused("origin-b", &format!("{taken}/16")).await?;
    assert!(
        log.contains("already held"),
        "the refusal should say why, got:\n{log}"
    );
    assert!(t.agent_alive(), "the first origin must be unaffected");
    t.pass();
    Ok(())
}

/// Scenario 7 — an edge that restarts keeps its tunnel address (the
/// controller's allocation is sticky) and traffic recovers.
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn an_edge_restart_keeps_its_address() -> Result<()> {
    ensure_built();
    let mut t = TunnelLab::new().await?;
    t.start_origin(true).await?;
    let edge = t.start_edge("edge-1", None).await?;
    t.wait_roundtrip(edge).await?;
    let before = t.proxy_address("edge-1").await?;

    t.restart_edge(edge).await?;
    t.wait_roundtrip_after_restart(edge).await?;
    assert_eq!(t.proxy_address("edge-1").await?, before);
    t.pass();
    Ok(())
}

/// Scenario 8 — Review Focus 5. An edge restarts while the controller is down:
/// it must come up on its saved address (admin `/healthz` answers), and when
/// the controller returns it re-registers with the same address.
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn an_edge_restarts_with_the_controller_down() -> Result<()> {
    ensure_built();
    let mut t = TunnelLab::new().await?;
    t.start_origin(true).await?;
    let edge = t.start_edge("edge-1", None).await?;
    t.wait_roundtrip(edge).await?;
    let before = t.proxy_address("edge-1").await?;
    let seen_before = t.proxy_last_seen("edge-1").await?;

    t.stop_controller().await?;
    // `restart_edge` returns only once the admin API answers — i.e. startup
    // went ahead on the saved address instead of failing.
    t.restart_edge(edge).await?;
    t.start_controller().await?;

    {
        let t = &t;
        wait_until(
            || async move {
                Ok(t.proxy_last_seen("edge-1")
                    .await
                    .is_ok_and(|s| s > seen_before))
            },
            Duration::from_secs(30),
            "the restarted edge to re-register with the returned controller",
        )
        .await?;
    }
    assert_eq!(t.proxy_address("edge-1").await?, before);
    t.pass();
    Ok(())
}
```

Run: `cargo test -p gsp-fleet-tests --test tunnel --no-run 2>&1 | tail -15`
Expected: compile errors — ``this method takes 3 arguments but 2 arguments were supplied`` (`start_edge`) and ``no method named `origin_ip` ``, `proxy_address`, `proxy_last_seen`, `stop_controller`, `start_controller`, `restart_edge`, `wait_roundtrip_after_restart`, `agent_refused`.

- [x] **Step 2: Implement the harness changes (`src/tunnel.rs`)**

Constants and imports:

```rust
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
```
Delete `const ORIGIN_TUNNEL_IP`; add:

```rust
const TUNNEL_NETWORK: &str = "10.60.0.0/16";

fn controller_args() -> Vec<String> {
    vec!["--tunnel-network".to_string(), TUNNEL_NETWORK.to_string()]
}
```
Import `spawn_controller_with` and `port_is_down` from the crate root (replace `spawn_controller_on` in the `use crate::{…}` line).

Structs:

```rust
struct Origin {
    agent: Proc,
    #[allow(dead_code)] // held for its Drop: stops the echo server
    echo: Option<EchoServer>,
    #[allow(dead_code)] // held for its Drop: stops the probe echo server
    probe: Option<EchoServer>,
    ns: Ns,
    pubkey: String,
    /// The tunnel address the controller allocated to this origin.
    ip: Ipv4Addr,
}

struct Edge {
    /// `None` only transiently, while [`TunnelLab::restart_edge`] swaps it.
    gsp: Option<Proc>,
    ns: Ns,
    args: Vec<String>,
}
```
`TunnelLab.controller` becomes `Option<Proc>`.

`new()`: spawn with `spawn_controller_with(&dir.path().join("controller"), &format!("0.0.0.0:{controller_port}"), &controller_args())?`, store `controller: Some(controller)`.

`start_origin`: drop the `"--address", &format!("{ORIGIN_TUNNEL_IP}/24")` pair and replace the backends pair with `"--backends", &format!(":{ECHO_PORT}")`. After reading `pubkey`, also read the allocated address:

```rust
        let ip: Ipv4Addr = body["tunnel_address"]
            .as_str()
            .context("registry entry has no tunnel_address")?
            .parse()?;
```
and store `ip` in `Origin`.

`wait_tunnel_up`: `let origin_ip = origin.ip;` before taking `let ns = &self.edges[edge].ns;`, and inside the closure use `SocketAddr::new(IpAddr::V4(origin_ip), PROBE_PORT)`.

`start_edge`: new signature `(&mut self, name: &str, pinned_pubkey: Option<&str>)`; delete the `"--tunnel-address", &format!("10.60.0.{tunnel_last_octet}/24")` pair; after spawning:

```rust
        let gsp = Proc::spawn_in(Some(&ns), "gsp", &args)?;
        wait_http_up(
            &format!("http://{}:{ADMIN_PORT}/healthz", ns.underlay()),
            Duration::from_secs(20),
        )
        .await?;
        self.edges.push(Edge {
            gsp: Some(gsp),
            ns,
            args,
        });
```

`Drop`: `if let Some(c) = &self.controller { dump(c); }` and `if let Some(g) = &e.gsp { dump(g); }`.

New methods:

```rust
    pub fn origin_ip(&self) -> Ipv4Addr {
        self.origin.as_ref().expect("start_origin first").ip
    }

    fn registry_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.controller_port)
    }

    /// The tunnel address the controller assigned to a registered proxy.
    pub async fn proxy_address(&self, name: &str) -> Result<String> {
        let body: serde_json::Value = reqwest::get(self.registry_url(&format!("/proxy-peers/{name}")))
            .await?
            .json()
            .await?;
        Ok(body["tunnel_address"]
            .as_str()
            .context("no tunnel_address in the proxy's registration")?
            .to_string())
    }

    /// When the controller last heard from a proxy (unix seconds), from the
    /// address table.
    pub async fn proxy_last_seen(&self, name: &str) -> Result<u64> {
        let table: serde_json::Value = reqwest::get(self.registry_url("/tunnel/addresses"))
            .await?
            .json()
            .await?;
        table["entries"]
            .as_array()
            .context("no entries in the address table")?
            .iter()
            .find(|e| e["role"] == "proxy" && e["name"] == name)
            .and_then(|e| e["last_seen"].as_u64())
            .with_context(|| format!("proxy {name:?} is not in the address table"))
    }

    /// Kill the controller and wait until its port refuses connections.
    pub async fn stop_controller(&mut self) -> Result<()> {
        self.controller = None;
        anyhow::ensure!(
            port_is_down(self.controller_port, Duration::from_secs(10)).await,
            "the controller's port did not close"
        );
        Ok(())
    }

    /// Start the controller again on the same port and data directory. `sled`
    /// releases its file lock on a background thread after a kill, so the first
    /// attempts can fail: retry.
    pub async fn start_controller(&mut self) -> Result<()> {
        let data = self.dir.path().join("controller");
        let listen = format!("0.0.0.0:{}", self.controller_port);
        let mut last = None;
        for _ in 0..20 {
            let ctl = spawn_controller_with(&data, &listen, &controller_args())?;
            match wait_http_up(
                &self.registry_url("/healthz"),
                Duration::from_secs(3),
            )
            .await
            {
                Ok(()) => {
                    self.controller = Some(ctl);
                    return Ok(());
                }
                Err(e) => {
                    last = Some(e);
                    drop(ctl);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
        Err(last.expect("at least one attempt ran")).context("restarting the controller")
    }

    /// Kill an edge's `gsp` and start it again with the same arguments in the
    /// same namespace. A killed kernel-backend `gsp` leaves its WireGuard
    /// interface behind (and a killed boringtun its socket), so both are
    /// removed first — what a supervisor or an operator would do.
    pub async fn restart_edge(&mut self, idx: usize) -> Result<()> {
        self.edges[idx].gsp = None;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = self.edges[idx].ns.run(&["ip", "link", "del", "gsp-tunnel0"]);
        let _ = std::fs::remove_file("/run/wireguard/gsp-tunnel0.sock");
        let gsp = Proc::spawn_in(Some(&self.edges[idx].ns), "gsp", &self.edges[idx].args)?;
        self.edges[idx].gsp = Some(gsp);
        wait_http_up(
            &format!(
                "http://{}:{ADMIN_PORT}/healthz",
                self.edges[idx].ns.underlay()
            ),
            Duration::from_secs(20),
        )
        .await
    }

    /// Like [`TunnelLab::wait_roundtrip`] with a 90 s deadline on both
    /// backends: after a restart WireGuard re-handshakes through the agent's
    /// 25 s persistent keepalive, which the kernel backend's usual 30 s
    /// deadline is too short for (measured 2026-10-02).
    pub async fn wait_roundtrip_after_restart(&self, edge: usize) -> Result<()> {
        let addr = self.public_addr(edge);
        wait_until(
            || async move { Ok(tcp_roundtrip(addr, b"ping").await.is_ok()) },
            Duration::from_secs(90),
            "a TCP round trip through the tunnel after the restart",
        )
        .await
    }

    /// Start a second agent that pins `address_cidr` and expect the controller
    /// to refuse it: returns the agent's log once it has exited non-zero.
    pub async fn agent_refused(&mut self, name: &str, address_cidr: &str) -> Result<String> {
        let ns = self.lab.add_ns()?;
        let data = self.dir.path().join(format!("agent-{name}"));
        let mut args: Vec<String> = [
            "--data-dir",
            data.to_str().unwrap(),
            "--controller-url",
            &self.controller_url(&ns),
            "--name",
            name,
            "--iface",
            "gsp-agent1",
            "--listen-port",
            &WG_PORT.to_string(),
            "--address",
            address_cidr,
            "--backends",
            &format!(":{ECHO_PORT}"),
            "--register-interval-sec",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.extend(self.backend.agent_flag().map(str::to_string));
        let mut agent = Proc::spawn_in(Some(&ns), "gsp-agent", &args)?;
        wait_until(
            || {
                let code = agent.exit_code();
                async move { Ok(code.is_some_and(|c| c != 0)) }
            },
            Duration::from_secs(40),
            "the refused agent to exit non-zero",
        )
        .await?;
        Ok(agent.log())
    }
```

- [x] **Step 3: Update the Makefile**

In `tunnel-e2e` remove `--skip known_bug_` from the test arguments. In `tunnel-e2e-ci` remove `-E 'not test(known_bug_)'` from the `--no-run` line and `-E "not test(known_bug_)"` from the in-namespace run line. Also update the comment above `tunnel-e2e` if it mentions skipping the known bug.

- [x] **Step 4: Run the scenarios — the whole tunnel suite on both backends**

```bash
cargo build -p gsp -p gsp-agent -p gsp-controller -p gsp-aggregator -p gsp-ui
TUNNEL_BACKEND=kernel make tunnel-e2e-ci 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' | grep -E "^\s+(PASS|FAIL)|Summary"
TUNNEL_BACKEND=userspace make tunnel-e2e-ci 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' | grep -E "^\s+(PASS|FAIL)|Summary"
```
Expected: `Summary … 12 tests run: 12 passed` on each backend, including `two_proxies_share_one_origin` (the old known bug, now fixed), `a_pinned_address_collision_is_refused`, `an_edge_restart_keeps_its_address` and `an_edge_restarts_with_the_controller_down`. If a restart scenario fails, read the printed process logs (`TunnelLab` dumps them on failure) before changing anything: the usual causes are a leftover interface (`ip link del`), the 25 s keepalive recovery window, or the controller's sled lock (`start_controller` retries).

Also run the plain-`cargo test` path once: `TUNNEL_BACKEND=userspace make tunnel-e2e 2>&1 | tail -5` → `test result: ok. 12 passed`.

- [x] **Step 5: Verify and commit**

```bash
cargo fmt --all && make check
git add -A crates/gsp-fleet-tests Makefile
git commit -m "test(fleet): tunnel e2e on controller-allocated addresses; the multi-proxy bug is fixed

Un-ignores two_proxies_share_one_origin (was known_bug_…), drops --skip
known_bug_, and adds pinned-collision, sticky-restart and controller-down
restart scenarios.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

---

### Task 8: Deploy examples, documentation, final verification

**Files:**
- Modify: `deploy/compose/compose.tunnel.yml`, `deploy/lint.sh`, `deploy/README.md`
- Modify: `docs/11-backend-transport.md`, `docs/12-deployment.md`, `docs/08-roadmap.md`, `README.md`, `AGENTS.md`, `HANDOVER.md`
- Modify: `docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md` (status line)

**Interfaces:**
- Consumes: everything above. Produces no code interfaces.

- [x] **Step 1: Write the failing lint checks**

In `deploy/lint.sh`'s ruby block, before the final `if errs.empty?`, add:

```ruby
# The tunnel override must use the address authority, not hand-picked addresses.
ctl_cmd = tunnel["controller"]["command"]
errs << "tunnel override: controller needs --tunnel-network=10.60.0.0/16" unless ctl_cmd.include?("--tunnel-network=10.60.0.0/16")
errs << "tunnel override: controller lost the base flags" unless %w[--listen=0.0.0.0:9901 --data-dir=/data].all? { |f| ctl_cmd.include?(f) } && ctl_cmd.any? { |a| a.start_with?("--auth-token=") }
errs << "tunnel override: gsp must not hand-pick --tunnel-address" if tunnel["gsp"]["command"].any? { |a| a.start_with?("--tunnel-address") }
agent_cmd = tunnel["agent"]["command"]
errs << "tunnel override: agent must not hand-pick --address" if agent_cmd.any? { |a| a.start_with?("--address") }
errs << "tunnel override: agent backends should use the :port shorthand" unless agent_cmd.include?("--backends=:25565")
```

Run: `sh deploy/lint.sh`
Expected: `LINT FAIL:` for the controller flag, the gsp `--tunnel-address`, the agent `--address` and the backends shorthand.

- [x] **Step 2: Update the compose tunnel override**

Replace `deploy/compose/compose.tunnel.yml` with:

```yaml
# Adds the phase-14 WireGuard transport (docs/11, docs/12). Needs NET_ADMIN and
# /dev/net/tun, and root in the container (a capability is useless to uid
# 65532). The controller allocates the tunnel addresses (--tunnel-network):
# nothing here hand-picks one. Usage (from deploy/compose):
#   docker compose -f docker-compose.yml -f compose.tunnel.yml up -d
# Not smoke-tested in CI (no privileged runner); `make tunnel-e2e` covers the logic.
services:
  controller:
    command:
      - --listen=0.0.0.0:9901
      - --data-dir=/data
      - --auth-token=${GSP_CONTROLLER_TOKEN:?set GSP_CONTROLLER_TOKEN in .env}
      - --tunnel-network=10.60.0.0/16
  gsp:
    user: "0"          # NET_ADMIN is only in the bounding set for non-root: run as root
    cap_add: [NET_ADMIN]
    devices: ["/dev/net/tun:/dev/net/tun"]
    volumes: ["gsp-tunnel-data:/data"]
    command:
      - --controller=http://127.0.0.1:9901
      - --controller-token=${GSP_CONTROLLER_TOKEN}
      - --aggregator=http://127.0.0.1:9902
      - --aggregator-token=${GSP_AGGREGATOR_TOKEN}
      - --tunnel-iface=gsp-tun0
      - --tunnel-key-file=/data/gsp-tunnel.key
      - --tunnel-controller-url=http://127.0.0.1:9901
      - --tunnel-controller-token=${GSP_CONTROLLER_TOKEN}
      - --tunnel-name=demo-proxy
      - --tunnel-endpoint=${GSP_TUNNEL_ENDPOINT:?public ip:port origins dial}
      - --aggregator-instance=demo-gsp
  # An origin's agent normally runs next to the game server, on another host;
  # it is here only to show the flags, so it is opt-in:
  #   docker compose -f docker-compose.yml -f compose.tunnel.yml --profile origin-demo up -d
  agent:
    profiles: [origin-demo]
    user: "0"
    build:
      context: ../..
      dockerfile: deploy/Dockerfile
      args:
        BIN_SOURCE: ${BIN_SOURCE:-builder}
      target: gsp-agent
    image: gsp-deploy/gsp-agent:local
    network_mode: host
    cap_add: [NET_ADMIN]
    devices: ["/dev/net/tun:/dev/net/tun"]
    volumes: ["agent-data:/data"]
    command:
      - --data-dir=/data
      - --controller-url=http://127.0.0.1:9901
      - --controller-token=${GSP_CONTROLLER_TOKEN}
      - --name=demo-origin
      - --backends=:25565
      - --listen-port=51821   # gsp already owns UDP 51820 on this shared host network
volumes:
  gsp-tunnel-data:
  agent-data:
```

In `deploy/README.md`'s "Tunnel (phase 14)" section add one sentence: the controller allocates tunnel addresses from `--tunnel-network` (`10.60.0.0/16` here), so no `--address` / `--tunnel-address` is given; pin one only to keep a specific address (see docs/11 "Address authority").

- [x] **Step 3: Run the lint to verify it passes**

Run: `sh deploy/lint.sh && BIN_SOURCE=prebuilt sh deploy/lint.sh`
Expected: `deploy lint: ok` twice.

- [x] **Step 4: Update the documentation**

`docs/11-backend-transport.md`: replace the "Tunnel-internal address collision/exhaustion at fleet scale" bullet under "Open questions" with `- **Tunnel-internal address collision/exhaustion** — resolved 2026-10-02: see "Address authority" below.`, and add this section immediately before "## Open questions":

```markdown
## Address authority (built 2026-10-02)

`gsp-controller` allocates tunnel-internal addresses, so no operator chooses (or
mis-chooses) one. Design and decisions:
[`docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`](../specs/2026-10-02-tunnel-address-authority-design.md).

- **Allocation.** Start the controller with `--tunnel-network 10.60.0.0/16` (IPv4,
  `/30` or shorter). A registration that omits its address is allocated the lowest
  free host address; the allocation is sticky per `(role, name)`. A registration may
  instead **pin** an address, which is granted only if free (`409` otherwise). One
  global address space is shared by origins and proxies. Without `--tunnel-network`
  the controller is pin-only: it enforces uniqueness but allocates nothing.
- **Startup.** `gsp-agent` and `gsp --tunnel-*` register *before* bringing their
  interface up (the answer is its address), persist the answer next to their key, and
  can start from it while the controller is down. A later, different answer is logged
  as an error and not applied until a restart.
- **Routing.** Every peer is a `/32`: origins route each proxy's tunnel address
  (this fixed the earlier `AllowedIPs 0.0.0.0/0` bug where a second proxy stole the
  first one's route), proxies route each origin's. Backends must be on the
  registrant's own address; `--backends :25565` means "my address, port 25565".
- **Release.** `DELETE /peers/{name}` / `DELETE /proxy-peers/{name}` free the address
  and emit a tombstone that subscribers turn into a WireGuard peer removal.
  `GET /tunnel/addresses` lists the table with a `stale` flag
  (`--tunnel-stale-after`, default 14 days); nothing is freed automatically.
- **Limits.** Not combinable with `--ha-peers`; IPv6, lease expiry, a UI view and
  live address changes are future work (HANDOVER "Known follow-ups").
```

Then grep `docs/11`, `docs/12`, `docs/08` and `README.md` for `--address`, `--tunnel-address`, `--backends`, `0.0.0.0/0` and `known bug` (`grep -n -- '--address\|--tunnel-address\|--backends\|0\.0\.0\.0/0\|known.bug' docs/11-backend-transport.md docs/12-deployment.md docs/08-roadmap.md README.md`) and update each hit to the new behaviour (addresses optional/allocated; `/32` peers). In `docs/08-roadmap.md` change the "Phase 14 follow-ups" bullet to record that the multi-proxy fix and the address authority landed.

`AGENTS.md`: under `gsp-controller/` add `addresses.rs` ("tunnel address book: allocation, pinning, release; shared by both peer registries") and `addresses/api.rs`; under `gsp-agent/` add `address_store.rs`; under `gsp/` add `tunnel_address.rs`; in the Commands table's `make tunnel-e2e` row drop any mention of skipping a known bug.

`HANDOVER.md`: (a) delete the "**KNOWN BUG — a second proxy steals the first one's route**" paragraph; (b) delete the "**Tunnel address authority** (phase 14, `docs/11` "Open questions")" bullet under "Deferred / not built"; (c) in "Current state"/"Remaining work" remove tunnel address authority from the remaining list and add a "most recent landings" bullet for it; (d) in the "Known follow-ups" row "Tunnel address authority — deferred pieces", change "Designed alongside item 3 (…, once written)" to "Spec: `docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`"; (e) fix the tunnel e2e note that says the test is skipped by `make tunnel-e2e`; (f) update "Last updated".

`README.md`: add a clause to the status block noting that tunnel addresses are controller-allocated.

Spec: change its `Status:` line to `implemented (slices 1–6, 2026-10-02)`.

- [x] **Step 5: Final verification (superpowers:verification-before-completion)**

```bash
cargo fmt --all
make check                                       # fmt + clippy -D warnings + all tests: exit 0
sh deploy/lint.sh && BIN_SOURCE=prebuilt sh deploy/lint.sh
sh .github/scripts/changes_test.sh && python3 .github/scripts/test_summary_test.py
TUNNEL_BACKEND=kernel make tunnel-e2e-ci          # 12 passed
TUNNEL_BACKEND=userspace make tunnel-e2e-ci       # 12 passed
git status --short                                # nothing unexpected
```
Report the actual outputs. Do not claim the e2e passed without having run both backends.

- [x] **Step 6: Commit**

```bash
git add -A deploy docs README.md AGENTS.md HANDOVER.md
git commit -m "docs(deploy): tunnel address authority — examples, docs/11 section, handover

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

Do not push; ask the owner. After pushing, the CI `tunnel` job (both backends) and `deploy` job are the first runs on GitHub.
