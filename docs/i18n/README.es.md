> Traducción de [README.md](../../README.md). En caso de discrepancias, consulte la versión en inglés.

<p align="center">
  <img src="../logo.svg" alt="Pier" height="120">
</p>

<h3 align="center">Un PaaS ligero y autoalojado.<br>Un solo binario. 20 MB de RAM. Despliega lo que sea.</h3>

<p align="center">
  <a href="https://github.com/joveptesg/pier/blob/main/LICENSE"><img src="https://img.shields.io/github/license/joveptesg/pier?color=blue" alt="Licencia"></a>
  <a href="https://github.com/joveptesg/pier/stargazers"><img src="https://img.shields.io/github/stars/joveptesg/pier?style=flat" alt="Estrellas"></a>
  <a href="https://github.com/joveptesg/pier/releases"><img src="https://img.shields.io/github/v/release/joveptesg/pier" alt="Versión"></a>
  <img src="https://img.shields.io/badge/rust-1.93%2B-orange" alt="Rust">
</p>

<p align="center">
  <a href="../../README.md">English</a> |
  <a href="README.ru.md">Русский</a> |
  <a href="README.zh-CN.md">中文</a> |
  <a href="README.de.md">Deutsch</a> |
  <a href="README.ja.md">日本語</a> |
  <strong>Español</strong> |
  <a href="README.fr.md">Français</a> |
  <a href="README.pt-BR.md">Português</a>
</p>

---

## ¿Qué es Pier?

**Pier es una alternativa open-source y autoalojada a Coolify / Heroku / Vercel — lo suficientemente ligera para un VPS de $5.**

Despliega contenedores, stacks de Docker Compose y repositorios Git con SSL automático, proxy inverso y un panel web moderno — todo desde un único binario de Rust usando apenas **20–40 MB de RAM**.

<!-- 
<p align="center">
  <img src="../screenshots/dashboard.png" alt="Panel de Pier" width="800">
</p>
-->

## ¿Por qué Pier?

[Coolify](https://coolify.io) es genial, pero ejecuta **más de 6 contenedores** y consume **750 MB – 1.2 GB de RAM** en reposo. Pier ofrece las mismas funciones principales en un solo binario.

| | Coolify | Pier |
|---|---|---|
| **RAM en reposo** | 750 MB – 1.2 GB | 20–40 MB (+Traefik) |
| **Disco** | ~1 GB | ~15–30 MB |
| **Contenedores en ejecución** | 6+ (Laravel, PostgreSQL, Redis, Soketi, Horizon, Traefik) | 1 binario + Traefik |
| **VPS mínimo** | 2 GB RAM, 2 vCPU | 512 MB RAM, 1 vCPU |
| **Base de datos** | PostgreSQL externo | SQLite integrado |
| **Lenguaje** | PHP / Laravel | Rust |
| **JS del frontend** | ~300 KB+ | ~30 KB (HTMX + Alpine.js) |

## Características

**Contenedores y Stacks**
- 📦 Gestión de contenedores Docker — crear, iniciar, detener, reiniciar, eliminar, logs, estadísticas
- 🐳 Stacks de Docker Compose con editor YAML integrado
- 🚀 Despliegue con un clic desde **más de 30 plantillas**

**Git y Despliegues**
- 🔄 Pipeline de Git a despliegue con webhooks de GitHub y GitLab
- 🛠 Construcción desde Dockerfile, imagen Docker o Compose
- ⏪ Historial de despliegues con reversión

**Red y SSL**
- 🌐 Proxy inverso mediante Traefik con HTTPS automático
- 🔒 Certificados SSL de Let's Encrypt (aprovisionados automáticamente)
- 🔗 Dominios personalizados con URLs de servicio autogeneradas

**Infraestructura**
- 🖥 Gestión multi-servidor con agentes remotos
- 💾 Respaldos programados con integración S3
- 📊 Monitoreo en tiempo real — CPU, RAM, Disco, Red

**Experiencia de desarrollo**
- ⚡ Interfaz web construida con HTMX + Alpine.js — modo oscuro, tiempo real, responsive
- 🔑 Autenticación JWT con hash de contraseñas bcrypt
- 🗃 SQLite integrado — sin base de datos externa requerida
- ⚙️ Configuración del servidor con un solo comando

## Plantillas

**Bases de datos** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**Servicios** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**Juegos** — Minecraft, Terraria

**VPN** — AmneziaWG

**Aplicaciones** — Despliegue desde Dockerfile, imagen Docker o Docker Compose

> ¿No encuentras lo que necesitas? Despliega cualquier imagen Docker o stack de Compose manualmente.

## Inicio rápido

### Opción A: Instalación con un solo comando (Ubuntu/Debian)

```bash
curl -fsSL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/setup.sh | sudo bash
```

### Opción B: Compilar desde el código fuente

```bash
git clone https://github.com/joveptesg/pier.git
cd pier
cargo build --release
sudo bash scripts/install.sh --binary target/release/pier
```

### Opción C: Docker

```bash
docker run -d \
  --name pier \
  -p 8443:8443 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v pier-data:/app/data \
  ghcr.io/joveptesg/pier:latest
```

Luego abre `http://TU_IP_DEL_SERVIDOR:8443/setup` para crear tu cuenta de administrador.

> Para una configuración detallada del servidor (hardening de seguridad, firewall, instalación de Docker), consulta [INSTALL.md](../../INSTALL.md).

## Stack tecnológico

| Capa | Tecnología | Propósito |
|---|---|---|
| Lenguaje | [Rust](https://www.rust-lang.org) | Rendimiento, seguridad, binario único |
| HTTP | [Axum](https://github.com/tokio-rs/axum) | API asíncrona + WebSocket |
| Docker | [Bollard](https://github.com/fussybeaver/bollard) | API del Docker Engine |
| Base de datos | [SQLite](https://github.com/rusqlite/rusqlite) | Persistencia integrada |
| Proxy | [Traefik](https://traefik.io) | Enrutamiento automático + Let's Encrypt |
| Plantillas | [MiniJinja](https://github.com/mitsuhiko/minijinja) | Renderizado del lado del servidor |
| Frontend | [HTMX](https://htmx.org) + [Alpine.js](https://alpinejs.dev) | JS mínimo, tiempo real |
| Estilos | [Tailwind CSS](https://tailwindcss.com) | Modo oscuro, responsive |
| Runtime | [Tokio](https://tokio.rs) | E/S asíncrona |
| Almacenamiento | [AWS S3](https://crates.io/crates/aws-sdk-s3) | Almacenamiento de respaldos |
| Autenticación | JWT + bcrypt | Autenticación sin estado |

## Arquitectura

```
                    ┌──────────────────────────────────┐
                    │       Pier  (binario único)       │
                    │                                    │
  Navegador ─────►  │  Axum ──► Rutas API (100+)         │
                    │    │                                │
                    │    ├──► MiniJinja ──► HTML (HTMX)   │
                    │    ├──► Bollard ──► Docker Engine    │
                    │    ├──► rusqlite ──► SQLite          │
                    │    └──► reqwest ──► Agentes remotos  │
                    └──────────────────────────────────┘
                                    │
                    ┌───────────────┴────────────────┐
                    │     Traefik  (proxy inverso)    │
                    │   Let's Encrypt · Auto-routing   │
                    └────────────────────────────────┘
```

> Para la arquitectura detallada, consulta [ARCHITECTURE.md](../../ARCHITECTURE.md).

## Hoja de ruta

- [x] Gestión de contenedores (API de Docker)
- [x] Stacks de Docker Compose con editor YAML
- [x] Plantillas de servicios con un clic (30+)
- [x] Proxy inverso + SSL automático (Traefik + Let's Encrypt)
- [x] Webhooks de Git + despliegue automático (GitHub, GitLab)
- [x] Gestión multi-servidor con agentes
- [x] Programador de respaldos con soporte S3
- [x] Panel web (HTMX + Tailwind, modo oscuro)
- [x] Gestión de buckets S3
- [x] Visualización de arquitectura (Canvas)
- [ ] RBAC (control de acceso basado en roles)
- [ ] 2FA (TOTP + WebAuthn)
- [ ] Balanceo de carga + escalado horizontal
- [ ] Notificaciones de alertas (Telegram, Discord, Slack)
- [ ] Mecanismo de actualización automática
- [ ] Aislamiento de red Docker por proyecto
- [ ] Proxy inverso basado en Pingora (reemplazo de Traefik)

## Contribuir

¡Las contribuciones son bienvenidas! Por favor lee [CONTRIBUTING.md](../../CONTRIBUTING.md) antes de enviar un pull request. Todos los contribuyentes deben aceptar nuestro [CLA](../../CLA.md).

```bash
cargo fmt          # Formatear código
cargo clippy       # Linter
cargo test         # Ejecutar pruebas
cargo build        # Compilar
```

## Licencia

[AGPL-3.0](../../LICENSE)

Pier es libre para autoalojar y modificar. Si ofreces una versión modificada como servicio en red, debes compartir tus modificaciones bajo la misma licencia.

Para licenciamiento comercial (uso sin obligaciones AGPL), contacta a [info@devcom.app](mailto:info@devcom.app).

---

<p align="center">
  <sub>Construido con 🦀 Rust — rápido, seguro, ligero.</sub>
</p>
