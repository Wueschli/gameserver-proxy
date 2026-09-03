# game-server-proxy

Ein **game-agnostischer Gameserver-Reverse-Proxy**: ein einzelner Einstiegspunkt vor
beliebigen Gameservern, der TCP- und UDP-Verkehr transparent auf Backend-Instanzen
weiterleitet – ohne das jeweilige Spielprotokoll zu kennen.

Typische Ziele: Verstecken der Backend-IPs (DDoS-Schutz), Port-/Hostname-basiertes
Routing auf viele Serverinstanzen, unterbrechungsfreie Neustarts (Connection Draining),
zentrale Metriken und Zugriffskontrolle.

## Planungsdokumente

| Datei | Inhalt |
|-------|--------|
| [docs/00-ueberblick.md](docs/00-ueberblick.md) | Ziele, Nicht-Ziele, Anwendungsfälle, Glossar |
| [docs/01-anforderungen.md](docs/01-anforderungen.md) | Funktionale & nicht-funktionale Anforderungen |
| [docs/02-architektur.md](docs/02-architektur.md) | Komponenten, Data Plane / Control Plane, Datenflüsse |
| [docs/03-routing.md](docs/03-routing.md) | Routing-Strategien im Detail |
| [docs/04-transport-und-client-ip.md](docs/04-transport-und-client-ip.md) | TCP/UDP-Handling, Client-IP-Erhalt, PROXY-Protokoll |
| [docs/05-konfiguration.md](docs/05-konfiguration.md) | Konfigurationsschema & Beispiele |
| [docs/06-betrieb-observability.md](docs/06-betrieb-observability.md) | Metriken, Logging, Health Checks, Draining |
| [docs/07-sicherheit-ddos.md](docs/07-sicherheit-ddos.md) | Rate Limiting, ACLs, DDoS-Mitigation |
| [docs/08-roadmap.md](docs/08-roadmap.md) | Umsetzung in Phasen / Meilensteine |
| [docs/09-technologiewahl.md](docs/09-technologiewahl.md) | Sprache, Bibliotheken, Alternativen |

## Status

Reine Planungsphase. Noch kein Code.
