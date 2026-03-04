# Pier

**Lightweight self-hosted PaaS written in Rust.**

Deploy containers, manage domains, auto-SSL — all from a single binary.

## Why Pier?

[Coolify](https://coolify.io) is great, but it consumes **750 MB - 1.2 GB RAM** idle (Laravel + PostgreSQL + Redis + Soketi + Horizon + Traefik). Pier aims to provide the same core functionality in a single Rust binary using **~20-40 MB RAM**.

| | Coolify | Pier (target) |
|---|---|---|
| RAM idle | 750 MB - 1.2 GB | 20-40 MB (+50-100 MB Traefik) |
| Disk | ~1 GB | ~15-30 MB |
| Containers | 6+ | 1 (single binary) |
| Min VPS | 2 GB RAM, 2 vCPU | 512 MB RAM, 1 vCPU |

## Planned Features

- **Container Management** — Docker containers & Compose stacks via [Bollard](https://github.com/fussybeaver/bollard)
- **Reverse Proxy** — Auto-routing via Docker labels (Traefik) with Let's Encrypt SSL
- **Git Auto-Deploy** — Webhook-triggered builds from GitHub/GitLab
- **Multi-Server** — Deploy to remote servers via agents with mTLS
- **Real-Time Monitoring** — CPU, RAM, Disk, Network per server/container/project
- **Template Catalog** — One-click deploy PostgreSQL, Redis, MongoDB, and more
- **Admin Web UI** — HTMX + Tailwind, embedded in the binary (30 KB JS)
- **Security** — 2FA (TOTP/WebAuthn), envelope encryption for secrets, CrowdSec

## Tech Stack

| Component | Technology |
|---|---|
| Language | Rust |
| HTTP/API | [Axum](https://github.com/tokio-rs/axum) |
| Docker API | [Bollard](https://github.com/fussybeaver/bollard) |
| Reverse Proxy | [Traefik](https://traefik.io) (MVP), [Pingora](https://github.com/cloudflare/pingora) (future) |
| Database | SQLite via [rusqlite](https://github.com/rusqlite/rusqlite) |
| Git | [gix (gitoxide)](https://github.com/GitoxideLabs/gitoxide) |
| SSH | [russh](https://github.com/Eugeny/russh) |
| TLS/ACME | [rustls-acme](https://crates.io/crates/rustls-acme) |
| Templates | [MiniJinja](https://github.com/mitsuhiko/minijinja) + [Tailwind CSS](https://tailwindcss.com) |
| Async Runtime | [Tokio](https://tokio.rs) |

## Status

> **Early development** — not ready for production use.

## Development

```bash
# Build
cargo build

# Run (development)
cargo run -p pier-core

# Run agent
cargo run -p pier-agent
```

## License

[AGPL-3.0](LICENSE)

Pier is free to self-host. If you modify Pier and offer it as a network service, you must share your modifications under the same license.

For commercial licensing (use without AGPL obligations), [contact us](mailto:info@devcom.app).
