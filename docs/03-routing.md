# 03 – Routing

Ziel: eine eingehende Verbindung/Session einem **Pool** (und optional einem
Affinitäts-Key) zuordnen – möglichst ohne Spielprotokoll-Wissen, mit optionalen
Plugins, wenn nötig.

## Auswertungsreihenfolge

1. **Frühe Filter** (ACL, Rate Limit, Geo) – noch vor Routing, können sofort ablehnen.
2. **Listener-Bindung** – der Listener kann schon 1:1 einem Pool zugeordnet sein
   (einfachster Fall, keine weitere Logik).
3. **Route-Matching** – geordnete Regelliste des Listeners; **erste passende Regel
   gewinnt**. Jede Regel: `match` + `action` (`pool` oder `resolver`).
4. **Resolver** (falls Regel es verlangt) – externer Lookup mit Cache/Fallback.
5. **Default-Route** – wenn nichts passt.

## Matcher-Typen

### `always`
Trifft immer. Für Default-/Catch-all-Regeln.

### `port`
Für Port-Range-Listener: Zielport bestimmt den Pool (`30000–30099 → pool-a`).

### `client-cidr`
Quell-IP in CIDR-Liste. Nutzen: interne Tester auf Staging-Pool, Region grob nach
IP-Block, Partner-Ranges.

### `sni` (TCP, TLS-Handshake – ohne Terminierung)
- Liest `ClientHello`, extrahiert `server_name`.
- Vergleich exakt / Suffix (`*.eu.example.com`) / Regex.
- TLS wird **nicht** terminiert; der Bytestrom (inkl. ClientHello) wird unverändert
  weitergereicht. Nur Peek.
- Kosten: ein `MSG_PEEK`, minimaler Parser (kein OpenSSL nötig).

### `first-bytes` (TCP oder UDP-erstes-Datagramm)
- `prefix`: exakter Byte-/String-Präfix (`hex:`- oder `ascii:`-Notation).
- `regex`: Regex über die ersten `N` Bytes (vorkompiliert, `N` begrenzt).
- `length`: Datagrammlänge in Bereich (grobe Heuristik, z. B. Query vs. Gameplay).
- `sniffer: <name>`: benanntes Plugin liefert strukturierte Hinweise, z. B.:
  - `sni` (generisch, s. o.)
  - `minecraft` → Handshake-Hostname + Protokollversion
  - `a2s` / `source-query` → Valve-Query erkannt (auf Query-Pool routen)
  - `quic` → Version + (unverschlüsselte) Connection-ID-Länge
  - `wireguard`, `openvpn` … (falls der Proxy auch sowas fronten soll)
- Plugin-Kontrakt: **read-only**, bekommt bis zu `peek_max_bytes`, gibt
  `Option<RouteHint { key?: String, pool_hint?: String, reject?: bool }>`. Kein
  Zugriff auf spätere Bytes, kein Schreiben.

### `external` (Resolver)
- Action einer Regel statt fixem Pool.
- Request an gRPC/HTTP-Endpunkt:
  ```json
  {
    "listener": "public-udp",
    "src": "203.0.113.7:51343",
    "dst": "198.51.100.10:7777",
    "sni": "eu.example.com",
    "first_bytes_b64": "AAECaGVsbG8=",
    "routing_key": "eu.example.com"
  }
  ```
- Response:
  ```json
  { "pool": "match-eu-1", "target": null, "sticky_key": "player:42", "ttl_sec": 30 }
  ```
  entweder `pool` (dann normale LB) **oder** `target` (fixe Instanz, z. B. vom
  Matchmaker vergeben).
- **Cache**: Key konfigurierbar (`src_ip`, `sni`, `routing_key`, Hash der ersten Bytes).
  Positive TTL aus Response, negative TTL separat.
- **Timeout/Fehler**: `on_error: fallback_route | reject | stale_ok`.
- Nutzen: Matchmaker weist Spieler-Token einer Instanz zu; der Proxy muss das Token
  nicht verstehen, nur weiterreichen und cachen.

## Lastverteilung im Pool

| Strategie | Einsatz |
|-----------|---------|
| `round_robin` | gleichwertige zustandslose Backends |
| `least_conn` | langlebige Sessions ungleicher Dauer |
| `weighted` | heterogene Hardware / Canary (`weight: 1` vs `weight: 20`) |
| `consistent_hash` | Affinität ohne Sticky-Table; Hash-Key = Client-IP oder Routing-Key |
| `first_available` | Aktiv/Passiv, Backend füllt sich bis Limit, dann nächstes |

## Session-Affinität

- **Sticky-Table**: `key → backend_id (+ TTL)`. Key kommt aus Resolver (`sticky_key`),
  Sniffer (`key`) oder Konfig (`hash_on: src_ip`).
- Neuer Request mit bekanntem Key und **gesundem** Backend → dorthin.
- Backend `unhealthy`/`draining` → Key neu auflösen, Tabelleneintrag ersetzen.
- Für UDP ist Affinität faktisch Pflicht (sonst zerfällt der Gameplay-Strom auf
  mehrere Instanzen). Default: `hash_on: src_ip` + Sticky-Table.

## Beispiele

### A) Ein Port, viele Match-Instanzen, Matchmaker entscheidet
```
listener public-udp (0.0.0.0:7777/udp)
  route 1: match = always  → action = external(resolver = matchmaker)
     resolver cache key = first_bytes(0..16)   # enthält Session-Token
     on_error = reject
```

### B) Region per SNI, TLS bleibt beim Backend
```
listener public-tcp (0.0.0.0:443/tcp)
  route 1: sni suffix ".eu.example.com" → pool match-eu   (consistent_hash on sni)
  route 2: sni suffix ".us.example.com" → pool match-us
  route 3: always                        → pool lobby-default
```

### C) Minecraft-Netzwerk (Plugin nur zum Auslesen des Hostnamens)
```
listener public-tcp (0.0.0.0:25565/tcp)
  route 1: sniffer minecraft, host == "survival.example.net" → pool mc-survival
  route 2: sniffer minecraft, host == "creative.example.net" → pool mc-creative
  route 3: always → pool mc-lobby
```

### D) Getrennte Query- und Gameplay-Pools nach Paketlänge
```
listener public-udp (0.0.0.0:27015/udp)
  route 1: first-bytes prefix hex:FFFFFFFF → pool source-query   # A2S
  route 2: always                          → pool gameplay
```
