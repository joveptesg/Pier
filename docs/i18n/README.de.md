> Übersetzung von [README.md](../../README.md). Bei Abweichungen gilt die englische Version.

<p align="center">
  <img src="../logo.svg" alt="Pier" height="120">
</p>

<h3 align="center">Eine leichtgewichtige, selbst gehostete PaaS.<br>Ein einzelnes Binary. 20 MB RAM. Alles deployen.</h3>

<p align="center">
  <a href="https://github.com/joveptesg/pier/blob/main/LICENSE"><img src="https://img.shields.io/github/license/joveptesg/pier?color=blue" alt="License"></a>
  <a href="https://github.com/joveptesg/pier/stargazers"><img src="https://img.shields.io/github/stars/joveptesg/pier?style=flat" alt="Stars"></a>
  <a href="https://github.com/joveptesg/pier/releases"><img src="https://img.shields.io/github/v/release/joveptesg/pier" alt="Release"></a>
  <img src="https://img.shields.io/badge/rust-1.93%2B-orange" alt="Rust">
</p>

<p align="center">
  <a href="../../README.md">English</a> |
  <a href="README.ru.md">Русский</a> |
  <a href="README.zh-CN.md">中文</a> |
  <strong>Deutsch</strong> |
  <a href="README.ja.md">日本語</a> |
  <a href="README.es.md">Español</a> |
  <a href="README.fr.md">Français</a> |
  <a href="README.pt-BR.md">Português</a>
</p>

---

## Was ist Pier?

Pier ist eine quelloffene, selbst gehostete Platform-as-a-Service, die als **einzelnes Rust-Binary** läuft. Deployen Sie Container, Docker-Compose-Stacks und Git-Repositories mit automatischem SSL, Reverse Proxy und einem modernen Web-Dashboard — und das alles mit nur **20–40 MB RAM**.

<!-- 
<p align="center">
  <img src="../../docs/screenshots/dashboard.png" alt="Pier Dashboard" width="800">
</p>
-->

## Warum Pier?

[Coolify](https://coolify.io) ist großartig, benötigt aber **6+ Container** und verbraucht **750 MB – 1,2 GB RAM** im Leerlauf. Pier bietet die gleichen Kernfunktionen in einem einzigen Binary.

| | Coolify | Pier |
|---|---|---|
| **RAM im Leerlauf** | 750 MB – 1,2 GB | 20–40 MB (+Traefik) |
| **Festplatte** | ~1 GB | ~15–30 MB |
| **Laufende Container** | 6+ (Laravel, PostgreSQL, Redis, Soketi, Horizon, Traefik) | 1 Binary + Traefik |
| **Minimale VPS** | 2 GB RAM, 2 vCPU | 512 MB RAM, 1 vCPU |
| **Datenbank** | Externe PostgreSQL | Eingebettete SQLite |
| **Sprache** | PHP / Laravel | Rust |
| **Frontend JS** | ~300 KB+ | ~30 KB (HTMX + Alpine.js) |

## Funktionen

**Container & Stacks**
- 📦 Docker-Container-Verwaltung — erstellen, starten, stoppen, neustarten, entfernen, Logs, Statistiken
- 🐳 Docker-Compose-Stacks mit integriertem YAML-Editor
- 🚀 Ein-Klick-Deployment aus **über 30 Vorlagen**

**Git & Deployments**
- 🔄 Git-to-Deploy-Pipeline mit GitHub- & GitLab-Webhooks
- 🛠 Build aus Dockerfile, Docker-Image oder Compose
- ⏪ Deployment-Verlauf mit Rollback

**Netzwerk & SSL**
- 🌐 Reverse Proxy über Traefik mit automatischem HTTPS
- 🔒 Let's Encrypt SSL-Zertifikate (automatisch bereitgestellt)
- 🔗 Eigene Domains mit automatisch generierten Service-URLs

**Infrastruktur**
- 🖥 Multi-Server-Verwaltung mit Remote-Agenten
- 💾 Geplante Backups mit S3-Integration
- 📊 Echtzeit-Monitoring — CPU, RAM, Festplatte, Netzwerk

**Entwickler-Erfahrung**
- ⚡ Web-UI mit HTMX + Alpine.js — Dark Mode, Echtzeit, responsiv
- 🔑 JWT-Authentifizierung mit bcrypt-Passwort-Hashing
- 🗃 Eingebettete SQLite — keine externe Datenbank erforderlich
- ⚙️ Server-Einrichtung mit einem Befehl

## Vorlagen

**Datenbanken** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**Dienste** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**Spiele** — Minecraft, Terraria

**VPN** — AmneziaWG

**Anwendungen** — Deployment aus Dockerfile, Docker-Image oder Docker Compose

> Sie finden nicht, was Sie brauchen? Deployen Sie jedes Docker-Image oder jeden Compose-Stack manuell.

## Schnellstart

### Option A: Ein-Befehl-Installation (Ubuntu/Debian)

```bash
curl -fsSL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/setup.sh | sudo bash
```

### Option B: Aus Quellcode bauen

```bash
git clone https://github.com/joveptesg/pier.git
cd pier
cargo build --release
sudo bash scripts/install.sh --binary target/release/pier
```

### Option C: Docker

```bash
docker run -d \
  --name pier \
  -p 8443:8443 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v pier-data:/app/data \
  ghcr.io/joveptesg/pier:latest
```

Öffnen Sie anschließend `http://IHRE_SERVER_IP:8443/setup`, um Ihr Admin-Konto zu erstellen.

> Für eine detaillierte Server-Einrichtung (Sicherheitshärtung, Firewall, Docker-Installation) siehe [INSTALL.md](../../INSTALL.md).

## Tech-Stack

| Schicht | Technologie | Zweck |
|---|---|---|
| Sprache | [Rust](https://www.rust-lang.org) | Performance, Sicherheit, einzelnes Binary |
| HTTP | [Axum](https://github.com/tokio-rs/axum) | Asynchrone API + WebSocket |
| Docker | [Bollard](https://github.com/fussybeaver/bollard) | Docker Engine API |
| Datenbank | [SQLite](https://github.com/rusqlite/rusqlite) | Eingebettete Persistenz |
| Proxy | [Traefik](https://traefik.io) | Auto-Routing + Let's Encrypt |
| Templates | [MiniJinja](https://github.com/mitsuhiko/minijinja) | Serverseitiges Rendering |
| Frontend | [HTMX](https://htmx.org) + [Alpine.js](https://alpinejs.dev) | Minimales JS, Echtzeit |
| Styling | [Tailwind CSS](https://tailwindcss.com) | Dark Mode, responsiv |
| Laufzeit | [Tokio](https://tokio.rs) | Asynchrone I/O |
| Speicher | [AWS S3](https://crates.io/crates/aws-sdk-s3) | Backup-Speicher |
| Auth | JWT + bcrypt | Zustandslose Authentifizierung |

## Architektur

```
                    ┌──────────────────────────────────┐
                    │       Pier  (single binary)       │
                    │                                    │
  Browser ───────►  │  Axum ──► API routes (100+)        │
                    │    │                                │
                    │    ├──► MiniJinja ──► HTML (HTMX)   │
                    │    ├──► Bollard ──► Docker Engine    │
                    │    ├──► rusqlite ──► SQLite          │
                    │    └──► reqwest ──► Remote Agents    │
                    └──────────────────────────────────┘
                                    │
                    ┌───────────────┴────────────────┐
                    │     Traefik  (reverse proxy)    │
                    │   Let's Encrypt · Auto-routing   │
                    └────────────────────────────────┘
```

> Für eine detaillierte Architekturübersicht siehe [ARCHITECTURE.md](../../ARCHITECTURE.md).

## Roadmap

- [x] Container-Verwaltung (Docker API)
- [x] Docker-Compose-Stacks mit YAML-Editor
- [x] Ein-Klick-Service-Vorlagen (30+)
- [x] Reverse Proxy + automatisches SSL (Traefik + Let's Encrypt)
- [x] Git-Webhooks + Auto-Deploy (GitHub, GitLab)
- [x] Multi-Server-Verwaltung mit Agenten
- [x] Backup-Planer mit S3-Unterstützung
- [x] Web-Dashboard (HTMX + Tailwind, Dark Mode)
- [x] S3-Bucket-Verwaltung
- [x] Architektur-Visualisierung (Canvas)
- [ ] RBAC (rollenbasierte Zugriffskontrolle)
- [ ] 2FA (TOTP + WebAuthn)
- [ ] Lastverteilung + horizontale Skalierung
- [ ] Benachrichtigungen (Telegram, Discord, Slack)
- [ ] Automatischer Update-Mechanismus
- [ ] Docker-Netzwerkisolierung pro Projekt
- [ ] Pingora-basierter Reverse Proxy (Traefik ersetzen)

## Mitwirken

Wir freuen uns über Beiträge! Bitte lesen Sie [CONTRIBUTING.md](../../CONTRIBUTING.md), bevor Sie einen Pull Request einreichen. Alle Mitwirkenden müssen unserer [CLA](../../CLA.md) zustimmen.

```bash
cargo fmt          # Code formatieren
cargo clippy       # Linting
cargo test         # Tests ausführen
cargo build        # Build erstellen
```

## Lizenz

[AGPL-3.0](../../LICENSE)

Pier ist kostenlos zum Selbsthosten und Modifizieren. Wenn Sie eine modifizierte Version als Netzwerkdienst anbieten, müssen Sie Ihre Änderungen unter derselben Lizenz veröffentlichen.

Für kommerzielle Lizenzierung (Nutzung ohne AGPL-Verpflichtungen) kontaktieren Sie [info@devcom.app](mailto:info@devcom.app).

---

<p align="center">
  <sub>Gebaut mit 🦀 Rust — schnell, sicher, leichtgewichtig.</sub>
</p>
