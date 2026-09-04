# HANDOVER

State of the work, how to pick it up, and the traps.
Last updated: 2026-09-04.

Design is the source of truth in [`docs/`](docs/); locked decisions are the ADR
table in [`docs/09-technology-choices.md`](docs/09-technology-choices.md). This
file is the *current-state + gotchas* layer on top of that — per-phase
implementation narration lives in git history and `docs/08`, not here.

---

## Where the work is

**Roadmap phases 0–9 are complete**, plus two follow-on passes on `main`:

- **Data-plane completion** — ClientHello/first-bytes reassembly across TCP
  segments; `RouteHint.reject` hard drop; per-resolver `target` connect/idle
  timeouts; `weighted` balancer (pool `weights: { "ip:port": N }`); per-plugin
  sniffer config (`settings.sniffers.modules[].config` + the widened
  `sniff(in_ptr, in_len, cfg_ptr, cfg_len)` ABI — ADR 16a); live reload of
  `resolvers:` and `backend_sources:` (both registries `ArcSwap`-backed,
  reconciled by the reload task); `GET /sessions` live registry; first
  HTTP-level admin API tests.
- **Perf pass** — `splice(2)` zero-copy TCP pump (ADR 17); `recvmmsg(2)` UDP
  ingress batching (ADR 18); UDP idle expiry via a single-level timing wheel
  (ADR 19). Still deferred: `sendmmsg` UDP egress batching.
- **Listener port-range bind (F1.4)** — `bind: "host:lo-hi"` (e.g.
  `"0.0.0.0:30000-30999"`) spawns one real socket per port (× `workers`,
  `SO_REUSEPORT`-shared) under one listener config, sharing its
  routes/filters/pool selection; a route's `port` matcher still sees the real
  accepted/received port. `gsp_config::ListenerConfig::bind` is the primary
  (lowest) port, `extra_binds: Vec<SocketAddr>` the rest (empty for a plain
  bind); `ListenerConfig::binds()` iterates both. Capped at 1024 ports/range
  (`MAX_BIND_RANGE`); mutually exclusive with `prefix`. Was requirement F1.4
  in `docs/01-requirements.md`, written down at project start and never
  carried into a phase or deferred-work note until a documentation audit
  caught the gap — closed out right after, see `docs/08` Phase 3.
- **Build/fd metrics** — `gsp_build_info{version,commit}` (gauge, set once at
  startup; `commit` baked in by `crates/gsp/build.rs` via `git rev-parse`),
  `gsp_fd_open` (sampled every 5 s by `crates/gsp/src/procinfo.rs`, a small
  detached background task, `/proc/self/fd` on Linux) and `gsp_fd_limit`
  (`RLIMIT_NOFILE` soft limit via `nix::sys::resource::getrlimit`, sampled
  once — `nix` gained the `resource` feature, now also a direct `gsp`-binary
  dep, not just `gsp-core`'s). Lives in the binary, not `gsp-core` — host/ops
  observability, no data-plane seam needed. See `docs/06`.

**Update (2026-09-04, later same day)**: phase 10+11 (the distributed
control plane's single-tier PoC) is now **implemented, not just designed** —
see the session notes immediately below for the full arc. Three new crates
(`gsp-controller`, `gsp-aggregator`, `gsp-ui`) plus a frontend
(`crates/gsp-ui/web/`), all 11 slices done and individually verified live
against real running processes, up through slice 11 (the admin GUI). Only
slice 12 (integration tests spinning up the whole fleet together) and slice
13 (docs polish) remain before phase 10+11 is fully closed out. Phase 12
(fleet hierarchy/HA/shared intent) and phase 13 (regional health fabric) are
still **design only** — see
[`docs/10-distributed-control-plane.md`](docs/10-distributed-control-plane.md).

**2026-09-04 design session**: `docs/10` gained a "Fleet topology" section —
the controller and the new aggregator are each a recursive tree of tiers (one
per failure domain, down to a single instance), not a single global service.
Control (Tier 1) pulls root→leaf with a static `standalone`/`slave` role per
tier (never inferred from connectivity — that would split-brain a partition);
the aggregator pushes leaf→root with a homogeneous schema at every hop
(chosen over pull specifically to avoid needing a network route down to every
proxy). Intra-tier HA (1 node vs. a Raft/etcd group) is an orthogonal setting
from the role. Adoption (flipping a `standalone` tier to `slave` post-install
via the admin UI) is noted but explicitly deferred.

**Locked scope for the first release** (`docs/08` phase 10+11, now merged
into one PoC-sized phase): one `standalone` controller + one aggregator,
`replicas: 1` each, no `slave` role, no HA, no adoption. Operator intent
(backend overlay/admin-state, route hints) **stays per-instance** as it is
today — only structural config moves into the controller for this release;
moving intent into the controller's revision log is phase 12. Tier-1 store is
an embedded `sled` KV, single node (ADR 20, `docs/09`) — the user explicitly
asked for a proper KV over pushing config files around. `docs/08`'s phase
10+11 section has the full 13-slice implementation plan (controller slices
1–5, aggregator slices 6–10, UI/tests/docs 11–13); phase 12 carries the
deferred hierarchy/HA/intent-migration work forward as its own phase.

**Admin GUI is served only by the root tier** (same session, added after the
above): a GUI per region would need per-region auth/RBAC kept consistent —
exactly what the hierarchy exists to avoid. Every `slave` tier stays a
machine-to-machine API, reachable directly as break-glass but never as a
served web frontend.

**Slice 1 done** (2026-09-04, new crate `crates/gsp-controller`, lib
`gsp_controller` + bin `gsp-controller`): `store::Store` wraps two `sled`
trees (`revisions`, `meta`) — `open`, `current_revision`, `get`, `current`,
`put` (assigns the next monotonic revision, writes it + the `current` pointer
in one `sled` transaction, then flushes). 4 unit tests incl. a
reopen-persists check. The binary opens the store and serves `GET /healthz`
only — `POST /config` (slice 2) and the subscribe/change-stream endpoint
(slice 3) aren't built yet. Workspace gained `sled` + `tempfile` (dev-dep) in
the root `Cargo.toml`. `make check` (fmt + clippy `-D warnings` + `cargo test
--all`) passes with the new crate in the workspace.

**Slice 2 done**: `crates/gsp-controller/src/api.rs` — `POST /config` runs
the same `gsp_config::parse_str` (parse + `validate()`) a proxy runs on a
file reload; a rejected submission (`422`, JSON error body) never touches
`Store::put`, so the current revision stays whatever it was — same
bad-reload-keeps-the-old-snapshot rule as the proxy, one hop earlier.
`GET /config` returns the current revision's raw text with an
`X-Config-Revision` header (`404` before any submission). Verified against
the real `config.example.yaml`. 3 new tests (7 total in the crate).
`gsp-config` is now a `gsp-controller` dependency; `tower` added as a
gsp-controller dev-dependency for the in-module `axum` router tests (mirrors
`gsp`'s own in-module admin HTTP tests).

**Slice 3 done**: `GET /config/subscribe?since=<revision>` (SSE) on
`gsp-controller` — catch-up range from `Store::revisions_after` then a live
tail off a `broadcast::Sender<u64>` `submit_config` feeds; a lagging
subscriber just re-runs the catch-up query, so `Store` (never forgets a
revision) is the only source of truth, no delivery state on the writer side.
5 new tests incl. a forced-lag one. `gsp` gained `--controller <url>`
(`conflicts_with` `--config`); `main.rs` now loads its first config (file or
`controller_client::fetch_current`'s `GET /config`) inside `block_on`, since
the controller path needs an async HTTP call before anything else exists.
`reload::apply` split into itself (file read) + a new `pub(crate)
apply_config` (validated-`Config` → rebuild/reconcile) so both the file
reload and `controller_client::run`'s pushed revisions share one pipeline.
`controller_client::run` holds the SSE connection, reconnects with capped
exponential backoff (500 ms → 30 s) from the last-*applied* cursor (not
where the connection started — a bug caught by the live smoke test below and
fixed: the cursor is now threaded through by `&mut` so a mid-stream error
doesn't roll it back and force a pointless replay). An invalid pushed
revision is logged, skipped, and still advances the cursor (must not replay
forever). Verified live end-to-end (not just unit tests): started
`gsp-controller`, submitted `config.example.yaml`, started
`gsp --controller <url>` and confirmed it came up and served `/healthz`,
pushed a second revision and watched `gsp` log
`configuration reloaded source=controller revision 2`, then killed the
controller and confirmed `gsp` kept running on its last config while
retrying with growing backoff (`cursor=2` correctly, post-fix). Workspace
gained `tokio-stream` (gsp-controller's SSE stream wrapper) and reqwest's
`stream` feature (gsp's `Response::chunk()`).

**Slice 5 done — all 5 controller slices complete.** `gsp-controller` gained:
`GET /config/revisions` (history: revision/size/`current`), `GET
/config/revisions/{revision}` (past revision's raw text), `GET
/config/revisions/{revision}/diff[?against=<revision>]` (a `similar`-crate
line diff against `current` or another revision — added `similar` as a
workspace dep), `POST /config/rollback/{revision}`. Rollback **never
rewrites history**: it re-submits the old revision's exact bytes through the
same validate-then-`Store::put` path `POST /config` already uses (a shared
`submit()` helper both call), so a subscriber just sees an ordinary new
revision — no special-casing needed anywhere downstream. New `auth.rs`:
`--auth-token <token>` on `gsp-controller` gates the whole `/config*`
router (via an `axum::middleware::from_fn_with_state` layer) with a bearer
check; `/healthz` stays outside that router so it's never gated, matching
plain liveness-probe convention. It's a single shared secret (`AppState`
carries `Option<Arc<str>>`), not RBAC — correctly scoped for this release's
one-controller PoC, not meant to be more. 8 new tests (19 total in the
crate). Verified live end-to-end over real HTTP against
`config.example.yaml`: unauthenticated request → `401`; submitted two
revisions; `GET /config/revisions` showed both with correct sizes and
`current` on the second; diffed revision 1 against current and got exactly
the changed `max_connections` line; rolled back to revision 1 and confirmed
`GET /config`'s `X-Config-Revision` became `3` (a new revision, not a
rewind) with revision 1's exact content.

**All 5 controller slices (1–5) are now done.**

**Slice 6 done**: new crate `crates/gsp-aggregator` (lib `gsp_aggregator` +
bin `gsp-aggregator`) — deliberately **no `gsp-core`/`gsp-config`
dependency**, the aggregator stays fully decoupled from the data-plane
crates (unlike `gsp-controller`, which needs `gsp-config::parse_str` to
validate submissions). `ingest::IngestStore` is a plain in-memory
`RwLock<HashMap<instance, InstanceState>>`, latest-write-wins, **never
persisted** — that's the actual design, not a shortcut: every fact the
aggregator holds is a proxy's own state, re-pushed on the next tick, so
restarting it loses nothing durable (see the "stateless and ephemeral by
design" note in the crate's `lib.rs`). `IngestPayload` is a summary — pool/
backend health + admin state, session *counts* — not the full live session
registry (`GET /sessions` already exists per-instance for that,
break-glass style). `POST /ingest` validates only that `instance` is
non-empty (`400`); a malformed body is rejected by axum's `Json` extractor
itself (`422` for valid-JSON-wrong-shape, `400` for invalid JSON syntax —
verified both, plus the happy path and an overwrite, live over real HTTP).
12 new tests (4 store, 8 API). `GET /healthz` served the same way as
`gsp`/`gsp-controller`.

CLAUDE.md's repository-layout listing gained `gsp-controller/` and
`gsp-aggregator/` entries — a gap from slice 1, caught and fixed now.

**Slice 7 done**: `crates/gsp/src/aggregator_client.rs` — `--aggregator
<url>` (+ `--aggregator-instance`, `--aggregator-interval-sec`, default 10s),
independent of `--controller`. Builds an `IngestPayload` (the wire shape is
duplicated here, not a `gsp-aggregator` dependency — same reasoning as
`controller_client`'s hand-parsed SSE JSON) straight from the live
`RuntimeHandle`: `snapshot().pools` for pool/backend health+state,
`sessions()` filtered by `gsp_core::Proto` for TCP/UDP counts. Posts on a
fixed interval via `reqwest::Client`. **Deliberately no retry buffer** — the
roadmap's original "ring buffer" wording predates settling on `IngestStore`
being latest-write-wins state rather than an event log; buffering old
snapshots would let a stale replay overwrite a fresher push that already
landed, so a failed send just logs (`Debug`-formatted — the raw
`reqwest::Error` Display is too terse to debug from) and is superseded by
the next tick. 1 new test. Verified live end-to-end over real HTTP.

**Debugging note for next time**: an initial live test looked like every
push was failing (`ConnectionRefused`). Root cause was the test harness, not
the code — `config.example.yaml`'s `shutdown_grace_sec: 30` means a plain
`timeout N gsp` doesn't kill it at `N` (SIGTERM triggers a graceful drain
that can run up to 30s), so mismatched nested `timeout` durations let the
aggregator die before `gsp`'s later push attempts. Fixed by using `timeout
-s KILL` and giving the aggregator a longer lifetime than the proxy in the
test script. Also: `pkill`/`kill` against background test processes
intermittently produced a bare "Exit code 144" from the Bash tool in this
session with no other output — switching to `timeout -s KILL <cmd>` wrapping
each process (no manual `kill`/`pkill` afterward) avoided it. Worth trying
first if a future smoke test's process cleanup misbehaves the same way.

**Slice 8 done**: `GET /fleet/pools` / `/fleet/sessions` / `/fleet/healthz`
on `gsp-aggregator`, all served straight from `IngestStore` (no fan-out —
data already arrived), each entry carrying `last_seen_ms_ago`.
`/fleet/healthz` flags `stale` past `STALE_AFTER_MS` (30s, ~3x `gsp`'s
default 10s push interval) — the first place "stale" gets an actual
threshold, per the note left in `ingest.rs` at slice 6. Added a test-only
`IngestStore::insert_state` seam (`#[cfg(test)]`) so the staleness path is
tested deterministically instead of needing a real 30s sleep. **Deliberately
skipped `GET /fleet/config`**: `IngestPayload` carries state, not config
content — the controller already owns config via `GET /config`/
`/config/revisions`; a "which revision is each instance running" view would
need `controller_client` and `aggregator_client` to share state inside `gsp`
that's deliberately independent today (slice 7's "unrelated axes"), so it's
deferred as a documented follow-up (an optional `config_revision` field), not
silently dropped. 8 new tests. Verified live end-to-end over real HTTP.

Along the way: the live smoke test's `/fleet/sessions` showed 28,230 UDP
sessions after ~4s against `config.example.yaml`, which was alarming until
traced to a pre-existing, unrelated fact about that file — its `realtime`
pool's targets (`127.0.0.1:27015`/`27016`) are the *same addresses* its own
listeners bind, so actually running it live (not just parsing it, which is
all CI/`--check` ever do) lets the `udp_probe` health check loop back
through the proxy's own listener as a fake client, snowballing session
counts. Confirmed unrelated to this session's work by re-running against a
minimal non-self-referential config (`active: 0`, as expected) — logged in
"Known follow-ups" below, not fixed (out of scope here, and
`config.example.yaml`'s job is documenting syntax, not being a runnable
fixture).

**Slice 9 done**: `crates/gsp-aggregator/src/fanout.rs` — thin, stateless
intent-verb fan-out. `IngestPayload` gained `admin_url` (self-reported by
`gsp`, always `http://{settings.admin.listen}` — the only "backend registry"
fan-out needs, no separate discovery). Two shapes: **targeted**
(`POST /fleet/instances/{instance}/drain`|`undrain`, pass-through response,
`404` unknown instance) and **broadcast** (backend add/patch/delete,
route-hint — every known instance via a `tokio::task::JoinSet`, per-instance
results, one bad instance never fails the rest). 9 new tests against real
mock instance HTTP servers (not mocked responses — actual `axum::serve`
listeners on ephemeral ports), including one using a real `Json` extractor
specifically to catch content-negotiation issues.

**A real bug found and fixed via the live smoke test**: `curl -X POST
.../fleet/pools/local/backends -d '{"addr":"..."}'` (no `-H content-type`)
came back `{"results":[{"status":415,...}]}` — the broadcast forwarded the
caller's raw body but never set `Content-Type`, so a caller (or a client
library) that omits it gets rejected by the target's own `Json` extractor
even though the body is perfectly valid JSON. Fixed by setting
`application/json` explicitly on every forwarded body in both `broadcast`
and `proxy_to_instance` — these endpoints are always JSON per the phase-5
admin API contract, so the aggregator shouldn't make every caller remember a
header it already knows the answer to. Added a regression test using a real
`axum::extract::Json` handler (not the raw-`Bytes` mock the other fan-out
tests use) to actually exercise that path, since the original bug would have
passed silently against a `Bytes`-only mock.

Verified live end-to-end over real HTTP, against real `gsp`/`gsp-aggregator`
processes: `POST /fleet/instances/demo-2/drain` → `200`, then `GET
127.0.0.1:19961/readyz` on the real instance showed `draining` / `503`;
`POST /fleet/pools/local/backends` (broadcast) → landed on the instance's
real `GET /pools` output.

Also confirmed, not caused by this work: `gsp-core`'s
`resolver::tests::resolver_target_gets_a_proxy_protocol_header` failed once
under full-workspace parallel `cargo test --all` load, passed immediately
both in isolation and on a full-suite rerun — pre-existing timing-sensitive
flakiness, not a regression (this session never touched `gsp-core`). Worth
knowing if it reappears, not worth chasing now.

**Slice 10 done — all 10 controller+aggregator slices complete.** Three
independent bearer-token secrets for three independent hops (deliberately
never one token threaded through everything):

- `gsp-config` gained `settings.admin.auth_token` (raw + resolved `Config`,
  `config.example.yaml`, `docs/05` "Admin API auth" note — the target schema
  there sketches an `auth: {mode, token}` object we don't implement; ours is
  a flat string, called out as a documented divergence). `gsp/src/admin.rs`
  gained a `require_bearer` middleware gating everything except `GET
  /healthz` (split the router into a `route_layer`-gated group + an
  unlayered `/healthz`, merged before `.with_state`).
- `gsp-aggregator --auth-token` gates its own API the same way (new
  `auth.rs`, mirrors the controller's exactly); `AppState` gained
  `auth_token` + `with_auth_token()`.
- `gsp-aggregator --instance-token` is the *separate* secret it presents
  going *out* to every instance's admin API in `fanout.rs` (`AppState`
  gained `instance_token` + `with_instance_token()`) — matches against that
  instance's own `settings.admin.auth_token`.
- `gsp --aggregator-token` is what a proxy presents pushing to `/ingest`
  (`aggregator_client::PushConfig` gained `token`).

3 new unit tests (admin auth gate, aggregator auth gate, `gsp-config`
parsing `auth_token`) plus a full live end-to-end run through every hop at
once: unauthenticated `GET /fleet/pools` → `401`; authenticated → the real
pushed data; unauthenticated instance `GET /pools` → `401`; a fan-out drain
through the aggregator (which had to present `--instance-token` correctly)
→ `200`, confirmed real by the instance's own `/readyz` flipping to `503`.

**Slice 11 redesigned before writing code** (design session, not yet fully
built): the user pushed back twice, correctly. First: hosting the GUI's
session login in `gsp-aggregator` taxes it with a concern outside its design
(a session store *is* a form of authority; the aggregator is explicitly
designed to carry none), and slice 11f (config editing) would need the
aggregator to proxy into the controller, inverting the natural authority
direction. Then: rather than attach the GUI to whichever service turns out
more convenient (the controller, matching `docs/10`'s original words about
where auth/RBAC/audit belong), the user chose to build it right from the
start as **a fourth, dedicated `gsp-ui` process** — a pure BFF holding both
the controller's and the aggregator's own machine credentials, authorizing
nothing itself beyond "is this a valid session," no store, no fleet data of
its own. `docs/10` "The admin GUI" and `docs/08` slice 11 (now 11a–11f) were
rewritten to match; framework choice locked in: React + Vite + TypeScript,
single shared `--ui-password` (session cookie), a WebSocket for live updates
rather than the browser polling.

**Slice 11a done**: `gsp-aggregator` gained `GET /fleet/subscribe` — see
`docs/08` for the mechanics. Reused the controller's SSE pattern exactly (a
`broadcast::Sender` fired by `POST /ingest`, a spawned worker doing
send-then-tail, turned into `Event`s by the route handler) with one addition
worth remembering if this pattern gets reused again: **debouncing**. A
revision log has no reason to debounce (each revision is distinct, wanted
individually); a *state* feed does — a burst of near-simultaneous pushes
across a fleet would otherwise trigger one rebuild+send per push instead of
one for the whole burst. `SUBSCRIBE_DEBOUNCE` (150 ms) plus draining any
further signals that land during it is the fix; tested directly (a 5-push
burst asserted to collapse into exactly one resend, with a second check that
nothing extra follows).

**Slice 11b done**: new crate `crates/gsp-ui` (lib `gsp_ui` + bin `gsp-ui`),
no `gsp-core`/`gsp-config` dependency. `session::SessionStore` (in-memory,
random 256-bit hex ids via `rand`, `HashSet<String>` — new workspace dep
`rand = "0.8"`). `api.rs`: `POST /ui/login` (`--ui-password`, `None` = open),
`POST /ui/logout`, `GET /ui/session` (gated). `auth.rs`'s `require_session`
mirrors the controller's/aggregator's `require_bearer` shape but checks a
session cookie, never a bearer token — the browser is never handed one.
Cookie handling is hand-rolled (parse `Cookie` header, build `Set-Cookie`
manually) rather than pulling in a cookie crate for one cookie, matching
this codebase's existing preference for hand-rolling simple well-understood
formats. One real mistake caught before committing: the first draft of the
router's test helper applied `require_session` to *every* route including
`/ui/login` itself — circular (you can't gain a session at a route that
requires one). Fixed by moving the gate/ungated split into `api::router`
itself (mirrors `gsp-controller`'s router shape, not `gsp-aggregator::
fanout`'s stateless-until-merged one, since here the split is per-route
within one module, not composed from a separate module). 9 new tests.
Verified live end-to-end over real HTTP through the whole cycle:
unauthenticated → `401`, wrong password → `401`, right password → a cookie
that unlocked the gated route, logout → `401` again.

**Slice 11c done**: new `crates/gsp-ui/src/aggregator_proxy.rs` — thin,
stateless proxy for fleet reads and the slice-9 operational verbs to
`gsp-aggregator` (`--aggregator-url`/`--aggregator-token`), mirroring
`gsp-aggregator::fanout`'s own shape (same "route definitions only, no
`.with_state()`" pattern, merged into `api::router`'s gated group). `503`
cleanly for "no aggregator configured," `502` cleanly for "configured but
unreachable" — both covered by tests, no panics. 6 new tests, including a
real mock-aggregator (`axum::serve` on an ephemeral port, not a mocked
response) proving the bearer token and request body both forward correctly.

Verified live end-to-end over the **real four-hop chain** — not per
component: `curl` (as the browser) → `gsp-ui` (session auth) → real
`gsp-aggregator` (bearer auth) → real `gsp`'s admin API (its own bearer
auth). Unauthenticated browser call → `401`; logged in → `GET
/api/fleet/pools` returned the real pushed fleet data; a drain issued
through the full chain landed for real, confirmed by the instance's own
`GET /readyz` flipping to `503`. This is the whole point of the `gsp-ui`
redesign proven working, not just each piece in isolation.

**Slice 11d done**: `crates/gsp-ui/src/fleet_feed.rs` (a single shared
subscription to the aggregator's `/fleet/subscribe`, fanned out via a
`broadcast::Sender<String>` plus a `latest` cache) + `src/ws.rs` (`GET
/ws/fleet`, gated by `require_session` same as everything else). Deliberate
design point: one aggregator SSE connection total, not one per browser tab —
`fleet_feed::run` is spawned once at startup and every WS connection just
subscribes to its internal broadcast. Reused `gsp`'s `controller_client`'s
hand-rolled SSE parsing verbatim (same reasoning: control-plane, human-paced,
not worth a dependency) and its reconnect-with-backoff shape. A `Lagged` WS
subscriber resends `latest` rather than trying to replay — consistent with
every other "this is state, not a log" spot in this codebase now
(`gsp-aggregator`'s `subscribe_fleet_worker`, `gsp-controller`'s
`subscribe_worker` handles an actual log so it's the one exception, correctly).

5 new tests, 3 of them against a *real* `tokio-tungstenite` client and a real
`axum::serve` listener (added `tokio-tungstenite` + `futures-util` as
`gsp-ui` dev-deps) — not mocks. One cleanup along the way: `FleetFeed::
set_latest` started private, but the WS tests needed to drive the cache
directly without a real aggregator connection, so it's `pub(crate)` with a
comment explaining why a test-only relaxation is fine here.

Verified live end-to-end over the **full five-hop chain** with a throwaway
`examples/ws_probe.rs` (written, used, then deleted — not part of the
crate): real `gsp` → real `gsp-aggregator` (SSE) → real `gsp-ui`
(`fleet_feed`) → a real WebSocket client, watching five live pushes arrive
in real time as `gsp` kept pushing on its 1s interval.

**Slice 11e done**: new `crates/gsp-ui/src/controller_proxy.rs`, same shape
as `aggregator_proxy` — proxies the controller's config API
(`--controller-url`/`--controller-token`). Notable difference from the
aggregator proxy: the controller's `POST /config` handler takes a plain
`String`, not JSON, so the body is forwarded byte-for-byte with no
content-type forced on it; the diff route also forwards the incoming query
string (`?against=`) via `axum::extract::RawQuery`.

**A real bug found and fixed by a test, not by inspection**: writing
`get_config_proxies_to_the_controller` (asserting `X-Config-Revision`
survived the `gsp-ui` hop) failed immediately — every proxy helper in this
fleet (`aggregator_proxy::proxy`, `controller_proxy::proxy`, and
`gsp-aggregator`'s own `fanout::proxy_to_instance`) was built passing only
`(status, body)` into `into_response()`, silently dropping every response
header from the upstream service. Fixed with a new
`gsp-ui/src/proxy_util.rs::forwardable_headers` (strips only
`connection`/`transfer-encoding`/`content-length` — the ones that describe
*this* hop's framing, not the content — forwards everything else verbatim),
applied to both `gsp-ui` proxies, plus the equivalent fix and a regression
test in `gsp-aggregator::fanout`. Worth remembering: **a thin pass-through
proxy needs an explicit test asserting a header survives the hop** —
status+body tests alone don't catch this class of bug, as proven here across
three independent call sites that all had it.

6 new tests. Verified live end-to-end over the full real chain: `gsp-ui` →
real `gsp-controller` — submit → `GET /api/config/revisions` → diff →
rollback → `GET /api/config` correctly showing `X-Config-Revision: 3` (the
post-rollback revision, not a rewind), header intact.

**Slice 11f done — all of slice 11 (11a-11f) is now complete.** New
standalone npm project `crates/gsp-ui/web/` (React + Vite + TypeScript, own
`package.json`, never a Cargo workspace member — `make ui` builds it, see
its own README). `gsp-ui` gained `--static-dir` (default
`crates/gsp-ui/web/dist`), served via `tower-http::services::ServeDir`
(new dep) as a fallback under whatever the API routes don't claim. No
client-side routing — one page, view state lives in React state.

Components: `Login` (session-cookie form), `FleetView` (live
instance/pool/backend table over the slice-11d WebSocket, drain/undrain,
backend add/patch/remove, route-hint), `ConfigView` (editor, submit,
revision history/diff/rollback). `src/api.ts` centralizes every `fetch`
call; the session cookie is the only credential the browser ever sends.
`npm install && npm run build` (`tsc -b && vite build`) verified clean —
TypeScript compiled with zero errors on the first real build.

**Verified live end-to-end with all four services running together for the
first time** — `gsp` + `gsp-controller` + `gsp-aggregator` + `gsp-ui`, with
the *actual built frontend* served: `GET /` returned real `index.html`
(`text/html`), a JS asset returned with the right content-type, login
worked, a config submission and a fleet-pools read went through the served
UI's own proxy paths, and a drain issued through the whole chain landed for
real (the instance's own `/readyz` flipped to `503`). This is the complete
picture the `gsp-ui` redesign was for, running as one coherent system, not
four services that happen to pass their own tests.

Housekeeping caught along the way: a manual smoke test earlier this session
left a stray `gsp-controller-data/` directory in the repo root (already
`.gitignore`d, see slice 11e's entry); this session's frontend work added
`crates/gsp-ui/web/node_modules` and `.../dist` to `.gitignore` too, so
neither ever gets committed by accident.

**Two more real bugs found and fixed while actually driving the fleet from a
browser** (not caught by any unit test, because neither is a Rust-side
contract violation — both are wire-shape assumptions the frontend got wrong):

1. **`gsp` had no way to authenticate to `gsp-controller` at all.** Every
   other cross-service link in this fleet has a token pairing
   (`--aggregator-token`/`--auth-token`, `--instance-token`/
   `settings.admin.auth_token`, `--controller-token` on `gsp-ui`) — but `gsp`
   pulling its own config from a controller had no `--controller-token`
   flag, so a `--auth-token`-protected controller would 401 every fetch and
   subscribe call. Fixed: `controller_client::fetch_current` and `::run`/
   `subscribe_once` now take an optional token and present it via
   `bearer_auth`; `gsp` gained `--controller-token`; `ConfigSource::
   Controller` carries the token alongside the URL so a reconnect keeps
   using it.
2. **The browser's login always failed with a generic "login failed."**
   `gsp-ui`'s `POST /ui/login`/`/ui/logout` return a plain-text `"ok"` body
   on success, but `crates/gsp-ui/web/src/api.ts`'s `login()`/`logout()`
   called `requestJson`, which unconditionally `JSON.parse()`s a successful
   response body — `JSON.parse("ok")` throws, and `Login.tsx`'s catch-all
   swallowed that into a misleading error message. The backend was correct
   the whole time (verified via `curl` before touching any frontend code).
   Fixed by switching both to `requestText`. Verified the *exact* failure
   with Node's `fetch` reproducing the old code path's throw, then verified
   the fix with the same script completing `login → GET /ui/session` →
   `{"authenticated":true}` cleanly.

Both were found by actually standing up the full four-process fleet
end-to-end for a human to click through — not by writing more unit tests
first. Worth the reminder: **wire-shape mismatches between a Rust JSON/text
handler and its TS caller are a real, recurring class of bug this project
has now hit twice** (see slice 11e's header-forwarding bug for the first),
and neither was visible from either side's own test suite in isolation —
only from driving the two together.

**Next**: slice 12 (integration tests spinning up N `gsp` + controller +
aggregator + `gsp-ui` together — subscribe/reconnect/freeze-on-disconnect,
push/ingest, fan-out partial failure, config reject-keeps-previous) and
slice 13 (docs polish: `docs/06`, `README.md` status block, `docs/08` status
legend). **Phase 10+11's entire controller + aggregator + UI implementation
is otherwise done** — slices 1-11 all complete and individually verified
live, now including an actual human driving the real UI in a real browser.

### Known follow-ups (none blocking)

| Item | Notes |
|------|-------|
| `config.example.yaml`'s `realtime` pool self-references its own listener ports | targets `127.0.0.1:27015`/`27016` == the listener binds of the same name; actually *running* this file live (not just `--check`/parsing it, which is all CI does) makes the `udp_probe` health check loop back through the proxy's own listener as if it were a client, growing `active`/session counts unbounded within seconds (observed: 28k+ after ~4s). Found via slice 8's live `/fleet/sessions` smoke test, confirmed unrelated to phase 10+11 by re-running against a minimal non-self-referential config (`active: 0`, as expected). Not a regression from this session — a pre-existing property of the example/documentation config when actually executed rather than just parsed. Not fixing now: out of scope for the aggregator work, and `config.example.yaml`'s job is to document every feature's syntax, not to be a runnable fixture.
| `sendmmsg` UDP egress batching | reply pump + upstream forward still one `send` per datagram; per-session reply buffers of `RECV_BATCH`×`MAX_DATAGRAM` would 16× RSS — needs a smaller batch buffer or per-datagram alloc, its own decision |
| Per-source cap + UDP sticky table: LRU eviction | both refuse / wholesale-clear when full today; acceptable defaults — do only if load testing shows them biting |
| k8s discovery watch informer | polling Endpoints now; a convergence-speed optimization, belongs with the fleet-phase discovery rework |
| Resolver `sticky_key` | deferred pending a design for how a later request recovers the key; overlaps the phase-11 intent model |
| `IPV6_TRANSPARENT` on musl / non-glibc | `set_ip_transparent` already calls `socket2` 0.6's `set_ip_transparent_v6` unconditionally — may already work; build + smoke-test on a musl target before writing code |
| Retire the UDP sticky table via `consistent_hash` | pure polish, no user-visible gap |
| Reload debounce only coalesces within one 200 ms window | wider-spaced events cause separate (idempotent) reloads; low priority |
| NFR N3/N4/N5/N9 (aggregate throughput, full 500k/1M, HA) | need dedicated hardware + multiple hosts + a real load generator; the `gsp-bench --mode concurrency` ramp already went as far as one box allows (~20k TCP / ~5k UDP verified here) |
| More sniffers (`quic`, `wireguard`, …), multiple sniffers per listener | community / plugin ecosystem; never blocks core work |

---

## Workflow gotcha: run `cargo fmt --all` as its own step before `make check`

CI once failed a `cargo fmt --all --check` even though `make check` had reportedly
passed locally, because `cargo fmt --all` was run early in the session and more
code was `Edit`ed in afterwards. `--check` (which `make check` runs) only
*reports* diffs and exits non-zero — it never rewrites the file. So:

**Immediately before every `make check` / commit, run `cargo fmt --all` (the
writing form, no `--check`) as its own step** — never assume an earlier fmt pass
covers later edits. Piping `make check` through `tail` also hides an early
`fmt-check` failure message; check the exit code or read from the top.

---

## Invariants that bite if you forget them

(Full list in [`CLAUDE.md`](CLAUDE.md) "Architecture invariants". These are the
ones with a subtlety.)

- **Exactly one place builds + stores a `Snapshot`: `reload::apply`.** Runtime
  edits (backend overlay, discovery refresh, admin) are just extra *rebuild
  triggers* — they mutate a persistence layer (`BackendOverlay`, `Discovery`)
  then call `request_reload()`. Backend *state* (`PATCH`) mutates atomics on the
  live `Backend` and needs no rebuild.
- **`Sniffers` and `Resolvers` registries are `ArcSwap`-backed** (unlike the
  snapshot, which every worker re-`load`s itself). Every worker clone points at
  the *same* instance, so `replace()` from the reload task is visible instantly
  with no replumbing.
- **Listeners are reconciled by name** (`ListenerManager::reconcile`), not
  rebuilt with the snapshot. An unchanged listener keeps running with its route
  rules captured at spawn; only the *pool contents* they resolve to are read
  live. A `ListenerConfig` change (bind, protocol, routes, affinity, `prefix`,
  `freebind`, `route_hint`, ACL, `rate_limit`, `geo`, `transparent`) stops and
  re-spawns it — a same-bind rebind is gapless via `SO_REUSEPORT`.
- **Startup-only config** (reload does *not* re-read): `workers`,
  `settings.limits`, `settings.geo_db`, `settings.sniffers` engine params
  (`call_timeout_ms` / `max_memory_bytes` — only the `dir` *contents* are live).
- **Zero `unsafe`.** `socket2` 0.6 (`SockRef` for `IP_TRANSPARENT` v4/v6,
  `IP_FREEBIND`), `nix` (`IP_PKTINFO` / `IP_ORIGDSTADDR` cmsgs, `recvmmsg`,
  `splice`) — all safe wrappers.
- **UDP amplification guard**: the proxy never sends to a client without an
  established session; a dropped datagram (routing / ACL / rate / gate) gets no
  reply. Covered by `crates/gsp-core/tests/amplification.rs`.

---

## Latency ledger

Per-connection / per-datagram cost of every feature. **If you add a
per-connection or per-datagram task, hop, or allocation, add it here.**

### Steady state (per byte / per datagram)

- **TCP pump**: on Linux, `splice(2)` `socket → pipe → socket` — no userspace
  copy, no 32 KiB×2 heap buffers (the pipe fds cost ~2 KiB kernel each). Up to 2
  extra syscalls per direction per 64 KiB chunk. Non-Linux / `pipe2` failure:
  buffered `try_read`/`try_write`, one 32 KiB buffer per direction. No lock.
- **UDP ingress**: a share of one `recvmmsg` (≤16 datagrams/syscall on Linux;
  fresh `MultiHeaders` amortised over the batch), one `HashMap` lookup by client
  `SocketAddr`, one relaxed atomic store (liveness), one `send` upstream. No
  lock, no alloc, no task spawn. Reply path is still one `recv` + one
  `send`/`send_to` per datagram (`sendmmsg` egress deferred).
- Recv buffers: `RECV_BATCH` (16) × 64 KB **per worker** (shared across that
  worker's sessions) + one 64 KB buffer per reply task.
- Established UDP sessions skip the ACL / rate-limit / geo / gate checks
  entirely — those run only for datagrams that miss the session table.

### Per new TCP connection / new UDP session (paid once)

- **Base**: 1 `Pool::acquire[_for]` (lock-free reads + one atomic add), 1
  upstream `connect`, 1 spawned pump/reply task, (UDP) 1 socket `bind`+`connect`
  + sticky-table + session-table insert.
- **Routing**: 1 `local_addr()` syscall + a linear scan of the small route list
  (bit-compare per `client_cidr`/`dst`, `u16` range per `port`, `starts_with` +
  len check per `first_bytes`, one `extract_sni` pass per `sni`).
- **Peek** (only if a route uses `first_bytes`/`sni`/`sniffer`): one `MSG_PEEK`
  into a `peek_len()`-sized `Vec` (`PEEK_MAX` = 4096 for `sni`/`sniffer`), 250 ms
  budget, + an `Arc<ListenerConfig>` clone into the per-conn task. Re-peeks every
  5 ms only if the first byte is a TLS record and the first record isn't whole.
- **`consistent_hash`**: `O(healthy)` — one `sort_by_key` with a `DefaultHasher`
  per backend over the healthy `Vec` every balancer already builds. Same work as
  `least_conn`. Process-stable hash only (fine: reload rebuilds pools, no
  cross-instance state).
- **`route_hint: true`**: one `ArcSwap::load` + `HashMap::get` (lock-free) + one
  `String` clone when a hint is present. Listeners without the flag pay nothing.
- **External resolver**: one `.await`ed HTTP/gRPC round-trip (`timeout_ms`,
  default 40 ms) on the routing path, in the spawned task — never the accept
  loop. With `cache:` a repeat key is a `Mutex<LruCache>` get instead. A
  `target` connection skips `Pool::acquire_for` entirely — cheaper than pooled.
- **Sniffer plugin** (`WasmSniffer`): one `Store::new` + `Instance::new` (fresh
  per call) + `memory.write` of the peeked bytes + one guest call + decode, all
  synchronous on the per-conn task. Bounded by `call_timeout_ms` (epoch
  interruption) + `max_memory_bytes`. **Benchmarked**: p50 8–10 µs, p99 12–26 µs
  for the three first-party plugins — comfortably inside NFR N1 (500 µs), so the
  fresh-`Store`-per-call design needs none of the `InstancePre` / pooling /
  warm-instance fallbacks held in reserve. Listeners without a `sniffer:` route
  pay one `HashMap::get`. See `docs/07` "Sniffer plugin sandbox guarantees".
- **Filter chain** — CIDR ACL: a bounded bit-walk of the `deny` (and, if
  non-empty, `allow`) radix trie, ≤32/128 hops, no alloc/lock. GeoIP: one
  MaxMind tree lookup + a small `Vec` scan. Rate limit / `per_source` / global
  caps: one `Mutex<HashMap>` or atomic op, one RAII guard, no `.await`, no alloc.
  Each unconfigured filter is one `is_empty()` / `is_enabled()` bool check.
- **UDP first-packet gate**: reuses the already-computed sniff hint + a short
  route-list scan for a `FirstBytes` match. No lock/alloc/task.
- **PROXY protocol** (`v1`/`v2`): one `Vec` (≤52 B) + one extra `write_all` to
  the backend before the pump. `v2-udp`: one `Cow::Owned` on the *first*
  datagram only. `none` pays nothing.
- **Transparent mode**: upstream socket gains one `setsockopt(IP_TRANSPARENT)` +
  one `bind` before connect; UDP also binds one per-session `IP_TRANSPARENT`
  reply socket and uses `recvmsg` + a fixed cmsg buffer. A few extra syscalls at
  setup; nothing per byte.
- **UDP passive health**: one `Arc<Backend>` clone per session; on a `send`/`recv`
  error only, one `kind()` compare and (for `ConnectionRefused`) one
  `Backend::observe(false)`.
- **Connection draining** (`ConnTracker`): one `watch::send_modify` on open and
  on close. Nothing steady-state.

### Control-plane only (zero data-path cost)

Listener reconcile, backend overlay rebuild, backend discovery refresh
(`refresh_loop` per source → `fetch` → `Discovery::store` + `notify` → snapshot
rebuild reads `Discovery::get`).

---

## Codebase map

| File | Responsibility |
|------|----------------|
| `crates/gsp-config/src/lib.rs` | Raw YAML types, `validate()`, resolved `Config` / `PoolConfig` / `ListenerConfig` / `ResolverConfig` / `SniffersConfig` / `HealthCheck`; routing (`Matcher`, `Action`, `OnError`, `Cidr`, `CidrSet` trie, `HostPattern`, `MatchContext`, `RouteHint`, `extract_sni`); filters (`Acl`, `GeoAcl`, `RateLimit`, `PerSourceLimit`, `GlobalLimits`). **All schema rules here.** |
| `crates/gsp-core/src/snapshot.rs` | `Snapshot { listeners, pools, sources, resolvers, limits, geo_db }`; `build` → `build_with_overlay` → `build_with_sources` carry health/admin-state over by address and apply overlay + discovered backends. |
| `crates/gsp-core/src/pool.rs` | `Pool` (balancer, `rr` index, `hash_on`, `weights`; `acquire` / `acquire_for` / `acquire_addr` / `backend`, `hrw_score`), `Backend` (health / active / streaks / `check_kind` + `AdminState`), `BackendGuard` (RAII slot + passive health), `PickError`. |
| `crates/gsp-core/src/listener.rs` | `run_tcp_listener`: accept loop; per-conn task does ACL/geo/rate/`per_source`/global-cap checks, first-bytes peek, route match, pool lookup. |
| `crates/gsp-core/src/listener_udp.rs` | `run_udp_listener`: per-worker `recvmmsg` batch loop, `(client, Option<SocketAddr> dst)` session table, sticky affinity, `IdleWheel` idle expiry, per-session upstream socket + reply pump. `UdpMode` Plain / Prefix (`IP_PKTINFO` + `sendmsg` reply) / Transparent (`IP_ORIGDSTADDR`, client-bound upstream, per-session `IP_TRANSPARENT` reply socket). |
| `crates/gsp-core/src/{ratelimit,src_conns,limits,geo}.rs` | Per-listener token bucket / per-source concurrent cap / process-wide caps / MaxMind country lookup. |
| `crates/gsp-core/src/sniff.rs` | `Sniffer` trait + `Sniffers` `ArcSwap`-backed registry (`register` / `get` / `replace`) + `warn_if_missing`. **No built-in sniffers** — the seam the phase-9 loader fills. |
| `crates/gsp-core/src/route_hint.rs` | `RouteHints` — `ArcSwap<HashMap>` `src_ip → pool` push-resolver table (`POST /route-hint`), lock-free read. |
| `crates/gsp-core/src/resolver.rs` | `trait Resolver`, `ResolveRequest` / `Resolution` / `ResolveError`, `Resolvers` (`ArcSwap`-backed) map, `resolve_route` (async route walk), `CachedResolver` (TTL LRU). Transports live in `gsp`. |
| `crates/gsp-core/src/discovery.rs` + `sources.rs` | `trait BackendSource` + `Discovery` last-known-good cache + `refresh_loop`; `SourceManager` reconciles one refresh task per pool `source` on reload (discovery analogue of `ListenerManager`). |
| `crates/gsp-core/src/drain.rs` | `ConnTracker` / `ConnGuard` — `watch<usize>` live count + an `id → {proto, listener, peer, local, pool, backend, since}` registry (`GET /sessions`); `wait_idle()`. |
| `crates/gsp-core/src/overlay.rs` | `BackendOverlay` — runtime backend add/remove, layered on file `targets` at rebuild. |
| `crates/gsp-core/src/proxy.rs` | `handle_tcp` (pool) / `handle_tcp_target` (resolver `target`, no guard) → `connect_backend` + `pump` (`splice` / buffered). Writes the PROXY protocol header before the pump. |
| `crates/gsp-core/src/proxy_protocol.rs` | `header(mode, src, dst)` — PROXY protocol v1 (text) / v2 (binary, STREAM or DGRAM). Write-only. |
| `crates/gsp-core/src/health.rs` | 500 ms sweep, probes due backends (`tcp_connect` / `udp_probe`), updates health + `gsp_pool_backends` gauges. |
| `crates/gsp-core/src/runtime.rs` | `Runtime::start*` builds `ListenerManager` + `SourceManager` + health task; owns `RouteHints` / `ConnTracker` / `BackendOverlay` / `Sniffers` / `reload_requested`. `shutdown_with_grace` = stop listeners + await health + `wait_idle` + `abort_all`. |
| `crates/gsp-core/src/listeners.rs` | `ListenerManager` — one task `Group` (workers + `watch<bool>` stop) per listener; `start_all`, `reconcile(&Snapshot)` (diff by name), `stop_all` / `abort_all`. |
| `crates/gsp-core/src/net.rs` | `bind_reuseport_tcp` (+ `freebind` / `transparent`), `bind_reuseport_udp` (`UdpMode`), `bind_transparent_udp`, `connect_tcp_from`, `set_ip_transparent` (v4 + v6). |
| `crates/gsp-core/src/metrics_defs.rs` | **Every metric name** (`pub const`). |
| `crates/gsp/src/main.rs` | CLI (`--config`, `--check`), tracing, runtime bring-up, shutdown. Opens `geo_db` + builds sniffers (fails `--check` / startup on a bad one). |
| `crates/gsp/src/admin.rs` | axum router: `GET /healthz` `/readyz` `/metrics` `/pools` `/config` `/sessions`; `POST /route-hint` `/admin/drain` `/admin/undrain`; `PATCH` backend state; `POST` / `DELETE` a backend. `router(state)` split out for the in-module HTTP tests. |
| `crates/gsp/src/resolver.rs` | `HttpResolver` (`reqwest`), `GrpcResolver` (`tonic`, `mod pb` from `build.rs`), `build_resolvers`. |
| `crates/gsp/src/sniffer_loader.rs` | `SnifferLoader` (shared `wasmtime::Engine` + epoch-ticker thread) + `scan(&SniffersConfig)`; `WasmSniffer`; `build_sniffers` = `new` + one `scan`. |
| `crates/gsp/src/discovery.rs` | `DnsSrvSource` (`hickory-resolver`), `ConsulSource` / `KubernetesSource` (`reqwest`), `DiscoveryFactory`. |
| `crates/gsp/src/reload.rs` | `SIGHUP` + `notify` file watch + `reload_requested()` → debounce → `apply` (validate, `build_with_overlay`, store, reconcile listeners / sources / resolvers, rescan sniffers). |
| `crates/gsp/src/procinfo.rs` | `gsp_build_info` / `gsp_fd_open` / `gsp_fd_limit` — build identity + a small detached `/proc/self/fd` sampling task. |
| `crates/gsp/proto/resolver.proto` + `build.rs` | gRPC resolver contract + `tonic_build` codegen (needs `protoc`). |
| `crates/plugins/` | Standalone workspace (own `[workspace]`): `gsp-sniffer-abi` guest helper + `a2s` / `minecraft` / `regex-firstbytes` plugins. `make plugins`. Never a dep of `gsp` / `gsp-core`. |
| `crates/gsp-bench/` | `make bench` — `latency` mode (in-process, added p50/p99 vs. NFR N1/N2) + `concurrency` mode (real separate `gsp` process, connection-count ramp, `/proc` RSS/fd sampling). |
| `crates/gsp-config/fuzz/` | Standalone workspace: `extract_sni` / `route_match` / `parse_config` `cargo-fuzz` targets. `make fuzz` (nightly). |

---

## Testing

`make check` runs fmt + clippy `-D warnings` + ~150 tests (`gsp-config`,
`gsp-core` unit + `crates/gsp-core/tests/{tcp_forward,udp_forward,amplification}.rs`,
`gsp` unit incl. the `sniffer_loader` WAT-fixture end-to-end and the in-module
admin HTTP tests). Needs `protoc` on `PATH`.

- `TP­ROXY` / `IP_TRANSPARENT` e2e is not in CI (needs `CAP_NET_ADMIN`) — covered
  by config parse/reject + `connect_tcp_from` fallback tests. Setup recipe in
  `docs/04`.
- The UDP `prefix:` e2e needs a Linux host with `IP_PKTINFO` and loopback
  `127.0.0.2` / `127.0.0.3`; not portable to macOS/Windows CI.
- `#[ignore]`d, run in the `plugins` CI job after `make plugins`: the first-party
  `.wasm` artifact round-trip and the WASM-boundary N1 latency bench.
- `wasmtime` is a normal `cargo` dep — it does **not** need the
  `wasm32-unknown-unknown` rustc target; that target is only needed to *build*
  the plugin crates. Loader tests assemble WASM from inline WAT via the `wat`
  crate.

---

## Infra / environment

- Toolchain via `rustup` (`stable`). If `cargo` isn't found:
  `export PATH="$HOME/.cargo/bin:$PATH"`.
- **`protoc` is a build requirement** (gRPC resolver codegen in
  `crates/gsp/build.rs`). CI installs `protobuf-compiler`.
- CI: `.github/workflows/ci.yml` — installs `protoc`, then `cargo fmt --check`,
  `clippy --all-targets --all-features`, `cargo test --all`. Plus nightly `fuzz`
  and `plugins` jobs.
- git remote `github.com/Wueschli/gameserver-proxy`, branch `main`; `git push`
  works, `origin/main` is current. The HTTPS credential helper logs a harmless
  "nonexistent Windows path" warning before falling back to a working credential.
- `Cargo.toml` declares `MIT OR Apache-2.0` with `LICENSE-MIT` / `LICENSE-APACHE`
  included. No explicit user decision on record — confirm if it matters.

---

## Open questions carried from `docs/01-requirements.md`

- Does one client ever need **two backends at once** (TCP control + UDP gameplay
  on different instances)? Affects the session model.
- Is **QUIC-aware routing** (connection ID) needed, or is opaque UDP enough?
  Assumed opaque.
- Cross-instance session failover: assumed **no** for v1 (ADR 4).
