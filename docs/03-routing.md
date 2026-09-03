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

### `dst` (Ziel-IP / Ziel-Präfix des eingehenden Pakets)
Die Adresse, an die der Client geschickt hat – **nicht** aus dem Payload, sondern aus
dem Socket. Vergleich gegen CIDR/Präfix-Liste (LPM-Trie). Das ist der Hebel für Spiele
**ohne** jeden Protokoll-Hinweis: die Subdomain wird per DNS auf eine eigene Ziel-IP
abgebildet, der Proxy unterscheidet an `dst`. Siehe Abschnitt
[„Routing ohne Protokoll-Hinweis"](#routing-ohne-protokoll-hinweis-rohe-daten-an-ipport).
Voraussetzung im Datenpfad: Empfang auf einem ganzen Präfix mit **einem** Socket
(`IP_PKTINFO` / `IPV6_RECVPKTINFO`), siehe [04](04-transport-und-client-ip.md).

### `client-cidr`
Quell-IP in CIDR-Liste. Nutzen: interne Tester auf Staging-Pool, Region grob nach
IP-Block, Partner-Ranges. Auch für den Launcher-/Push-Resolver (Abschnitt unten):
kurzlebige `src_ip → pool`-Zuordnung.

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

## Routing ohne Protokoll-Hinweis (rohe Daten an IP:Port)

Viele Spiele schicken ab dem ersten Byte rohe, oft verschlüsselte Nutzdaten an eine
feste `IP:Port` – kein Hostname, kein SNI, nichts Stabiles im ersten Paket. Dann gibt
es **im Stream** nichts, woran man `survival` von `creative` unterscheiden könnte.

**Grundsatz:** Man routet nach dem, was man ohne Payload trotzdem kennt – der
**Ziel-Adresse** (`dst_ip`) und dem **Ziel-Port**, an die der Client gesendet hat. Die
„Subdomain" muss also über DNS auf eine unterscheidbare `(IP, Port)`-Kombination
abgebildet werden. Drei Schemata, einzeln oder kombiniert:

### Schema A – eine IP pro Server (empfohlen, besonders mit IPv6)

- DNS bildet den Namen auf eine **eigene** Adresse ab:
  ```
  survival.example.net   AAAA  2001:db8:ace:1::1
  creative.example.net   AAAA  2001:db8:ace:1::2
  hardcore.example.net    A    198.51.100.7      # IPv4 nur wenn nötig
  ```
- Der Proxy bekommt ein **geroutetes Präfix** (z. B. ein IPv6-`/64` oder `/48`) und
  nimmt Verkehr auf **allen** Adressen daraus mit **einem einzigen** Wildcard-Socket
  an – kein Socket pro IP:
  - **UDP**: `IP_PKTINFO` / `IPV6_RECVPKTINFO` liefert pro Datagramm die tatsächliche
    Ziel-Adresse; die Antwort wird per `cmsg` mit **derselben** Quell-Adresse gesendet.
  - **TCP**: `IP_FREEBIND` bzw. `net.ipv4.ip_nonlocal_bind` / `bind`-any auf das
    Präfix; die Ziel-IP der akzeptierten Verbindung kommt aus `getsockname()`.
- Routentabelle: `dst`-Matcher gegen einen LPM-Trie `Präfix → Pool`. Millionen
  Ziel-IPs kosten praktisch nichts (Speicher ~ Anzahl Regeln, nicht Anzahl IPs).
- IPv6 ist hier der Normalfall: Adressraum ist faktisch unbegrenzt, jede Instanz kann
  ihre eigene öffentliche Adresse bekommen. IPv4 nur mit kleinem, teurem IP-Pool –
  dann eher Schema B.

### Schema B – ein Port pro Server

- Alle Namen zeigen auf **dieselbe** IP; unterschieden wird am Port:
  ```
  survival.example.net → 198.51.100.7:30001
  creative.example.net → 198.51.100.7:30002
  ```
- Wo das Spiel/Protokoll es unterstützt, verteilt ein **SRV-Record** Host+Port
  (`_game._udp.survival.example.net SRV 0 0 30001 edge.example.net`). Sonst trägt der
  Launcher / die Server-Browser-Config den Port.
- Proxy: Port-Range-Listener + `port`-Matcher (`30001 → pool-survival`).
- Grenzen: manche Clients haben den Port hart kodiert; restriktive Client-Firewalls;
  SRV-Unterstützung ist außerhalb weniger Protokolle selten.

### Schema C – Vorab-Registrierung über Launcher/API (Push-Resolver)

- Startet das Spiel über einen Launcher oder Server-Browser, ruft **dieser** vor dem
  Connect die Control-Plane-API:
  `POST /route-hint { src_ip: "203.0.113.7", pool: "survival", ttl_sec: 30 }`.
- Der Proxy hält eine kurzlebige Tabelle `src_ip → pool`. Das erste Paket/SYN von
  dieser IP ohne weiteren Hinweis wird darüber aufgelöst, danach normale Sticky-Table.
- Schwäche: mehrere Spieler hinter **einer** NAT-IP, die gleichzeitig verschiedene
  Subdomains wollen, sind nicht trennbar – es sei denn, der Launcher kann zusätzlich
  einen kurzen Token setzen, der doch ins erste Paket wandert (dann `first-bytes`).
  Andernfalls Fallback auf Schema A/B.
- Dies ist die „umgekehrte" Variante des [`external`-Resolvers](#external-resolver):
  Push statt Pull, nützlich wenn der Proxy den Client-Kontext nicht selbst erfragen
  kann.

### Was grundsätzlich nicht geht

Gleiches Spiel, **gleiche IP, gleicher Port**, kein Token im ersten Paket, mehrere
logische Server dahinter: nicht unterscheidbar. Dann führt kein Weg an Schema A
(eigene IP) oder B (eigener Port) vorbei – oder an einem Launcher, der einen Token
mitgibt.

### DNS und Routentabelle aus einer Quelle

Damit `Name → (IP/Präfix | Port) → Pool` konsistent bleibt, sollte **eine**
Inventardatei (pro Server: `name`, `address` bzw. `port`, `pool`) sowohl die DNS-Zone
als auch die `listeners[].routes` mit `dst`/`port`-Match generieren. Sonst driften
Auflösung und Weiterleitung auseinander.

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

### E) Rohes UDP ohne jeden Hinweis – Subdomain per Ziel-IP (Schema A)
```
DNS:  survival.example.net  AAAA  2001:db8:ace:1::1
      creative.example.net  AAAA  2001:db8:ace:1::2
      arena.example.net     AAAA  2001:db8:ace:1::3

listener raw-udp
  bind:      "[2001:db8:ace:1::]/64 :7777/udp"   # ein Wildcard-Socket, IPV6_RECVPKTINFO
  routes:
    route 1: dst 2001:db8:ace:1::1/128 → pool survival
    route 2: dst 2001:db8:ace:1::2/128 → pool creative
    route 3: dst 2001:db8:ace:1::3/128 → pool arena
    route 4: always                     → reject   # unbekannte Ziel-IP
  affinity: hash_on = src_ip            # UDP-Session bleibt auf einem Backend
```
Der Client verbindet zu `survival.example.net:7777`, schickt sofort rohe Pakete; der
Proxy liest die Ziel-Adresse aus dem Datagramm-`cmsg` und wählt den Pool. Antworten
gehen mit `2001:db8:ace:1::1` als Quelle zurück.

### F) Wie E, aber nur IPv4 verfügbar – Subdomain per Port (Schema B)
```
DNS/SRV:  _game._udp.survival.example.net  SRV 0 0 30001 edge.example.net
          _game._udp.creative.example.net  SRV 0 0 30002 edge.example.net

listener raw-udp-v4
  bind:  "0.0.0.0:30000-30099/udp"
  routes:
    route 1: port 30001 → pool survival
    route 2: port 30002 → pool creative
    route 3: always     → reject
```
