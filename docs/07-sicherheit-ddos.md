# 07 – Sicherheit & DDoS-Mitigation

## Bedrohungsmodell

| Angreifer | Ziel | Gegenmaßnahme (Ebene) |
|-----------|------|------------------------|
| Volumetrisch (L3/4, Gbit/s) | Uplink sättigen | **vorgelagert**: Anycast/Scrubbing, BGP-Flowspec. Proxy kann das nicht allein. |
| SYN-Flood (TCP) | Verbindungstabelle/Accept | Kernel-SYN-Cookies, `accept`-Backpressure, Conn-Rate-Limit pro IP |
| UDP-Flood / Spoofed | Worker-CPU, Session-Tabelle | Rate-Limit vor Allokation, „erst-Paket-validieren“, Session-Cap, kein Reflect |
| Amplification via Proxy | Proxy als Reflektor missbrauchen | nie ungefragt antworten; Antwort nur an Absender einer etablierten Session |
| Slowloris / Idle-Halten | Ressourcen binden | Peek-Timeout, Idle-Timeout, Min-Durchsatz-Wächter |
| Backend-IP-Leak | Direktangriff am Proxy vorbei | Backends nur im privaten Netz, Firewall: nur Proxy-IPs dürfen zu Backends |
| PROXY-Header-Spoofing | gefälschte Client-IP am Backend | Backend akzeptiert PROXY-Header nur von Proxy-IP (allowlist) |
| Resolver-Missbrauch | Matchmaker mit Lookups fluten | Resolver-Cache, Negative-TTL, Rate-Limit pro IP vor Resolver-Call |
| Admin-API-Zugriff | Umkonfiguration | mTLS/Token, nur internes Interface, kein Default-Bind auf 0.0.0.0 |

## Filter-Kette (vor dem Routing, billig zuerst)

1. **CIDR Allow/Deny** – statische Listen + optionale dynamische Blockliste
   (Feed/Datei, Hot-Reload). O(1) via LPM-Trie.
2. **Geo-Filter** (optional) – MaxMind-DB, Allow/Deny nach Land pro Listener.
3. **Connection-/Datagramm-Rate-Limit** – Token-Bucket je `src_ip` **und** je `/24`
   (gegen verteilte Einzel-IPs). Scope konfigurierbar. Überschuss → verwerfen +
   Metrik, **keine** Fehlerantwort (kein Reflect).
4. **Globale Obergrenzen** – `max_connections`, `max_udp_sessions`,
   `max_new_sessions_per_sec`. Bei Erreichen: neue ablehnen, Bestehende schützen.

Alle Schritte laufen **vor** Puffer-/Session-Allokation.

## TCP-Härtung

- Kernel: `net.ipv4.tcp_syncookies=1`, sinnvolle `somaxconn`, kurzer
  `tcp_synack_retries`.
- `accept`-Backpressure: bei Worker-Überlast Accept-Rate drosseln statt Queue explodieren
  lassen.
- **Peek-Timeout** (`peek_timeout_ms`): wer nach Verbindungsaufbau nichts sendet,
  fliegt raus, bevor ein Backend-Connect passiert.
- **Min-Progress-Wächter**: Verbindung ohne Byte-Fortschritt über `idle_timeout`
  → close.
- Optionales `max_connections_per_ip`.

## UDP-Härtung

- **Kein blindes Antworten.** Der Proxy sendet erst Richtung Client, wenn das Backend
  in einer etablierten Session geantwortet hat.
- **First-Packet-Gate** (optional, pro Route): erstes Datagramm muss `first_bytes`-
  Match bestehen (bekanntes Handshake-Präfix / Sniffer sagt „gültig“), sonst keine
  Session-Anlage. Hält generischen Spoof-Flood von der Session-Tabelle fern.
- **Session-Cap** pro `src_ip` / `/24` und global; LRU-Eviction der ältesten idle
  Sessions bei Druck.
- **Idle-Timeout kurz** halten, wo das Spiel es erlaubt (Query-Pools: wenige Sekunden).
- Empfangs-Puffer groß genug, dass legitime Bursts nicht mit Flood-Verwürfen
  konkurrieren.
- `conntrack` für den Proxy-Pfad per `NOTRACK` deaktivieren (sonst eigene DoS-Fläche).

## Netz- & Deployment-Härtung

- Backends: eigene Sicherheitszone; ingress-Firewall erlaubt **nur** Proxy-Quell-IPs
  auf die Gameserver-Ports. Egress der Backends minimal.
- Proxy läuft ohne Root: nur `CAP_NET_BIND_SERVICE` (Ports < 1024) und – falls
  transparenter Modus – `CAP_NET_ADMIN`. Read-only-Rootfs, seccomp-Profil.
- Secrets (Admin-Token, mTLS-Keys) via Env/Datei mit `0600`, nicht in der YAML.
- Admin-API und Metrics an separates internes Interface binden; nie auf den
  öffentlichen Listener-IPs.
- Rate-Limit- und ACL-Zustände sind pro Instanz; bei HA hinter Anycast pro Knoten
  dimensionieren (Angriff verteilt sich auf alle Knoten).

## Missbrauch als Amplifier – Checkliste

- [ ] Keine Antwort auf Datagramme ohne etablierte Session.
- [ ] Keine ICMP-/Fehlerantworten an gespoofte Absender aus dem App-Pfad.
- [ ] Antwortgröße nie größer als durch Backend-Nutzdaten gedeckt (Proxy erzeugt
      selbst keine Payload).
- [ ] Rate-Limit greift vor jeder Zustandsänderung.

## Sicherheits-Logging

- Ereignisse: ACL-Block, Rate-Limit-Trip (aggregiert, nicht pro Paket), Session-Cap
  erreicht, Health-Flap, Konfig-Reload (wer/was via Admin-API), Resolver-Fehlerserie.
- Kein Klartext von Spiel-Payload in Logs (Datenschutz); nur Metadaten.
