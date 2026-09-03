# 02 – Architektur

## Überblick

```
                 ┌─────────────────────────── Proxy-Instanz ───────────────────────────┐
                 │                                                                     │
   Clients       │   ┌─────────┐   ┌──────────┐   ┌───────────┐   ┌────────────────┐   │   Backends
 (TCP/UDP)  ─────┼──▶│Listener │──▶│ Router   │──▶│ Upstream/ │──▶│ Connection/    │───┼──▶ Gameserver
                 │   │ (Accept)│   │ (Match + │   │ Pool +    │   │ Session-Pump   │   │    (privates Netz)
                 │   └─────────┘   │ Resolver)│   │ LB + Aff. │   │ (splice/io)    │   │
                 │        │        └────┬─────┘   └─────┬─────┘   └───────┬────────┘   │
                 │        │             │               │                │            │
                 │   ┌────┴─────────────┴───────────────┴────────────────┴────────┐   │
                 │   │                     Data Plane (heißer Pfad)               │   │
                 │   └───────────────────────────┬───────────────────────────────┘   │
                 │                               │ liest Snapshot (lock-free)        │
                 │   ┌───────────────────────────┴───────────────────────────────┐   │
                 │   │  Control Plane: Config-Loader · Admin-API · Discovery-     │   │
                 │   │  Adapter · Health-Checker · Metrik-/Log-Exporter          │   │
                 │   └───────────────────────────────────────────────────────────┘   │
                 └─────────────────────────────────────────────────────────────────────┘
```

## Trennung Data Plane / Control Plane

- **Data Plane**: nimmt Verbindungen an, trifft die Routing-Entscheidung anhand eines
  **unveränderlichen Konfig-Snapshots**, pumpt Bytes. Kein Lock auf dem heißen Pfad;
  Snapshot-Wechsel über einen atomaren Pointer-Swap (`arc-swap`-Muster).
- **Control Plane**: baut neue Snapshots (aus Datei + Discovery + Admin-API), führt
  Health Checks aus, exportiert Metriken/Logs. Läuft in eigenen Tasks/Threads, darf
  blockieren.

Ein **Snapshot** enthält: Listener-Definitionen, kompilierte Routentabelle (inkl.
vorkompilierter Matcher), Pools mit aktueller Backend-Liste **und** Health-Zustand.
Health-Updates erzeugen einen neuen Snapshot (billiges Copy-on-Write der betroffenen
Pool-Struktur, Rest wird geteilt).

## Komponenten

### 1. Listener
- Bindet Socket(s). TCP: `accept`-Loop bzw. `SO_REUSEPORT`-Sharding über Worker.
- UDP: ein/mehrere `recvmmsg`-Loops pro Socket, `SO_REUSEPORT` für Skalierung.
- Wendet **frühe Filter** an: IP-Allow/Deny, Connection-Rate-Limit (billig, vor jeder
  Allokation).
- Übergibt an den Router: bei TCP nach optionalem Peek der ersten Bytes; bei UDP mit
  dem ersten Datagramm.

### 2. Router
- Wählt die erste passende Route (deterministische Priorität).
- Matcher-Typen: `always`, `sni`, `first-bytes` (Präfix/Regex/Länge), `client-cidr`,
  `port`, `external`.
- **First-Packet-Peek** (TCP): `MSG_PEEK` bis zu `peek_max_bytes` oder `peek_timeout`.
  Reicht das nicht (Client sendet nichts zuerst), greift die Default-Route.
- **External Resolver**: ruft gRPC/HTTP mit `{listener, src_ip, sni, first_bytes(b64),
  routing_key}` auf, erwartet `{pool | target, sticky_key?, ttl}`. Ergebnis wird
  gecacht (Key = konfigurierbar). Timeout → Fallback-Route.
- Ergebnis: Ziel-Pool + optionaler Affinitäts-Key.

### 3. Upstream / Pool
- Hält Backend-Liste + Zustände aus dem Snapshot.
- LB-Strategie wählt gesundes Backend; Affinitäts-Key wird zuerst konsultiert
  (konsistentes Hashing / Sticky-Table mit TTL).
- Prüft Pro-Backend-Limits (max Sessions, Neu-Rate). Kein Kandidat → definierte
  Fehlerbehandlung (TCP: RST/close; UDP: Datagramm verwerfen + Metrik).

### 4. Connection- / Session-Pump
- **TCP**: bidirektionales Kopieren. Linux-Fastpath via `splice()` zwischen den beiden
  Sockets (zero-copy), Fallback auf User-Space-Puffer. Half-Close propagieren,
  Timeouts (idle, connect), `TCP_NODELAY` durchreichen.
- **UDP**: pro Session ein verbundener Upstream-Socket (`connect(2)`), damit Antworten
  ohne Tabellensuche zurückfinden. Session-Tabelle: `HashMap<4-Tupel, Session>` +
  Idle-Timer-Rad (timing wheel). Idle-Timeout schließt Session.
- **PROXY-Protokoll**: bei aktivierter Option Header vor den ersten Nutzdaten an das
  Backend schreiben (TCP) bzw. voranstellen (UDP, erstes Datagramm).

### 5. Health-Checker (Control Plane)
- Aktive Prüfungen je Backend nach Intervall/Timeout/Schwellen (rise/fall).
- Passive Signale aus dem Datenpfad (Connect-Fehler, frühe Resets, UDP-„port
  unreachable“ / ICMP) fließen als Ereignisse ein.
- Publiziert neuen Snapshot bei Zustandswechsel.

### 6. Config-Loader & Discovery-Adapter
- Lädt/validiert YAML, baut Snapshot, kompiliert Matcher.
- Adapter (DNS-SRV, Consul, K8s Endpoints, statisch) implementieren eine gemeinsame
  `BackendSource`-Schnittstelle und liefern Backend-Listen pro Pool.
- Reload: neuer Snapshot atomar aktiv; Listener werden nur bei Bind-Änderung neu
  gebunden (sonst weiterbetrieben).

### 7. Admin-API
- `GET /config` (aktiver Snapshot, redigiert), `GET /pools`, `GET /sessions?...`
- `POST /pools/{p}/backends`, `DELETE ...`, `PATCH .../{b} {state: draining}`
- `POST /reload`, `GET /healthz`, `GET /readyz`, `GET /metrics`
- Auth: mTLS oder Bearer-Token, nur an internem Interface gebunden.

## Threading-/Laufzeitmodell

- **Worker pro CPU-Kern**, `SO_REUSEPORT`-Sockets pro Worker → kein Accept-Contention.
- Jede Session ist an ihren annehmenden Worker gepinnt (Thread-lokale Session-Tabelle
  → keine Sperren im UDP-Pfad).
- Control-Plane-Tasks auf eigenem, kleinem Threadpool.
- Speicher: pro Worker vorab reservierte Puffer-Pools; Session-Structs aus Slab-
  Allokator zur Vermeidung von Fragmentierung.

## Ausfall- & Fehlerverhalten

- Backend fällt während Session aus: TCP → close beidseitig, Metrik; UDP → Session
  bleibt kurz bestehen (konfigurierbar), danach Verwurf. Optional „Rehoming“ neuer
  Datagramme auf anderes Backend, wenn keine Affinität verlangt ist.
- Proxy-Instanz fällt aus: Clients reconnecten; L4-LB/Anycast vor dem Proxy verteilt
  auf verbleibende Instanzen. Kein geteilter Session-State in v1.
- Überlast: frühe Rate-Limits + `accept`-Drosselung + Lastabweisung mit Metrik, bevor
  Latenz bestehender Sessions leidet.

## Erweiterungspunkte

- **Sniffer-Plugin-API**: `fn sniff(&[u8]) -> Option<RouteHint>` (SNI, Minecraft,
  Steam A2S, FiveM …). In-Process (statisch gelinkt) in v1; WASM/Proc-Plugin später.
- **BackendSource**-Adapter (siehe oben).
- **Filter-Kette** vor Routing (ACL, Rate Limit, Geo) als geordnete, konfigurierbare
  Liste.
