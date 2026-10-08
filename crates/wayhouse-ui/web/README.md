# wayhouse-ui frontend

The React + Vite + TypeScript SPA `wayhouse-ui` (`crates/wayhouse-ui`, the phase 10+11
admin GUI's BFF, `docs/10` "The admin GUI") serves to the browser. A
**standalone `npm` project** — its own `package.json`, deliberately never a
Cargo workspace member — the same reason `crates/wayhouse-config/fuzz/`
is standalone: this build needs a different
toolchain (Node/npm) than the rest of the repo, and `make check` on the main
workspace must not require it.

## Build

```sh
make ui          # from the repo root: npm install && npm run build
```

or directly:

```sh
cd crates/wayhouse-ui/web
npm install
npm run build     # tsc -b && vite build -> dist/
```

`wayhouse-ui --static-dir <dir>` (default `crates/wayhouse-ui/web/dist`, assuming the
binary runs from the repo root like every other `cargo run -p ...` example
in this repo) serves whatever `dist/` contains as a fallback under every
route its own API doesn't claim. A missing or unbuilt `dist/` doesn't fail
`wayhouse-ui`'s startup — requests for it just 404, the same posture running
`wayhouse-ui` for its API alone (as every test in `crates/wayhouse-ui` does) already
has.

## Tests

```sh
make ui-test     # from the repo root
# or: cd crates/wayhouse-ui/web && npm test   (vitest run, jsdom + Testing Library)
```

CI runs them in the `ui` job, alongside `npm run build` (which type-checks).
They mock `src/api.ts` / `useFleetSocket` at the module boundary, so they need no
running `wayhouse-ui`. What they pin: every destructive action (drain, remove
backend, set a backend `draining`/`disabled`, config rollback, sniffer remove,
applying Settings)
asks for confirmation first and never calls the API on cancel; restorative
actions (undrain, re-enable) don't ask; action buttons disable while a request
is in flight; `api.ts` error/response handling (plain-text `ok` login body, JSON
`error` field, `X-Config-Revision` header). The Rust half of the wire-shape
contract — every proxied route forwards status + response headers — is
`header_contract` in `crates/wayhouse-ui/src/proxy_util.rs` (part of `make check`).

## Local frontend development

```sh
npm run dev
```

runs Vite's dev server with hot reload, proxying `/ui`, `/api`, `/ws`, and
`/healthz` to a real `wayhouse-ui` process on `127.0.0.1:9903` (see
`vite.config.ts`) — start that separately, pointed at real
`wayhouse-controller`/`wayhouse-aggregator` instances (or none, to exercise the
`503`-when-unconfigured paths).

## Layout

- `src/api.ts` — thin `fetch` wrappers against `wayhouse-ui`'s own API
  (`/ui/*`, `/api/fleet/*`, `/api/config/*`). Every call sends the session
  cookie and nothing else; this frontend never holds or sends a bearer
  token — `wayhouse-ui` presents those to the controller/aggregator itself.
- `src/types.ts` — the wire shapes shared with `crates/wayhouse-ui`'s Rust side
  (kept in sync by hand; there is no schema generation here).
- `src/useFleetSocket.ts` — the `GET /ws/fleet` client: keeps the latest
  fleet view in React state, reconnects on disconnect.
- `src/components/Login.tsx` — the session-cookie login form.
- `src/components/FleetView.tsx` — the fleet dashboard: per-instance /
  per-pool / per-backend table (live over the WebSocket), drain/undrain,
  add/patch/remove a backend, and the route-hint form — phase 10's
  "operational" GUI capability level.
- `src/components/ConfigView.tsx` — the config editor + revision
  history/diff/rollback — phase 10's "full management" GUI level.

Client-side routing uses `react-router` (`/fleet`, `/settings`, `/sniffers`,
`/plugins`, `/config-history`, `/tunnel`), so each page has its own URL and works with the
browser back button.

## Browser e2e (Playwright)

`make ui-e2e` (`npm run test:e2e`) builds the UI, serves it with `vite preview`
and drives it in Chromium. The wayhouse-ui backend is stubbed per test with
`page.route()` (`e2e/`), so no Rust process is needed. First run:
`npx playwright install chromium`, or set `PW_CHROMIUM` to an existing
Chromium binary.
