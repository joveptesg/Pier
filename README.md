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

## Quick Start

### Option A: One-command install (Ubuntu/Debian)

```bash
curl -fsSL https://pier.team/install | sudo bash
```

> The short URL redirects to [`scripts/bootstrap.sh`](scripts/bootstrap.sh). The script installs Docker, downloads the latest release binary (with sha256 verification), and runs `install.sh`. Re-run it anytime to update to the latest release.

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

### Option D: Install from a pre-built release (no build)

Already have Docker? Grab the latest pre-built binary directly — no Rust toolchain, no compilation:

```bash
# 1. Download the pre-built binary + checksum (linux/amd64)
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64 -o pier-linux-amd64
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64.sha256 -o pier-linux-amd64.sha256
sha256sum -c pier-linux-amd64.sha256          # verify integrity

# 2. Fetch the installer and run it
curl -fL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/install.sh -o install.sh
chmod +x pier-linux-amd64
sudo bash install.sh --binary ./pier-linux-amd64
```

> The manual equivalent of Option A, minus auto-installing Docker. Requires Docker + Compose already present (see [INSTALL.md](INSTALL.md)). The binary filename must stay `pier-linux-amd64` so `sha256sum -c` matches.

### Updating Pier

Updates pull a fresh **pre-built binary** — no rebuild from source needed. `install.sh` detects the running service, stops it, swaps the binary, and restarts it, preserving your `.env` and `/opt/pier/data`.

```bash
# Easiest — re-run the one-command installer (re-downloads the latest release):
curl -fsSL https://pier.team/install | sudo bash

# Or manually, same flow as Option D (download → verify → install.sh).
```

Then open `http://YOUR_SERVER_IP:8443/setup` to create your admin account.

> For detailed server setup (security hardening, firewall, Docker installation), see [INSTALL.md](INSTALL.md).

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
- ✨ **Auto-build (Railpack)** — zero-config builds from source for Node, Python, Go, PHP, Java, Ruby, Rust, Vite/Astro/CRA and more, no Dockerfile required
- ⏪ Deployment history with rollback

**Networking & SSL**
- 🌐 Reverse proxy via Traefik with automatic HTTPS
- 🔒 Let's Encrypt SSL certificates (auto-provisioned)
- 🔗 Custom domains with auto-generated service URLs

**Infrastructure**
- 🖥 Multi-server management with remote agents
- 💾 Scheduled backups with S3 integration
- 📊 Real-time monitoring — CPU, RAM, Disk, Network
- 🗄 Built-in **data editor** — browse tables/collections and run SQL/Mongo/Redis queries from the dashboard (PostgreSQL, MySQL/MariaDB, MongoDB, Redis)

**Developer Experience**
- ⚡ Web UI built with HTMX + Alpine.js — dark mode, real-time, responsive
- 🔑 JWT authentication with bcrypt password hashing
- 🗃 Embedded SQLite — no external database required
- ⚙️ One-command server setup

## npm Registry

**A private + proxy npm registry, built into the binary.** No Verdaccio container, no extra database — Pier serves an npm-compatible API at `/registry/npm/`, mirrors `registry.npmjs.org` transparently, and works with every modern package manager.

### Supported clients

| Client | Versions | Notes |
|---|---|---|
| **npm** | 7 – 11 | Works out of the box |
| **yarn classic** | 1.22.x | Add `always-auth=true` to `.npmrc` |
| **yarn berry** | 2 · 3 · 4 | `.yarnrc.yml` with `npmAlwaysAuth: true` |
| **pnpm** | 9 · 10 | Works out of the box |
| **bun** | latest | Works out of the box |

### Supported commands

| Command | Status |
|---|---|
| `npm install` / `yarn add` / `pnpm add` / `bun add` | ✓ |
| `npm publish` (scoped + unscoped) | ✓ |
| `npm login` (CouchDB flow + `--auth-type=web`) | ✓ |
| `npm dist-tag add / rm / ls` | ✓ |
| `npm deprecate` | ✓ |
| `npm unpublish` (single version + whole package) | ✓ |
| `npm whoami` · `npm ping` | ✓ |

### Upstream proxy mode

Pier can transparently mirror **npmjs.org** (or any npm-compatible upstream) so the whole team uses one URL. Packuments are cached and revalidated through `If-None-Match`, tarballs are pulled lazily on first install, and a background LRU GC keeps the on-disk cache under a configurable cap. Manage everything in **Packages → Upstream proxy**.

- One `.npmrc` URL for the whole team — no scope routing
- Installs keep working when `npmjs.org` is down
- Audit trail: every public package the team actually uses
- TTL revalidation with 304 short-circuit

### Quick start

```ini
# .npmrc in your project
registry=https://YOUR-PIER-HOST/registry/npm/
//YOUR-PIER-HOST/registry/npm/:_authToken=pier_npm_xxx
always-auth=true
```

Mint the token in **Packages → Manage tokens**, then:

```bash
npm publish                  # private package
npm install left-pad         # proxied from npmjs.org + cached
```

Full per-client guides: [npm](https://pier.team/docs/registry/clients/npm) · [yarn 1.x](https://pier.team/docs/registry/clients/yarn-classic) · [yarn 2/3/4](https://pier.team/docs/registry/clients/yarn-berry) · [pnpm](https://pier.team/docs/registry/clients/pnpm) · [bun](https://pier.team/docs/registry/clients/bun).

## Auto-build (Railpack)

The **Auto-build** source lets you deploy from a Git repository **without writing a Dockerfile**. Under the hood Pier shells out to [Railpack](https://github.com/railwayapp/railpack) (Railway's open-source builder, successor to Nixpacks) running against a local [moby/buildkit](https://github.com/moby/buildkit) daemon. Both are provisioned automatically by `install.sh`. See the [from-railpack guide](https://pier.team/docs/applications/from-railpack) for the full walkthrough.

> ### ⚠ Server requirements — please read before enabling
>
> Auto-build is **substantially heavier** than the other deploy paths. Compiling user code on the host is fundamentally different from just running a pre-built container, so the resource picture changes:
>
> |              | Dockerfile / Compose / Docker Image | Auto-build (Railpack) |
> |---|---|---|
> | Minimum RAM  | 512 MB                              | **4 GB** (8 GB for Rust) |
> | Free disk    | a few GB per stack                  | **40+ GB** (BuildKit cache) |
> | First deploy | seconds                             | 1–10 minutes |
>
> **If your VPS has less than 4 GB RAM, use the Dockerfile or Docker Image source instead.** The UI shows a hard warning when the host has &lt;4 GB and the build will almost certainly OOM-kill itself or another process. Pier-core prunes the BuildKit cache daily back to ~10 GB / 7-day retention; you can also `PIER_SKIP_RAILPACK=1 bash install.sh` to skip provisioning entirely.

**What Railpack detects** (no manual config needed for any of these):

| Language / framework | Auto-detected from |
|---|---|
| Node.js / Bun / Deno | `package.json`, `bun.lockb`, `deno.json` |
| Python | `requirements.txt`, `pyproject.toml`, `Pipfile` |
| Go | `go.mod` |
| Rust | `Cargo.toml` |
| PHP | `composer.json` |
| Java | `pom.xml`, `build.gradle` |
| Ruby | `Gemfile` |
| Elixir | `mix.exs` |
| Vite / Astro / CRA static sites | their bundler config + build output dir |

For projects that need overrides, drop a [`railpack.json`](https://railpack.com/configuration/file) in the repo root — Railpack picks it up automatically.

**Tuning knobs** (set in the systemd unit or before `install.sh`):

- `PIER_RAILPACK_MAX_PARALLEL_BUILDS=N` — cap concurrent builds (default 1). Can also be set from `Settings → Auto-build (Railpack)` in the UI.
- `PIER_BUILDKIT_MEMORY=4g` — RAM limit for the buildkit container (default 4g).
- `PIER_SKIP_RAILPACK=1` — skip provisioning entirely; the feature card stays in the UI but shows a clear "railpack binary not found" message on build.

**FAQ**

- **Why not Nixpacks?** Railpack is the active successor (Railway moved to it in March 2025); Nixpacks is in maintenance mode. Railpack produces ~38% smaller Node images and ~77% smaller Python images thanks to its BuildKit-graph approach.
- **Does it work on ARM/aarch64?** Yes — both `railpack` and `moby/buildkit` ship linux/arm64 binaries. The install script picks the right architecture automatically.
- **Can I disable it?** Yes — `PIER_SKIP_RAILPACK=1 bash install.sh` skips provisioning. You can still use Dockerfile / Compose / Docker Image sources.

## Data editor

**Browse and query your databases from the dashboard — no Adminer, no pgweb, no external client.** Every database service gets a **Data** tab: explore the schema, page through rows, and run queries inline. Built into the binary, gated by RBAC, and every query is audited.

### Supported engines

| Engine | Driver | Browse | Query runner |
|---|---|---|---|
| **PostgreSQL** (incl. PostGIS, TimescaleDB) | native `sqlx` | schemas · tables · views · structure · rows | arbitrary SQL |
| **MySQL / MariaDB** | native `sqlx` | databases · tables · views · structure · rows | arbitrary SQL |
| **MongoDB** | `mongosh` (docker-exec) | databases · collections · documents | `mongosh` scripts |
| **Redis / Valkey** | native `redis` | keys (SCAN) · type-aware values · TTL | raw commands |

### Browse

- **SQL** — schema/table tree, per-table structure (columns, types, nullability, defaults, primary keys, indexes), and paginated rows with a total count.
- **MongoDB** — database → collection tree, paginated documents rendered as EJSON.
- **Redis** — `SCAN`-based key browser with per-key type, a type-aware value view (string / list / set / zset / hash / stream) and TTL; switch between DBs 0–15.

### Query

- **SQL Runner** — run any statement against PostgreSQL or MySQL/MariaDB. Reads return a grid (capped at 1,000 rows); writes report the affected-row count. A 15-second statement timeout keeps a runaway query from pinning the database.
- **Mongo Shell** — run any `mongosh` script against the selected database.
- **Redis commands** — run any command (`GET`, `HGETALL`, `TTL`, …) and read the reply as JSON.

### Access & audit

- **Read** (browse, structure, rows) requires `Viewer`; **write** (any runner) requires `Editor` — enforced per-resource by Pier's RBAC.
- Every runner execution is recorded in the `db_query_log` audit table — who ran what, against which database, with status, row count and duration.
- Connections use credentials decrypted from the service's encrypted env. Private databases are reached over the `pier-net` Docker network, so no port has to be published to the host.

## Templates

**Databases** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**Services** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**Games** — Minecraft, Terraria

**VPN** — AmneziaWG

**Applications** — Deploy from Dockerfile, Docker image, or Docker Compose

> Can't find what you need? Deploy any Docker image or Compose stack manually.

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
- [x] RBAC (role-based access control)
- [x] 2FA (TOTP + recovery codes)
- [x] Load balancing + horizontal scaling
- [x] Alert notifications (Telegram, Discord, Slack, Email)
- [x] Auto-update mechanism
- [x] Docker network isolation per project
- [x] Built-in data editor (PostgreSQL, MySQL/MariaDB, MongoDB, Redis)
- [ ] WebAuthn / passkeys (second 2FA factor)
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
