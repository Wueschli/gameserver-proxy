# 04 – Transport & Client-IP-Erhalt

## TCP

- **Annahme**: `accept4()` mit `SOCK_NONBLOCK`; `SO_REUSEPORT`-Sharding pro Worker.
- **Ziel-IP der Verbindung**: für `dst`-basiertes Subdomain-Routing ohne Socket je IP
  wird der Listener mit `IP_FREEBIND` / `ip_nonlocal_bind` auf ein geroutetes Präfix
  gebunden; die konkrete Ziel-IP der akzeptierten Verbindung kommt aus
  `getsockname()`. (Im transparenten Modus liefert `getsockname()` bereits die echte
  vom Client adressierte IP.)
- **Peek fürs Routing**: `recv(MSG_PEEK)` bis `peek_max_bytes` / `peek_timeout_ms`.
  Sendet der Client zuerst nichts (server-speaks-first), sofort Default-Route.
- **Upstream-Connect**: nicht-blockierend, `connect_timeout`. `TCP_NODELAY` gesetzt;
  vorhandene Optionen des Clients werden nicht „vererbt“, sondern per Konfig gesetzt.
- **Datenpumpe**: Linux `splice()` (Socket→Pipe→Socket, zero-copy). Fallback: zwei
  Richtungspuffer (je 32–64 KB) mit `readv/writev`. `SO_RCVBUF`/`SO_SNDBUF` konfigurierbar.
- **Half-Close**: `shutdown(SHUT_WR)` in eine Richtung weiterreichen, andere Richtung
  weiterlaufen lassen, bis auch sie schließt.
- **Timeouts**: connect, idle (kein Byte in beide Richtungen), optional
  max-lifetime.
- **Keepalive**: `TCP_KEEPALIVE` Richtung Backend optional aktivieren, um tote
  Sessions in NAT-freien internen Netzen zu erkennen.

## UDP

UDP hat keine Verbindung – der Proxy baut das Konzept „Session“ selbst.

- **Session-Schlüssel**: `(src_ip, src_port, dst_ip, dst_port)` (4-Tupel).
- **Empfang**: `recvmmsg()` in Batches auf `SO_REUSEPORT`-Sockets, ein Loop je Worker.
- **Empfang auf einem ganzen Präfix** (für `dst`-Routing bei Spielen ohne
  Protokoll-Hinweis, siehe [03](03-routing.md)): der Listener bindet **nicht** je
  Ziel-IP einen Socket, sondern einen Wildcard-Socket und aktiviert
  `IP_PKTINFO` / `IPV6_RECVPKTINFO`. Pro Datagramm liefert das `cmsg` die
  **tatsächliche Ziel-Adresse** (`ipi_addr` / `ipi6_addr`); danach `dst`-Match gegen
  den LPM-Trie. Antworten müssen dieselbe Quell-Adresse tragen – die Ziel-IP wird
  beim Senden per `cmsg` (`IP_PKTINFO`) bzw. über den an die Client-IP gebundenen
  Session-Socket gesetzt. Voraussetzung: das Präfix ist auf den Proxy-Host geroutet
  (kein NDP/ARP je Adresse nötig) und `net.ipv6.ip_nonlocal_bind` bzw. `IP_FREEBIND`
  erlaubt das Binden.
- **Pro Session ein Upstream-Socket**, per `connect(2)` an das Backend gebunden:
  - Rückantworten kommen ohne Tabellensuche direkt am richtigen Socket an.
  - Kernel filtert fremde Absender.
  - `sendmmsg()` Richtung Backend in Batches.
- **Session-Tabelle**: `HashMap<4-Tupel, SessionRef>` thread-lokal pro Worker
  (keine Sperren). Slab-Allokator für `Session`.
- **Idle-Timeout**: Timing-Wheel; jede Aktivität schiebt den Eintrag. Default 30–120 s,
  pro Route konfigurierbar (Turn-based-Spiele brauchen mehr).
- **Obergrenzen**: max Sessions global / pro Worker / pro Backend; neue-Session-Rate
  pro Quell-IP.
- **ICMP**: „port unreachable“ vom Backend als passives Health-Signal auswerten.
- **Fragmentierung / MTU**: keine Reassemblierung; überlange Antworten laufen normal
  über IP-Fragmentierung. `IP_MTU_DISCOVER` Richtung Backend nach Bedarf.

### QUIC/DTLS
Als opakes UDP behandelt. Migration (Client-Port-Wechsel) bricht ohne QUIC-CID-
Awareness die Session → optionaler `quic`-Sniffer, der die Connection-ID liest und
als Session-Schlüssel statt des 4-Tupels verwendet. Nur als Ausbaustufe.

## Client-IP an das Backend weitergeben

Standardmäßig sieht das Backend nur die Proxy-IP. Optionen, pro Pool wählbar:

### 1. PROXY-Protokoll (empfohlen, wenn Backend es kann)
- **TCP**: v2-Binärheader (oder v1-Text) **vor** den ersten Nutzdaten an das Backend.
  Enthält echte Quell-/Ziel-Adresse. Backend (bzw. dessen Framework) muss es parsen.
- **UDP**: v2-Header wird dem **ersten** Datagramm der Session vorangestellt; Folge-
  Datagramme ohne Header. Braucht Backend-Unterstützung (z. B. Bibliothek, die den
  ersten Payload als PROXY-Header interpretiert). Als Option `proxy_protocol: v2-udp`.
- Sicherheit: Backends müssen PROXY-Header **nur** von der Proxy-IP akzeptieren
  (sonst IP-Spoofing durch Clients).

### 2. Transparenter Modus (Linux TPROXY)
- Proxy-Socket mit `IP_TRANSPARENT`; Upstream-Verbindung nutzt `IP_TRANSPARENT` +
  Bind an die **Client-IP** als Quelle. Das Backend sieht die echte Client-IP ohne
  Protokolländerung.
- Voraussetzung: Routing so, dass Rückverkehr des Backends wieder über den Proxy läuft
  (Policy-Routing / `ip rule` + `iptables -t mangle` bzw. nftables, oder Backend-
  Default-GW = Proxy). Aufwändiger im Netzdesign, dafür 100 % transparent.
- Nötige Capability: `CAP_NET_ADMIN` (bzw. `CAP_NET_RAW`), kein voller Root.

### 3. Kein Erhalt
- Backend sieht Proxy-IP. Ausreichend, wenn Anti-Cheat/Logik die Client-IP nicht
  braucht oder sie anderweitig (im Spiel-Login-Token) mitkommt. Dokumentiertes
  Default-Verhalten.

## Socket-Tuning (Startwerte, per Konfig überschreibbar)

| Parameter | TCP | UDP |
|-----------|-----|-----|
| `SO_REUSEPORT` | an (Sharding) | an (Sharding) |
| `SO_RCVBUF` / `SO_SNDBUF` | 256 KB | 4–8 MB (Loss-Vermeidung bei Bursts) |
| `TCP_NODELAY` | an | – |
| `SO_BUSY_POLL` | optional | optional (Latenz ↓, CPU ↑) |
| `recvmmsg`/`sendmmsg` Batch | – | 32–64 |
| Pipe-Größe für `splice` | 256 KB | – |

## Grenzen des Betriebssystems

- FD-Limit (`RLIMIT_NOFILE`) hoch setzen (≥ 2 × erwartete Verbindungen).
- Ephemeral-Port-Bereich Richtung Backend groß (`net.ipv4.ip_local_port_range`) –
  betrifft nur den nicht-transparenten Modus; im transparenten Modus wird an die
  Client-IP gebunden.
- `net.core.somaxconn`, `net.core.netdev_max_backlog`, `nf_conntrack` (oder conntrack
  für den Proxy-Pfad per `NOTRACK` deaktivieren).
