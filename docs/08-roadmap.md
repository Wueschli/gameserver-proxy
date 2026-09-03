# 08 – Roadmap

Inkrementell. Jede Phase ist für sich einsetzbar.

## Phase 0 – Grundgerüst (Woche 1–2)
- Projektsetup, CI, Lint, Test-Harness.
- Konfig-Laden + Validierung + Snapshot-Datenstruktur (noch ohne Reload).
- Strukturiertes Logging, `/healthz`, `/metrics`-Skelett, `build_info`.
- **Ergebnis**: Prozess startet mit Konfig, exponiert Basismetriken.

## Phase 1 – L4-TCP-Proxy (Woche 3–4)
- TCP-Listener mit `SO_REUSEPORT`, Worker-pro-Kern.
- Statisches Listener→Pool-Mapping, `round_robin` + `least_conn`.
- Bidirektionale Pumpe mit `splice()` + Fallback.
- Connect-/Idle-Timeouts, Half-Close.
- Aktive Health Checks `tcp_connect`; Backend-Zustände.
- Kernmetriken (Verbindungen, Bytes, Dauer, Backend-Fehler).
- **Ergebnis**: nutzbar als reiner TCP-Gameserver-Frontproxy.

## Phase 2 – L4-UDP-Proxy (Woche 5–7)
- UDP-Listener, `recvmmsg`/`sendmmsg`, thread-lokale Session-Tabelle, Timing-Wheel.
- Pro-Session `connect(2)`-Upstream-Socket.
- Session-Affinität (`hash_on: src_ip` + Sticky-Table), `consistent_hash`.
- `udp_probe` Health Check.
- Session-Caps, Idle-Timeout pro Route.
- **Ergebnis**: deckt die Mehrheit der Echtzeit-Gameserver ab.

## Phase 3 – Routing-Intelligenz (Woche 8–10)
- Route-Regelliste mit Prioritäten.
- Matcher: `always`, `port`, `client-cidr`, `first-bytes` (prefix/regex/length).
- `dst`-Matcher + Präfix-Listener (ein Socket, `IP_PKTINFO`/`getsockname`) für Spiele
  ohne Protokoll-Hinweis (Subdomain → eigene Ziel-IP). Optional Push-Resolver
  (`/route-hint`).
- SNI-Peek-Matcher (ohne TLS-Terminierung).
- Sniffer-Plugin-API (in-process, statisch): `sni`, `minecraft`, `a2s` als Referenz.
- **Ergebnis**: mehrere Spiele/Regionen hinter einem Port.

## Phase 4 – Externe Routing-Logik (Woche 11–12)
- Resolver-Client (gRPC + HTTP), Request/Response-Schema.
- Ergebnis-Cache mit positiver/negativer TTL, konfigurierbarer Key.
- `on_error`-Strategien (reject/fallback/stale_ok).
- **Ergebnis**: Matchmaker-Integration, Token→Instanz-Routing.

## Phase 5 – Betrieb & Zero-Downtime (Woche 13–14)
- Hot Reload (SIGHUP + Datei-Watch), atomarer Snapshot-Swap.
- Admin-API: Backends CRUD, `draining`, Snapshot lesen, `drain`/`readyz`.
- Graceful Draining (Backend & Proxy-Instanz), `SIGTERM`-Ablauf.
- Passive Health-Signale aus dem Datenpfad.
- **Ergebnis**: produktionsreifer Deploy-/Update-Zyklus.

## Phase 6 – Client-IP-Erhalt (Woche 15–16)
- PROXY-Protokoll v1/v2 (TCP), v2-UDP-Variante.
- Transparenter Modus (TPROXY) inkl. Doku für Netz-/Routing-Setup.
- **Ergebnis**: Backends sehen echte Client-IP.

## Phase 7 – Sicherheit & Härtung (Woche 17–18)
- Filter-Kette: CIDR-Allow/Deny (LPM-Trie), Rate-Limit (src_ip + /24), globale Caps.
- UDP First-Packet-Gate, Amplifier-Checkliste automatisiert testen.
- Optional Geo-Filter.
- Fuzzing der Peek-/Sniffer-Parser, Lasttests gegen NFRs.
- **Ergebnis**: gegen gängige L4/7-Missbräuche gehärtet.

## Phase 8 – Discovery & Skalierung (Woche 19–20)
- `BackendSource`-Adapter: DNS-SRV, Kubernetes Endpoints, Consul.
- HA-Doku: Anycast/L4-LB davor, Kapazitätsplanung, Dashboards & Alarme.
- **Ergebnis**: dynamische Backend-Flotten, horizontale Skalierung.

## Später / Optional
- QUIC-CID-aware Sniffer & Session-Keying.
- WASM-/Out-of-Process-Plugins.
- Cross-Instanz-Session-Handover (geteilter State).
- eBPF/XDP-Vorfilter für Flood-Drop vor dem User-Space.
- Optionales TLS/DTLS-Wrapping (Proxy terminiert, Backend plain).
- Web-UI fürs Admin-API.

## Meilenstein-Schnitte
- **MVP**: Phase 0–2 (L4 TCP+UDP, statisch, Health, Metriken).
- **v1.0**: + Phase 3–5 (Routing, Resolver, Zero-Downtime).
- **v1.1**: + Phase 6–7 (Client-IP, Härtung).
- **v1.2**: + Phase 8 (Discovery, HA-Betriebsdoku).
