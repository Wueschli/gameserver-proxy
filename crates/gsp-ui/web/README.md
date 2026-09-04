# gsp-ui frontend

The React + Vite + TypeScript SPA `gsp-ui` (`crates/gsp-ui`, the phase 10+11
admin GUI's BFF, `docs/10` "The admin GUI") serves to the browser. A
**standalone `npm` project** — its own `package.json`, deliberately never a
Cargo workspace member — the same reason `crates/plugins/` and
`crates/gsp-config/fuzz/` are standalone: this build needs a different
toolchain (Node/npm) than the rest of the repo, and `make check` on the main
workspace must not require it.

## Build

```sh
make ui          # from the repo root: npm install && npm run build
```

or directly:

```sh
cd crates/gsp-ui/web
npm install
npm run build     # tsc -b && vite build -> dist/
```

`gsp-ui --static-dir <dir>` (default `crates/gsp-ui/web/dist`, assuming the
binary runs from the repo root like every other `cargo run -p ...` example
in this repo) serves whatever `dist/` contains as a fallback under every
route its own API doesn't claim. A missing or unbuilt `dist/` doesn't fail
`gsp-ui`'s startup — requests for it just 404, the same posture running
`gsp-ui` for its API alone (as every test in `crates/gsp-ui` does) already
has.

## Local frontend development

```sh
npm run dev
```

runs Vite's dev server with hot reload, proxying `/ui`, `/api`, `/ws`, and
`/healthz` to a real `gsp-ui` process on `127.0.0.1:9903` (see
`vite.config.ts`) — start that separately, pointed at real
`gsp-controller`/`gsp-aggregator` instances (or none, to exercise the
`503`-when-unconfigured paths).

## Layout

- `src/api.ts` — thin `fetch` wrappers against `gsp-ui`'s own API
  (`/ui/*`, `/api/fleet/*`, `/api/config/*`). Every call sends the session
  cookie and nothing else; this frontend never holds or sends a bearer
  token — `gsp-ui` presents those to the controller/aggregator itself.
- `src/types.ts` — the wire shapes shared with `crates/gsp-ui`'s Rust side
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

No client-side routing: the whole app is one page, view state (which tab,
which dialog) lives in React state, not the URL. There's nothing here that
needs a deep link or a browser-back button yet.
