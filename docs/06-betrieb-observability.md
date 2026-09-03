# 06 – Betrieb & Observability

## Metriken (Prometheus)

### Listener / Verbindungen
- `gsp_listener_connections_total{listener,protocol,result}` – `result` =
  `accepted|denied_acl|denied_ratelimit|no_route|resolver_error`
- `gsp_active_connections{listener,protocol}` (Gauge)
- `gsp_active_udp_sessions{listener}` (Gauge)
- `gsp_connection_duration_seconds{listener,pool}` (Histogram)
- `gsp_session_setup_seconds{listener,pool,phase}` – `phase` = `peek|route|resolve|connect`

### Durchsatz
- `gsp_bytes_total{listener,pool,dir}` – `dir` = `c2s|s2c`
- `gsp_packets_total{listener,pool,dir}` (UDP)
- `gsp_datagrams_dropped_total{listener,reason}`

### Upstream / Pool
- `gsp_pool_backends{pool,state}` (Gauge; `state` = healthy|unhealthy|draining|disabled)
- `gsp_backend_active_sessions{pool,backend}` (Gauge)
- `gsp_backend_connect_errors_total{pool,backend,kind}` – `kind` = `timeout|refused|unreachable`
- `gsp_healthcheck_total{pool,backend,result}`
- `gsp_lb_selections_total{pool,strategy,result}` – `result` = `ok|no_backend|limit_reached`

### Resolver
- `gsp_resolver_requests_total{resolver,result}` – `ok|error|timeout`
- `gsp_resolver_latency_seconds{resolver}` (Histogram)
- `gsp_resolver_cache{resolver,state}` – hits/misses/entries/evictions

### Proxy-intern
- `gsp_config_reload_total{result}` / `gsp_config_version` (Gauge, Zeitstempel)
- `gsp_worker_busy_ratio{worker}`
- `gsp_buffer_pool_exhausted_total`
- `gsp_fd_open` / `gsp_fd_limit`
- `gsp_build_info{version,commit}`

**RTT-Aufschlag** ist die zentrale SLO-Metrik: aus `session_setup_seconds` +
optionalem periodischem synthetischem Ping (Proxy→Backend) und, falls Sniffer/Resolver
Latenzdaten liefern, Client→Proxy-Schätzung.

## Verbindungslogs (strukturiert, JSON)

Ein Ereignis bei Session-**Ende** (plus optional bei -Start), samplebar:

```json
{
  "ts": "2026-09-03T10:48:12Z",
  "event": "session_close",
  "listener": "public-udp",
  "protocol": "udp",
  "src": "203.0.113.7:51343",
  "route": "route#1",
  "resolver": "matchmaker",
  "pool": "match-eu",
  "backend": "10.1.0.12:7777",
  "sticky_key": "player:42",
  "bytes_c2s": 184320,
  "bytes_s2c": 942080,
  "pkts_c2s": 1440,
  "pkts_s2c": 1460,
  "duration_ms": 812345,
  "setup_ms": 6,
  "close_reason": "idle_timeout",
  "proxy_protocol": "none"
}
```

`close_reason`: `client_close|backend_close|idle_timeout|drain|limit|backend_down|error`.

## Tracing (optional, OpenTelemetry)

Nur die **Setup-Phase** tracen (nicht den Byte-Stream): Span `session.setup` mit
Child-Spans `peek`, `route.match`, `resolver.call`, `backend.connect`. Trace-ID kann
in den PROXY-v2-TLV mitgegeben werden, wenn das Backend sie korrelieren soll.

## Health-Endpunkte des Proxys

- `GET /healthz` – Prozess lebt.
- `GET /readyz` – Konfig geladen & valide, mindestens ein Listener gebunden.
- `GET /metrics` – Prometheus.
- `GET /config`, `GET /pools`, `GET /sessions?listener=&src=&pool=` – Introspektion.

## Health Checks der Backends

- **Aktiv**: `tcp_connect` / `udp_probe` (send/expect Bytes) / `http` (Sidecar-Port).
  Parameter: `interval`, `timeout`, `rise`, `fall`. Checks laufen in der Control Plane,
  Ergebnis → neuer Snapshot.
- **Passiv**: Datenpfad meldet `connect refused/timeout`, frühe RST, ICMP unreachable.
  Nach `n` Fehlern in `t` Sekunden → Backend `unhealthy` (schneller als aktiver Check).
- **Recovery**: erst nach `rise` erfolgreichen aktiven Checks zurück auf `healthy`.

## Graceful Draining & Deployments

Ablauf für Backend-Austausch ohne Spielerabriss:

1. Neues Backend registrieren (`POST /pools/{p}/backends`) → nach `rise` Checks `healthy`.
2. Altes Backend `PATCH state=draining` → LB vergibt keine neuen Sessions mehr dorthin;
   Sticky-Keys darauf werden bei nächstem Kontakt neu aufgelöst.
3. Warten bis `gsp_backend_active_sessions` = 0 **oder** `drain_deadline` erreicht.
4. Nach Deadline: verbleibende Sessions mit `close_reason=drain` beenden.
5. Altes Backend entfernen.

Für **Proxy-Instanz**-Neustart:
1. `readyz` auf „false“ setzen (`POST /admin/drain`) → vorgelagerter LB/Anycast nimmt
   die Instanz aus der Rotation.
2. Grace-Period: keine neuen `accept`s, bestehende Sessions laufen weiter.
3. Nach `shutdown_grace` (z. B. 30–120 s) verbleibende Sessions schließen, Prozess beenden.
4. `SIGTERM` löst genau diesen Ablauf aus.

## Kapazitätsplanung / Alarme

- Alarm: `gsp_worker_busy_ratio > 0.8` (5 min) → skalieren.
- Alarm: `gsp_pool_backends{state="healthy"} < N_min` pro Pool.
- Alarm: `gsp_datagrams_dropped_total`-Rate > 0 (Puffer zu klein / Überlast).
- Alarm: `gsp_resolver_requests_total{result!="ok"}`-Anteil > 1 %.
- Alarm: `gsp_fd_open / gsp_fd_limit > 0.8`.
- Alarm: `session_setup_seconds` p99 über SLO.
