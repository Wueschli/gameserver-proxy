# 09 – Technologiewahl

## Sprache

### Empfehlung: Rust
- Vorhersagbare Latenz ohne GC-Pausen – entscheidend für den RTT-Aufschlag (NFR N1/N2).
- Zero-Copy-Datenpfad (`splice`, `sendmmsg`) gut abbildbar; feine Kontrolle über
  Allokationen (Slab, Puffer-Pools).
- Sichere Parser für Peek/Sniffer (kein Buffer-Overflow im exponierten Pfad).
- Ökosystem: `tokio` / `monoio` (io_uring), `socket2`, `nix`, `arc-swap`, `hashbrown`,
  `governor` (Rate-Limit), `prometheus`/`metrics`, `rustls` (nur falls TLS-Wrapping),
  `tonic` (gRPC-Resolver).

### Alternative: Go
- Schnellere Entwicklung, sehr gute Netz-Standardbibliothek, einfache Cross-Compilation.
- GC-Pausen heute klein, aber unter Millionen Sessions messbar; mehr Tuning nötig.
- Gut, wenn Team Go-affin ist und p99 < 2 ms Spielraum hat.

### Nicht empfohlen
- C/C++: maximale Kontrolle, aber Sicherheitsrisiko im exponierten Parser-Pfad.
- Sprachen mit schwerer Laufzeit (JVM, Node) für den Datenpfad – nur Control Plane
  denkbar.

## Laufzeit / IO-Modell

| Option | Bewertung |
|--------|-----------|
| `tokio` (epoll) | reif, breit, gut genug; Standard-Wahl für v1 |
| `monoio` / `glommio` (io_uring, thread-per-core) | beste Latenz/Durchsatz, `SO_REUSEPORT` + geteiltes-nichts passt exakt zum Design; jünger, kleineres Ökosystem |
| io_uring direkt | maximale Kontrolle, hoher Aufwand |

Plan: v1 auf `tokio` mit thread-per-core-Layout (`SO_REUSEPORT`, LocalSet je Worker).
io_uring-Backend als spätere Optimierung hinter einer IO-Abstraktion.

## Schlüsselbibliotheken (Rust, Vorschlag)

- **Sockets/Syscalls**: `socket2`, `nix` (für `recvmmsg`, `IP_TRANSPARENT`, `splice`).
- **Datenstrukturen**: `hashbrown` (Session-Map), `slab`, `ip_network_table`/LPM-Trie
  für ACLs, `hashring` für konsistentes Hashing.
- **Config**: `serde` + `serde_yaml`, `figment` für Env-Overlay, `notify` für Datei-Watch.
- **Snapshot-Swap**: `arc-swap`.
- **Rate-Limit**: `governor` (GCRA-Token-Bucket).
- **Metriken**: `metrics` + `metrics-exporter-prometheus`.
- **Tracing**: `tracing` + `opentelemetry`.
- **Resolver**: `tonic` (gRPC), `reqwest`/`hyper` (HTTP).
- **Admin-API**: `axum` (klein, auf internem Interface).
- **PROXY-Protokoll**: `ppp` oder eigene kleine v2-Implementierung.
- **Tests**: `criterion` (Bench), `cargo-fuzz` (Parser), eigene Lastlast-Tools
  (`udp-flood-gen`, `conn-storm`).

## Plattform

- **Primär Linux** (x86-64 + arm64). Kernel ≥ 5.10 für stabile io_uring-Option.
- Genutzte Kernel-Features: `SO_REUSEPORT`, `splice`, `recvmmsg`/`sendmmsg`,
  `IP_TRANSPARENT`/TPROXY, `TCP_NODELAY`, SYN-Cookies, `NOTRACK`.
- Container: distroless/scratch-Image, nur benötigte Capabilities.
- Kein Windows/macOS im Datenpfad (nur als Dev-Build mit reduziertem Fastpath).

## Architektur-Entscheidungen (ADR-Kurzform)

| ADR | Entscheidung | Begründung | Verworfene Optionen |
|-----|--------------|-----------|---------------------|
| 1 | L4-agnostisch als Kern, L7 nur als optionale Read-only-Sniffer | Ein Proxy für alle Spiele; kein Protokoll-Reverse-Engineering im Kern | Pro-Spiel-Proxy; L7-Terminierung |
| 2 | Thread-per-core + `SO_REUSEPORT`, thread-lokaler Session-State | keine Locks im heißen Pfad; lineare Skalierung | globale Session-Map mit Sharding-Locks |
| 3 | Immutable Config-Snapshot + atomarer Swap | Reload ohne Datenpfad-Blockade | RWLock auf Live-Config |
| 4 | Kein geteilter Session-State zwischen Instanzen (v1) | Komplexität/Latenz; Anycast-Reconnect reicht | Raft/Redis-Session-Store |
| 5 | Client-IP-Erhalt optional (PROXY-Proto **oder** TPROXY) | unterschiedliche Backend-Fähigkeiten/Netz-Setups | nur eine Methode erzwingen |
| 6 | Externe Routing-Logik über Resolver-Callback + Cache | Matchmaking bleibt außerhalb; Proxy versteht keine Tokens | Routing-Regeln im Proxy hart kodieren |
| 7 | Rust | GC-freie Latenz, sichere Parser | Go (GC), C++ (Speichersicherheit) |
| 8 | UDP: `connect(2)`-Socket pro Session | Rückweg ohne Tabellensuche, Kernel-Absenderfilter | ein Socket + manuelles Demux |

## Risiken & Gegenmaßnahmen

| Risiko | Gegenmaßnahme |
|--------|---------------|
| io_uring-Portabilität/Bugs | Abstraktion, `tokio`/epoll als Default-Backend |
| `splice`-Fastpath deckt nicht alle Fälle (TLS-Peek-Restbytes) | sauberer Fallback-Puffer-Pfad, Property-Tests |
| Session-Tabelle als Speicher-DoS | harte Caps + LRU-Eviction + First-Packet-Gate |
| Sniffer-Parser als Angriffsfläche | `#![forbid(unsafe)]` in Plugins, Fuzzing, Byte-/Zeit-Limits |
| TPROXY-Netz-Setup fehleranfällig | ausführliche Doku + `preflight check`-Kommando |
| Resolver-Latenz im Verbindungsaufbau | Cache, enges Timeout, `stale_ok`, Fallback-Route |
