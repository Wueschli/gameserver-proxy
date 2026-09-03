# 00 – Überblick

## Problem

Gameserver direkt aus dem Internet erreichbar zu machen bringt wiederkehrende Probleme:

- Die echte Server-IP ist öffentlich und damit direkt DDoS-angreifbar.
- Viele Serverinstanzen (Matches, Lobbies, Regionen) brauchen viele Ports/IPs und
  eine externe Stelle, die Clients auf die richtige Instanz lenkt.
- Neustarts/Deployments trennen aktive Spieler.
- Betrieb (Metriken, Verbindungslogs, Sperrlisten) ist pro Spiel neu zu bauen.

Bestehende Lösungen wie BungeeCord/Velocity (Minecraft) oder spielspezifische Master-
Server lösen das jeweils nur für **ein** Spiel und oft nur für **ein** Transport-Protokoll.

## Ziel

Ein Reverse-Proxy für Gameserver, der **protokoll-agnostisch auf Layer 4** arbeitet
(TCP + UDP) und optional über **austauschbare L7-Plugins** Routing-Entscheidungen aus
dem ersten Handshake-Paket ableitet. Der Kern kennt kein Spiel; Spielwissen steckt
ausschließlich in optionalen Plugins bzw. in externer Routing-Logik.

## Anwendungsfälle

1. **IP-Schutz / Edge-Terminierung** – Clients verbinden nur zum Proxy; Backends liegen
   in einem privaten Netz. Der Proxy ist der einzige DDoS-exponierte Punkt.
2. **Fan-out auf viele Instanzen** – ein öffentlicher Endpunkt `play.example.com:7777`
   verteilt auf hunderte Match-Container.
3. **Region-/Hostname-Routing** – `eu.example.com` und `us.example.com` auf demselben
   Port, Auswahl anhand SNI (TLS) oder erstem Paket.
4. **Matchmaker-Integration** – der Proxy fragt bei Verbindungsaufbau einen externen
   Dienst (gRPC/HTTP): „Wohin gehört dieser Spieler / dieses Token?“
5. **Zero-Downtime-Deployments** – neue Backend-Version wird registriert, alte auf
   „draining“ gesetzt; bestehende Sessions laufen aus, neue gehen auf neu.
6. **Blue/Green & Canary** für Gameserver-Builds.
7. **Zentrale Betriebssicht** – einheitliche Metriken (Verbindungen, Durchsatz, RTT-
   Aufschlag, Fehler) und Verbindungslogs über alle Spiele.
8. **Zugriffskontrolle** – IP-Allow/Deny-Listen, Geo-Filter, Rate Limiting,
   Verbindungs-Obergrenzen pro Backend.

## Nicht-Ziele

- **Keine Spiellogik**, kein Matchmaking, kein Anti-Cheat, kein Lobby-System.
- **Kein Parsen/Umschreiben von Spielpaketen** im Kern (nur optionale Plugins dürfen
  die ersten N Bytes lesen – nie den laufenden Stream verändern).
- **Kein NAT-Ersatz** für beliebigen Client-zu-Client-Traffic (kein TURN/Relay-Mesh).
- **Keine Transportverschlüsselung erzwingen**, wenn das Spiel sie nicht nutzt
  (optionales TLS-/DTLS-Wrapping ist ein Zusatzfeature, kein Muss).
- **Kein Ersatz für einen L3-Scrubbing-Anbieter** bei volumetrischen Angriffen – der
  Proxy ergänzt ihn, ersetzt ihn nicht.

## Leitprinzipien

- **Latenz zuerst.** Jeder zusätzliche Hop kostet RTT. Der Datenpfad muss allokations-
  arm, kopierarm und ohne unnötige Serialisierung sein.
- **Agnostisch als Default, Plugin als Ausnahme.** Ohne Plugin muss reines
  L4-Forwarding funktionieren.
- **Control Plane vom Data Plane trennen.** Konfig-/Backend-Änderungen dürfen den
  Datenpfad nicht blockieren.
- **Betrieb ist ein Feature.** Health Checks, Draining, Metriken, Reload gehören in
  die erste Version, nicht in „später“.

## Glossar

| Begriff | Bedeutung |
|---------|-----------|
| **Listener** | Öffentlicher Socket (IP:Port + Protokoll), an dem Clients ankommen. |
| **Route** | Regel, die eingehende Verbindungen einem Backend-Pool zuordnet. |
| **Backend / Target** | Konkrete Gameserver-Instanz (IP:Port). |
| **Pool / Upstream** | Menge gleichwertiger Backends mit Auswahlstrategie. |
| **Session** | Für UDP: logische Verbindung, identifiziert über das 4-Tupel. |
| **Sniffer / L7-Plugin** | Optionaler Parser, der aus den ersten Bytes eine Routing-Info zieht (z. B. SNI, Minecraft-Handshake-Hostname). |
| **Draining** | Backend nimmt keine neuen Sessions mehr an, bestehende laufen aus. |
| **PROXY-Protokoll** | Header (v1/v2), der die echte Client-Adresse an das Backend voranstellt. |
| **TPROXY** | Linux-Kernel-Feature für transparentes Proxying mit Erhalt der Quell-IP. |
