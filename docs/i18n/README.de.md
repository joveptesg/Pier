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

**Pier ist eine quelloffene & selbst gehostete Alternative zu Coolify / Heroku / Vercel — leichtgewichtig genug für einen $5-VPS.**

Deployen Sie Container, Docker-Compose-Stacks und Git-Repositories mit automatischem SSL, Reverse Proxy und einem modernen Web-Dashboard — alles aus einem einzigen Rust-Binary mit nur **20–40 MB RAM**.

<!-- 
<p align="center">
  <img src="../../docs/screenshots/dashboard.png" alt="Pier Dashboard" width="800">
</p>
-->

## Schnellstart

### Option A: Ein-Befehl-Installation (Ubuntu/Debian)

```bash
curl -fsSL https://pier.team/install | sudo bash
```

> Die Kurz-URL leitet auf [`scripts/bootstrap.sh`](../../scripts/bootstrap.sh) weiter. Das Skript installiert Docker, lädt das neueste Release-Binary herunter (mit sha256-Prüfung) und führt `install.sh` aus. Führen Sie es jederzeit erneut aus, um auf das neueste Release zu aktualisieren.

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

### Option D: Aus einem vorgefertigten Release installieren (kein Build)

Sie haben Docker bereits? Holen Sie sich das neueste vorgefertigte Binary direkt — keine Rust-Toolchain, keine Kompilierung:

```bash
# 1. Vorgefertigtes Binary + Prüfsumme herunterladen (linux/amd64)
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64 -o pier-linux-amd64
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64.sha256 -o pier-linux-amd64.sha256
sha256sum -c pier-linux-amd64.sha256          # Integrität prüfen

# 2. Installer holen und ausführen
curl -fL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/install.sh -o install.sh
chmod +x pier-linux-amd64
sudo bash install.sh --binary ./pier-linux-amd64
```

> Das manuelle Äquivalent zu Option A, abzüglich der automatischen Docker-Installation. Setzt voraus, dass Docker + Compose bereits vorhanden sind (siehe [INSTALL.md](../../INSTALL.md)). Der Binary-Dateiname muss `pier-linux-amd64` bleiben, damit `sha256sum -c` passt.

### Pier aktualisieren

Updates ziehen ein frisches **vorgefertigtes Binary** — kein erneuter Build aus dem Quellcode nötig. `install.sh` erkennt den laufenden Dienst, stoppt ihn, tauscht das Binary aus und startet ihn neu, wobei Ihre `.env` und `/opt/pier/data` erhalten bleiben.

```bash
# Am einfachsten — den Ein-Befehl-Installer erneut ausführen (lädt das neueste Release erneut herunter):
curl -fsSL https://pier.team/install | sudo bash

# Oder manuell, gleicher Ablauf wie bei Option D (herunterladen → prüfen → install.sh).
```

Öffnen Sie anschließend `http://IHRE_SERVER_IP:8443/setup`, um Ihr Admin-Konto zu erstellen.

> Für eine detaillierte Server-Einrichtung (Sicherheitshärtung, Firewall, Docker-Installation) siehe [INSTALL.md](../../INSTALL.md).

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
- ✨ **Auto-Build (Railpack)** — Zero-Config-Builds direkt aus dem Quellcode für Node, Python, Go, PHP, Java, Ruby, Rust, Vite/Astro/CRA und mehr, kein Dockerfile nötig
- ⏪ Deployment-Verlauf mit Rollback

**Netzwerk & SSL**
- 🌐 Reverse Proxy über Traefik mit automatischem HTTPS
- 🔒 Let's Encrypt SSL-Zertifikate (automatisch bereitgestellt)
- 🔗 Eigene Domains mit automatisch generierten Service-URLs

**Infrastruktur**
- 🖥 Multi-Server-Verwaltung mit Remote-Agenten
- 💾 Geplante Backups mit S3-Integration
- 📊 Echtzeit-Monitoring — CPU, RAM, Festplatte, Netzwerk
- 🗄 Integrierter **Daten-Editor** — Tabellen/Collections durchsuchen und SQL/Mongo/Redis-Abfragen direkt im Dashboard ausführen (PostgreSQL, MySQL/MariaDB, MongoDB, Redis)

**Entwickler-Erfahrung**
- ⚡ Web-UI mit HTMX + Alpine.js — Dark Mode, Echtzeit, responsiv
- 🔑 JWT-Authentifizierung mit bcrypt-Passwort-Hashing
- 🗃 Eingebettete SQLite — keine externe Datenbank erforderlich
- ⚙️ Server-Einrichtung mit einem Befehl

## npm Registry

**Private + Proxy-npm-Registry, direkt im Binary integriert.** Kein Verdaccio-Container, keine zusätzliche Datenbank — Pier stellt unter `/registry/npm/` eine npm-kompatible API bereit, spiegelt `registry.npmjs.org` transparent und funktioniert mit jedem modernen Paketmanager.

### Unterstützte Clients

| Client | Versionen | Hinweise |
|---|---|---|
| **npm** | 7 – 11 | Funktioniert out-of-the-box |
| **yarn classic** | 1.22.x | `always-auth=true` in `.npmrc` setzen |
| **yarn berry** | 2 · 3 · 4 | `.yarnrc.yml` mit `npmAlwaysAuth: true` |
| **pnpm** | 9 · 10 | Funktioniert out-of-the-box |
| **bun** | latest | Funktioniert out-of-the-box |

### Unterstützte Befehle

| Befehl | Status |
|---|---|
| `npm install` / `yarn add` / `pnpm add` / `bun add` | ✓ |
| `npm publish` (scoped + unscoped) | ✓ |
| `npm login` (CouchDB-Flow + `--auth-type=web`) | ✓ |
| `npm dist-tag add / rm / ls` | ✓ |
| `npm deprecate` | ✓ |
| `npm unpublish` (einzelne Version + ganzes Paket) | ✓ |
| `npm whoami` · `npm ping` | ✓ |

### Upstream-Proxy-Modus

Pier kann **npmjs.org** (oder jede npm-kompatible Upstream-Registry) transparent spiegeln — das ganze Team benutzt eine URL. Packument-Metadaten werden gecacht und via `If-None-Match` revalidiert, Tarballs werden beim ersten `install` lazy nachgezogen, ein Hintergrund-LRU-GC hält den On-Disk-Cache unter einem konfigurierbaren Limit. Verwaltung unter **Packages → Upstream proxy**.

- Eine `.npmrc`-URL für das gesamte Team — kein Scope-Routing
- `install` funktioniert auch bei Ausfall von `npmjs.org`
- Audit: Sichtbar, welche öffentlichen Pakete das Team tatsächlich nutzt
- TTL-Revalidierung mit 304-Kurzschluss

### Schnellstart

```ini
# .npmrc im Projekt
registry=https://YOUR-PIER-HOST/registry/npm/
//YOUR-PIER-HOST/registry/npm/:_authToken=pier_npm_xxx
always-auth=true
```

Token unter **Packages → Manage tokens** erzeugen, dann:

```bash
npm publish                  # privates Paket
npm install left-pad         # via Proxy von npmjs.org + gecacht
```

Vollständige Anleitungen pro Client: [npm](https://pier.team/docs/registry/clients/npm) · [yarn 1.x](https://pier.team/docs/registry/clients/yarn-classic) · [yarn 2/3/4](https://pier.team/docs/registry/clients/yarn-berry) · [pnpm](https://pier.team/docs/registry/clients/pnpm) · [bun](https://pier.team/docs/registry/clients/bun).

## Auto-Build (Railpack)

Die Quelle **Auto-Build** ermöglicht das Deployment direkt aus einem Git-Repository, **ohne ein Dockerfile zu schreiben**. Im Hintergrund delegiert Pier an [Railpack](https://github.com/railwayapp/railpack) (Railways Open-Source-Builder, Nachfolger von Nixpacks), der mit einem lokalen [moby/buildkit](https://github.com/moby/buildkit)-Daemon arbeitet. Beide Komponenten werden automatisch von `install.sh` bereitgestellt. Eine vollständige Anleitung steht im [from-railpack-Guide](https://pier.team/docs/applications/from-railpack).

> ### ⚠ Server-Anforderungen — bitte vor dem Aktivieren lesen
>
> Auto-Build ist **deutlich ressourcenintensiver** als die anderen Deploy-Pfade. Kompilieren von Benutzercode auf dem Host unterscheidet sich grundlegend vom bloßen Ausführen eines vorgefertigten Containers — entsprechend ändert sich das Ressourcenprofil:
>
> |              | Dockerfile / Compose / Docker Image | Auto-Build (Railpack) |
> |---|---|---|
> | Mindest-RAM   | 512 MB                              | **4 GB** (8 GB für Rust) |
> | Freier Speicher | wenige GB pro Stack               | **40+ GB** (BuildKit-Cache) |
> | Erstes Deploy | Sekunden                            | 1–10 Minuten |
>
> **Wenn Ihr VPS weniger als 4 GB RAM hat, verwenden Sie stattdessen die Quellen Dockerfile oder Docker Image.** Die UI zeigt eine deutliche Warnung, sobald der Host unter 4 GB liegt — der Build wird fast sicher per OOM-Kill abgewürgt (entweder der Build selbst oder ein anderer Prozess). Pier-core schneidet den BuildKit-Cache täglich auf ~10 GB / 7 Tage Aufbewahrung zurück. Mit `PIER_SKIP_RAILPACK=1 bash install.sh` kann die Bereitstellung auch komplett übersprungen werden.

**Was Railpack automatisch erkennt** (keine manuelle Konfiguration erforderlich):

| Sprache / Framework | Erkennung anhand |
|---|---|
| Node.js / Bun / Deno | `package.json`, `bun.lockb`, `deno.json` |
| Python | `requirements.txt`, `pyproject.toml`, `Pipfile` |
| Go | `go.mod` |
| Rust | `Cargo.toml` |
| PHP | `composer.json` |
| Java | `pom.xml`, `build.gradle` |
| Ruby | `Gemfile` |
| Elixir | `mix.exs` |
| Vite / Astro / CRA (statische Sites) | Bundler-Konfig + Build-Output-Verzeichnis |

Für Projekte, die Overrides brauchen, legen Sie eine [`railpack.json`](https://railpack.com/configuration/file) ins Repository-Root — Railpack greift sie automatisch auf.

**Tuning-Schalter** (in der systemd-Unit oder vor `install.sh` setzen):

- `PIER_RAILPACK_MAX_PARALLEL_BUILDS=N` — Limit für parallele Builds (Standard 1). Lässt sich auch über die UI unter `Einstellungen → Auto-build (Railpack)` ändern.
- `PIER_BUILDKIT_MEMORY=4g` — RAM-Limit für den buildkit-Container (Standard 4g).
- `PIER_SKIP_RAILPACK=1` — Bereitstellung vollständig überspringen. Die Karte bleibt in der UI sichtbar, zeigt aber beim Build die klare Meldung "railpack binary not found".

**FAQ**

- **Warum nicht Nixpacks?** Railpack ist der aktive Nachfolger (Railway hat im März 2025 umgestellt); Nixpacks ist im Maintenance-Modus. Railpack erzeugt dank seines BuildKit-Graph-Ansatzes ca. 38 % kleinere Node-Images und ca. 77 % kleinere Python-Images.
- **Läuft es auf ARM/aarch64?** Ja — sowohl `railpack` als auch `moby/buildkit` liefern linux/arm64-Binärdateien. Das Installationsskript wählt die richtige Architektur automatisch.
- **Kann ich es deaktivieren?** Ja — `PIER_SKIP_RAILPACK=1 bash install.sh` überspringt die Bereitstellung. Dockerfile / Compose / Docker Image bleiben uneingeschränkt nutzbar.

## Vorlagen

**Datenbanken** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**Dienste** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**Spiele** — Minecraft, Terraria

**VPN** — AmneziaWG

**Anwendungen** — Deployment aus Dockerfile, Docker-Image oder Docker Compose

> Sie finden nicht, was Sie brauchen? Deployen Sie jedes Docker-Image oder jeden Compose-Stack manuell.

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
- [x] Integrierter Daten-Editor (PostgreSQL, MySQL/MariaDB, MongoDB, Redis)
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
