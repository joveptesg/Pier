<p align="center">
  <img src="docs/logo.svg" alt="Pier" height="120">
</p>

<h3 align="center">A lightweight, self-hosted PaaS.<br>Single binary. 20 MB RAM. Deploy anything.</h3>

<p align="center">
  <a href="https://github.com/joveptesg/pier/blob/main/LICENSE"><img src="https://img.shields.io/github/license/joveptesg/pier?color=blue" alt="License"></a>
  <a href="https://github.com/joveptesg/pier/stargazers"><img src="https://img.shields.io/github/stars/joveptesg/pier?style=flat" alt="Stars"></a>
  <a href="https://github.com/joveptesg/pier/releases"><img src="https://img.shields.io/github/v/release/joveptesg/pier" alt="Release"></a>
  <img src="https://img.shields.io/badge/rust-1.93%2B-orange" alt="Rust">
</p>

<p align="center">
  <strong>English</strong> |
  <a href="docs/i18n/README.ru.md">Русский</a> |
  <a href="docs/i18n/README.zh-CN.md">中文</a> |
  <a href="docs/i18n/README.de.md">Deutsch</a> |
  <a href="docs/i18n/README.ja.md">日本語</a> |
  <a href="docs/i18n/README.es.md">Español</a> |
  <a href="docs/i18n/README.fr.md">Français</a> |
  <a href="docs/i18n/README.pt-BR.md">Português</a>
</p>

---

## What is Pier?

**Pier is an open-source & self-hostable alternative to Coolify / Heroku / Vercel — lightweight enough for a $5 VPS.**

Deploy containers, Docker Compose stacks, and Git repositories with automatic SSL, reverse proxy, and a modern web dashboard — all from a single Rust binary using **20–40 MB of RAM**.

<!-- 
<p align="center">
  <img src="docs/screenshots/dashboard.png" alt="Pier Dashboard" width="800">
</p>
-->

## Why Pier?

[Coolify](https://coolify.io) is great, but it runs **6+ containers** and consumes **750 MB – 1.2 GB RAM** idle. Pier delivers the same core features in a single binary.

| | Coolify | Pier |
|---|---|---|
| **Idle RAM** | 750 MB – 1.2 GB | 20–40 MB (+Traefik) |
| **Disk** | ~1 GB | ~15–30 MB |
| **Running containers** | 6+ (Laravel, PostgreSQL, Redis, Soketi, Horizon, Traefik) | 1 binary + Traefik |
| **Minimum VPS** | 2 GB RAM, 2 vCPU | 512 MB RAM, 1 vCPU |
| **Database** | External PostgreSQL | Embedded SQLite |
| **Language** | PHP / Laravel | Rust |
| **Frontend JS** | ~300 KB+ | ~30 KB (HTMX + Alpine.js) |

## Features

**Containers & Stacks**
- 📦 Docker container management — create, start, stop, restart, remove, logs, stats
- 🐳 Docker Compose stacks with built-in YAML editor
- 🚀 One-click deploy from **30+ templates**

**Git & Deployments**
- 🔄 Git-to-deploy pipeline with GitHub & GitLab webhooks
- 🛠 Build from Dockerfile, Docker image, or Compose
- ⏪ Deployment history with rollback

**Networking & SSL**
- 🌐 Reverse proxy via Traefik with automatic HTTPS
- 🔒 Let's Encrypt SSL certificates (auto-provisioned)
- 🔗 Custom domains with auto-generated service URLs

**Infrastructure**
- 🖥 Multi-server management with remote agents
- 💾 Scheduled backups with S3 integration
- 📊 Real-time monitoring — CPU, RAM, Disk, Network

**Developer Experience**
- ⚡ Web UI built with HTMX + Alpine.js — dark mode, real-time, responsive
- 🔑 JWT authentication with bcrypt password hashing
- 🗃 Embedded SQLite — no external database required
- ⚙️ One-command server setup

## Templates

**Databases** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**Services** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**Games** — Minecraft, Terraria

**VPN** — AmneziaWG

**Applications** — Deploy from Dockerfile, Docker image, or Docker Compose

> Can't find what you need? Deploy any Docker image or Compose stack manually.

## Quick Start

### Option A: One-command install (Ubuntu/Debian)

```bash
curl -fsSL https://pier.team/install | sudo bash
```

> The short URL redirects to [`scripts/bootstrap.sh`](scripts/bootstrap.sh). The script installs Docker, downloads the latest release binary (with sha256 verification), and runs `install.sh`.

### Option B: Build from source

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

Then open `http://YOUR_SERVER_IP:8443/setup` to create your admin account.

> For detailed server setup (security hardening, firewall, Docker installation), see [INSTALL.md](INSTALL.md).

## Tech Stack

| Layer | Technology | Purpose |
|---|---|---|
| Language | [Rust](https://www.rust-lang.org) | Performance, safety, single binary |
| HTTP | [Axum](https://github.com/tokio-rs/axum) | Async API + WebSocket |
| Docker | [Bollard](https://github.com/fussybeaver/bollard) | Docker Engine API |
| Database | [SQLite](https://github.com/rusqlite/rusqlite) | Embedded persistence |
| Proxy | [Traefik](https://traefik.io) | Auto-routing + Let's Encrypt |
| Templates | [MiniJinja](https://github.com/mitsuhiko/minijinja) | Server-side rendering |
| Frontend | [HTMX](https://htmx.org) + [Alpine.js](https://alpinejs.dev) | Minimal JS, real-time |
| Styling | [Tailwind CSS](https://tailwindcss.com) | Dark mode, responsive |
| Runtime | [Tokio](https://tokio.rs) | Async I/O |
| Storage | [AWS S3](https://crates.io/crates/aws-sdk-s3) | Backup storage |
| Auth | JWT + bcrypt | Stateless authentication |

## Architecture

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

> For detailed architecture, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Roadmap

- [x] Container management (Docker API)
- [x] Docker Compose stacks with YAML editor
- [x] One-click service templates (30+)
- [x] Reverse proxy + auto-SSL (Traefik + Let's Encrypt)
- [x] Git webhooks + auto-deploy (GitHub, GitLab)
- [x] Multi-server management with agents
- [x] Backup scheduler with S3 support
- [x] Web dashboard (HTMX + Tailwind, dark mode)
- [x] S3 bucket management
- [x] Architecture visualization (Canvas)
- [ ] RBAC (role-based access control)
- [ ] 2FA (TOTP + WebAuthn)
- [ ] Load balancing + horizontal scaling
- [ ] Alert notifications (Telegram, Discord, Slack)
- [ ] Auto-update mechanism
- [ ] Docker network isolation per project
- [ ] Pingora-based reverse proxy (replace Traefik)

## Contributing

We welcome contributions! Please read [CONTRIBUTING.md](CONTRIBUTING.md) before submitting a pull request. All contributors must agree to our [CLA](CLA.md).

```bash
cargo fmt          # Format code
cargo clippy       # Lint
cargo test         # Run tests
cargo build        # Build
```

## License

[AGPL-3.0](LICENSE)

Pier is free to self-host and modify. If you offer a modified version as a network service, you must share your modifications under the same license.

For commercial licensing (use without AGPL obligations), contact [info@devcom.app](mailto:info@devcom.app).

---

<p align="center">
  <sub>Built with 🦀 Rust — fast, safe, lightweight.</sub>
</p>
