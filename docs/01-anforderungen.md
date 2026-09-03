# 01 – Anforderungen

## Funktionale Anforderungen

### F1 – Listener
- F1.1 Mehrere Listener gleichzeitig (verschiedene IP:Port/Protokoll).
- F1.2 Transport: TCP und UDP; QUIC/DTLS als „opaques UDP“ ohne Sonderbehandlung.
- F1.3 Dualstack (IPv4 + IPv6).
- F1.4 Optionaler Port-Range-Listener (ein Listener für `30000–30999`) für Spiele mit
  dynamischen Ports.

### F2 – Routing
- F2.1 Statisches Mapping Listener → Pool.
- F2.2 Routing nach SNI (bei TLS-Handshake) ohne TLS zu terminieren.
- F2.3 Routing nach First-Packet-Match (Bytes-Präfix, Regex auf ersten N Bytes).
- F2.4 Routing über externen Resolver (gRPC/HTTP-Callback) mit Cache & Timeout-Fallback.
- F2.5 Routing nach Client-IP / Geo (CIDR-Listen).
- F2.6 Fallback-/Default-Route, wenn keine Regel greift.
- F2.7 Regel-Priorität deterministisch (erste passende Regel gewinnt).

### F3 – Upstream / Pool
- F3.1 Auswahlstrategien: round-robin, least-connections, konsistentes Hashing
  (nach Client-IP oder Routing-Key), gewichtet, „erst-verfügbar“.
- F3.2 Session-Affinität: gleiches 4-Tupel bzw. gleicher Routing-Key → gleiches Backend,
  solange dieses gesund ist.
- F3.3 Health Checks: aktiv (TCP-Connect, UDP-Probe, HTTP-GET auf Sidecar-Port) und
  passiv (Fehlerquote/Timeouts im Datenpfad).
- F3.4 Backend-Zustände: `healthy`, `unhealthy`, `draining`, `disabled`.
- F3.5 Max. Sessions / max. neue Sessions pro Sekunde pro Backend.

### F4 – Session-Management
- F4.1 UDP-Session-Tabelle über 4-Tupel mit konfigurierbarem Idle-Timeout.
- F4.2 TCP: normale Verbindungslebensdauer, Half-Close-Weitergabe.
- F4.3 Graceful Draining: neue Sessions umleiten, alte mit Grace-Period beenden.
- F4.4 Obergrenzen: globale und Pro-Listener-Verbindungslimits.

### F5 – Client-IP-Erhalt (Details in [04](04-transport-und-client-ip.md))
- F5.1 PROXY-Protokoll v1/v2 für TCP, optional pro Pool.
- F5.2 PROXY-Protokoll v2 für UDP (Header im ersten Datagramm) als Option.
- F5.3 Transparenter Modus (TPROXY, `IP_TRANSPARENT`) unter Linux.
- F5.4 Ohne all das: nur Proxy-IP sichtbar (dokumentiertes Verhalten).

### F6 – Konfiguration & Control Plane
- F6.1 Deklarative Datei (YAML) als Quelle der Wahrheit.
- F6.2 Hot Reload ohne Verbindungsabbruch (SIGHUP / Datei-Watch / API).
- F6.3 Admin-API (HTTP/gRPC) für: Backend registrieren/abmelden, Zustand setzen,
  Konfig-Snapshot lesen, Draining auslösen.
- F6.4 Optionaler dynamischer Backend-Discovery-Adapter (DNS-SRV, Consul, Kubernetes
  Endpoints) hinter einer stabilen internen Schnittstelle.
- F6.5 Validierung beim Laden; ungültige Konfig wird abgelehnt, alte bleibt aktiv.

### F7 – Observability (Details in [06](06-betrieb-observability.md))
- F7.1 Prometheus-Metriken.
- F7.2 Strukturierte Verbindungslogs (JSON), samplebar.
- F7.3 Optionales OpenTelemetry-Tracing für die Verbindungsaufbau-Phase.
- F7.4 Health-/Readiness-Endpunkte für den Proxy selbst.

### F8 – Sicherheit (Details in [07](07-sicherheit-ddos.md))
- F8.1 IP-Allow/Deny-Listen (CIDR), pro Listener.
- F8.2 Rate Limiting: neue Verbindungen/Datagramme pro Quell-IP und pro Subnetz.
- F8.3 SYN-Flood-Schutz (SYN-Cookies via Kernel), UDP-Amplification-Schutz
  (Antwort-nur-nach-erstem-gültigen-Paket-Heuristik, optional Plugin).
- F8.4 Ressourcen-Obergrenzen gegen Speicher-/FD-Erschöpfung.

## Nicht-funktionale Anforderungen

| # | Anforderung | Zielwert (erste Version) |
|---|-------------|--------------------------|
| N1 | Zusätzliche Latenz (p50) durch Proxy im selben RZ | < 0,5 ms |
| N2 | Zusätzliche Latenz (p99) | < 2 ms |
| N3 | Durchsatz pro Instanz (Commodity-16-Core) | ≥ 20 Gbit/s bzw. ≥ 2 Mpps weitergeleitet |
| N4 | Gleichzeitige TCP-Verbindungen pro Instanz | ≥ 500 000 |
| N5 | Gleichzeitige UDP-Sessions pro Instanz | ≥ 1 000 000 |
| N6 | Konfig-Reload ohne Paketverlust bestehender Sessions | ja |
| N7 | Startup bis „ready“ | < 2 s |
| N8 | Speicher pro idle Session | < 1 KB (UDP), < 4 KB (TCP inkl. Puffer) |
| N9 | Verfügbarkeit im HA-Verbund | Ausfall einer Instanz ohne globalen Abriss |

Weitere: horizontal skalierbar (stateless bzgl. persistentem Speicher, Session-State
nur lokal), Linux als Primärplattform, Container-tauglich, Betrieb ohne Root nach
Setup (Capabilities statt Root).

## Annahmen

- Backends sind über ein vertrauenswürdiges internes Netz erreichbar.
- Der Client wählt den Proxy per DNS an; der Proxy muss die Client-Identität nicht
  kryptografisch prüfen (das macht ggf. das Spiel).
- Volumetrische L3/4-Angriffe werden vorgelagert (Scrubbing/Anycast) abgefedert.

## Offene Fragen

- Muss der Proxy **mehrere Backends gleichzeitig** pro Client bedienen (z. B. TCP-Control
  + UDP-Gameplay auf getrennte Instanzen)? → beeinflusst Session-Modell.
- Braucht es **Cross-Instanz-Session-Sharing** (Failover einer laufenden UDP-Session auf
  eine andere Proxy-Instanz)? Erste Version: nein.
- Ist **QUIC-aware Routing** (Connection ID) nötig, oder reicht opakes UDP?
