# 05 – Konfiguration

## Prinzipien

- **Eine deklarative YAML-Datei** ist die Quelle der Wahrheit. Discovery-Adapter und
  Admin-API ergänzen zur Laufzeit nur die **Backend-Listen** und **-Zustände**.
- **Validierung vor Aktivierung.** Fehlerhafte Konfig wird abgelehnt; die laufende
  bleibt aktiv.
- **Hot Reload** über `SIGHUP`, Datei-Watch oder `POST /reload`. Listener werden nur
  bei geänderter Bind-Adresse neu gebunden.
- Umgebungsvariablen-Interpolation (`${VAR}`) für Secrets/Token.

## Schema (Referenz)

```yaml
# global
settings:
  workers: 0                 # 0 = Anzahl CPU-Kerne
  admin:
    listen: "127.0.0.1:9900"
    auth: { mode: "bearer", token: "${ADMIN_TOKEN}" }   # oder mode: mtls
  metrics: { path: "/metrics" }
  log:
    format: "json"
    connection_log: { enabled: true, sample: 1.0 }
  limits:
    max_connections: 500000
    max_udp_sessions: 1000000

# wiederverwendbare Filter
filters:
  - name: block-bogons
    type: deny_cidr
    cidrs: ["10.0.0.0/8", "192.168.0.0/16", "100.64.0.0/10"]
  - name: conn-rate
    type: rate_limit
    scope: src_ip            # src_ip | src_/24 | global
    new_per_sec: 50
    burst: 100

# Backend-Quellen
backend_sources:
  - name: static-eu
    type: static
    targets: ["10.1.0.11:7777", "10.1.0.12:7777"]
  - name: k8s-match
    type: kubernetes_endpoints
    namespace: "games"
    service: "match-server"
    port_name: "game"
  - name: srv-us
    type: dns_srv
    record: "_game._udp.us.internal.example.com"
    refresh_sec: 10

# Pools
pools:
  - name: match-eu
    source: static-eu
    balancer: { strategy: consistent_hash, hash_on: src_ip }
    affinity: { table_ttl_sec: 120 }
    health_check:
      type: udp_probe          # tcp_connect | udp_probe | http
      send_hex: "01"
      expect_hex_prefix: "02"
      interval_sec: 2
      timeout_ms: 500
      rise: 2
      fall: 3
    per_backend:
      max_sessions: 200
      max_new_per_sec: 20
    proxy_protocol: "none"     # none | v1 | v2 | v2-udp
    connect_timeout_ms: 300
    idle_timeout_sec: 90

  - name: match-us
    source: srv-us
    balancer: { strategy: least_conn }
    health_check: { type: tcp_connect, interval_sec: 2, timeout_ms: 400, rise: 2, fall: 3 }

  - name: source-query
    source: static-eu
    balancer: { strategy: round_robin }
    idle_timeout_sec: 10

# Resolver (externe Routing-Logik)
resolvers:
  - name: matchmaker
    type: grpc                 # grpc | http
    endpoint: "https://matchmaker.internal:8443"
    timeout_ms: 40
    cache:
      key: ["first_bytes:0:16"]     # z. B. Session-Token-Präfix
      positive_ttl_sec: 30
      negative_ttl_sec: 2
      max_entries: 200000
    on_error: "reject"         # reject | fallback | stale_ok

# Listener + Routen
listeners:
  - name: public-udp
    bind: "0.0.0.0:7777"
    protocol: udp
    filters: ["block-bogons", "conn-rate"]
    peek_max_bytes: 64
    routes:
      - match: { type: always }
        action: { resolver: matchmaker }
      - match: { type: always }        # Fallback, falls resolver on_error=fallback
        action: { pool: match-eu }

  - name: public-tls
    bind: "0.0.0.0:443"
    protocol: tcp
    peek_max_bytes: 2048
    peek_timeout_ms: 200
    routes:
      - match: { type: sni, suffix: ".eu.example.com" }
        action: { pool: match-eu }
      - match: { type: sni, suffix: ".us.example.com" }
        action: { pool: match-us }
      - match: { type: always }
        action: { pool: match-eu }

  - name: public-mc
    bind: "0.0.0.0:25565"
    protocol: tcp
    peek_max_bytes: 512
    routes:
      - match: { type: sniffer, name: minecraft, host: "survival.example.net" }
        action: { pool: match-eu }
      - match: { type: always }
        action: { pool: match-us }

  - name: source-27015
    bind: "0.0.0.0:27015"
    protocol: udp
    routes:
      - match: { type: first_bytes, prefix: "hex:FFFFFFFF" }
        action: { pool: source-query }
      - match: { type: always }
        action: { pool: match-eu }

  # Rohes UDP ohne Protokoll-Hinweis: Subdomain per Ziel-IP (siehe docs/03 Schema A)
  - name: raw-udp
    bind: "[2001:db8:ace:1::]/64:7777"   # Präfix-Bind, ein Socket
    protocol: udp
    recv_dst_addr: true                   # IPV6_RECVPKTINFO / IP_PKTINFO aktivieren
    freebind: true                        # ip_nonlocal_bind / IP_FREEBIND
    routes:
      - match: { type: dst, cidr: "2001:db8:ace:1::1/128" }   # survival.example.net
        action: { pool: match-eu }
      - match: { type: dst, cidr: "2001:db8:ace:1::2/128" }   # creative.example.net
        action: { pool: match-us }
      - match: { type: always }
        action: { reject: true }          # unbekannte Ziel-IP -> verwerfen
    affinity: { hash_on: src_ip }

  # Variante nur-IPv4: Subdomain per Port (Schema B), SRV verteilt den Port
  - name: raw-udp-v4
    bind: "0.0.0.0:30000-30099"
    protocol: udp
    routes:
      - match: { type: port, eq: 30001 }
        action: { pool: match-eu }
      - match: { type: port, eq: 30002 }
        action: { pool: match-us }
      - match: { type: always }
        action: { reject: true }
```

## Validierungsregeln (Auszug)

- Jeder `pool.source` muss auf eine `backend_sources[].name` zeigen.
- Jede `action.pool` / `action.resolver` muss existieren.
- Jeder Listener braucht mindestens eine Route; letzte Route sollte `always` sein
  (sonst Warnung „kein Default“).
- `proxy_protocol: v2-udp` nur zusammen mit `protocol: udp`.
- `consistent_hash` erfordert `hash_on`.
- `match.type: dst` erfordert `recv_dst_addr: true` am Listener (sonst ist die
  Ziel-Adresse pro Paket/Verbindung nicht bekannt); Präfix-Bind erfordert `freebind:
  true` und ein auf den Host geroutetes Präfix.
- `match.type: port` nur sinnvoll bei Range-Bind (`:30000-30099`).
- Bind-Adressen dürfen sich zwischen Listenern nicht überlappen (gleiche IP:Port:Proto);
  ein Präfix-Bind darf keine Einzel-Bind-Adresse eines anderen Listeners überdecken.
- Zahlenbereiche: Timeouts > 0, `rise`/`fall` ≥ 1, TTLs ≥ 0.

## Reload-Semantik

| Änderung | Verhalten |
|----------|-----------|
| Backend hinzugefügt/entfernt | sofort im neuen Snapshot, bestehende Sessions unberührt |
| Backend → `draining` | keine neuen Sessions, bestehende laufen aus |
| Pool-Balancer geändert | gilt für **neue** Routing-Entscheidungen |
| Route geändert/neu | gilt für neue Verbindungen/Sessions |
| Listener-Bind geändert | alter Socket wird geschlossen, neuer gebunden (kurzer Gap) |
| `settings.workers` geändert | erfordert Neustart (dokumentiert) |
| ungültige Datei | Reload abgelehnt, Metrik `config_reload_failed_total++`, alte Konfig aktiv |
