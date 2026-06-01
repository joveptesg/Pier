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

## Inicio rápido

### Opción A: Instalación con un solo comando (Ubuntu/Debian)

```bash
curl -fsSL https://pier.team/install | sudo bash
```

> La URL corta redirige a [`scripts/bootstrap.sh`](../../scripts/bootstrap.sh). El script instala Docker, descarga el binario de la última release (con verificación sha256) y ejecuta `install.sh`. Vuelve a ejecutarlo en cualquier momento para actualizar a la última release.

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

### Opción D: Instalar desde una release preconstruida (sin compilar)

¿Ya tienes Docker? Descarga directamente el último binario preconstruido — sin toolchain de Rust, sin compilación:

```bash
# 1. Descarga el binario preconstruido + checksum (linux/amd64)
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64 -o pier-linux-amd64
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64.sha256 -o pier-linux-amd64.sha256
sha256sum -c pier-linux-amd64.sha256          # verifica la integridad

# 2. Obtén el instalador y ejecútalo
curl -fL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/install.sh -o install.sh
chmod +x pier-linux-amd64
sudo bash install.sh --binary ./pier-linux-amd64
```

> El equivalente manual de la Opción A, sin la instalación automática de Docker. Requiere Docker + Compose ya presentes (consulta [INSTALL.md](../../INSTALL.md)). El nombre del archivo del binario debe seguir siendo `pier-linux-amd64` para que `sha256sum -c` coincida.

### Actualizar Pier

Las actualizaciones descargan un **binario preconstruido** nuevo — sin necesidad de recompilar desde el código fuente. `install.sh` detecta el servicio en ejecución, lo detiene, reemplaza el binario y lo reinicia, conservando tu `.env` y `/opt/pier/data`.

```bash
# Lo más fácil — vuelve a ejecutar el instalador de un solo comando (vuelve a descargar la última release):
curl -fsSL https://pier.team/install | sudo bash

# O manualmente, con el mismo flujo que la Opción D (descargar → verificar → install.sh).
```

Luego abre `http://TU_IP_DEL_SERVIDOR:8443/setup` para crear tu cuenta de administrador.

> Para una configuración detallada del servidor (hardening de seguridad, firewall, instalación de Docker), consulta [INSTALL.md](../../INSTALL.md).

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
- ✨ **Auto-build (Railpack)** — construcciones sin configuración directamente desde el código fuente para Node, Python, Go, PHP, Java, Ruby, Rust, Vite/Astro/CRA y más, sin necesidad de Dockerfile
- ⏪ Historial de despliegues con reversión

**Red y SSL**
- 🌐 Proxy inverso mediante Traefik con HTTPS automático
- 🔒 Certificados SSL de Let's Encrypt (aprovisionados automáticamente)
- 🔗 Dominios personalizados con URLs de servicio autogeneradas

**Infraestructura**
- 🖥 Gestión multi-servidor con agentes remotos
- 💾 Respaldos programados con integración S3
- 📊 Monitoreo en tiempo real — CPU, RAM, Disco, Red
- 🗄 **Editor de datos** integrado — explora tablas/colecciones y ejecuta consultas SQL/Mongo/Redis desde el panel (PostgreSQL, MySQL/MariaDB, MongoDB, Redis)

**Experiencia de desarrollo**
- ⚡ Interfaz web construida con HTMX + Alpine.js — modo oscuro, tiempo real, responsive
- 🔑 Autenticación JWT con hash de contraseñas bcrypt
- 🗃 SQLite integrado — sin base de datos externa requerida
- ⚙️ Configuración del servidor con un solo comando

## Registro npm

**Registro npm privado + proxy, integrado en el binario.** Sin contenedor Verdaccio aparte, sin base de datos adicional — Pier sirve una API compatible con npm en `/registry/npm/`, hace mirror transparente de `registry.npmjs.org` y funciona con todos los gestores de paquetes modernos.

### Clientes soportados

| Cliente | Versiones | Notas |
|---|---|---|
| **npm** | 7 – 11 | Funciona sin configuración extra |
| **yarn classic** | 1.22.x | Añadir `always-auth=true` al `.npmrc` |
| **yarn berry** | 2 · 3 · 4 | `.yarnrc.yml` con `npmAlwaysAuth: true` |
| **pnpm** | 9 · 10 | Funciona sin configuración extra |
| **bun** | latest | Funciona sin configuración extra |

### Comandos soportados

| Comando | Estado |
|---|---|
| `npm install` / `yarn add` / `pnpm add` / `bun add` | ✓ |
| `npm publish` (scoped + unscoped) | ✓ |
| `npm login` (flujo CouchDB + `--auth-type=web`) | ✓ |
| `npm dist-tag add / rm / ls` | ✓ |
| `npm deprecate` | ✓ |
| `npm unpublish` (versión única + paquete completo) | ✓ |
| `npm whoami` · `npm ping` | ✓ |

### Modo proxy upstream

Pier puede hacer mirror transparente de **npmjs.org** (o cualquier upstream compatible con npm) — todo el equipo usa una sola URL. Los packuments se cachean y revalidan con `If-None-Match`, los tarballs se descargan de forma perezosa en el primer `install`, y un GC LRU en segundo plano mantiene el caché en disco bajo un límite configurable. Gestiona todo en **Packages → Upstream proxy**.

- Una URL en el `.npmrc` para todo el equipo — sin scope routing
- Las instalaciones siguen funcionando aunque `npmjs.org` caiga
- Auditoría: qué paquetes públicos usa realmente el equipo
- Revalidación basada en TTL con cortocircuito 304

### Inicio rápido

```ini
# .npmrc en tu proyecto
registry=https://YOUR-PIER-HOST/registry/npm/
//YOUR-PIER-HOST/registry/npm/:_authToken=pier_npm_xxx
always-auth=true
```

Crea el token en **Packages → Manage tokens**, luego:

```bash
npm publish                  # paquete privado
npm install left-pad         # proxy desde npmjs.org + cacheado
```

Guías completas por cliente: [npm](https://pier.team/docs/registry/clients/npm) · [yarn 1.x](https://pier.team/docs/registry/clients/yarn-classic) · [yarn 2/3/4](https://pier.team/docs/registry/clients/yarn-berry) · [pnpm](https://pier.team/docs/registry/clients/pnpm) · [bun](https://pier.team/docs/registry/clients/bun).

## Auto-build (Railpack)

La fuente **Auto-build** te permite desplegar desde un repositorio Git **sin escribir un Dockerfile**. Por dentro Pier delega en [Railpack](https://github.com/railwayapp/railpack) (el constructor open-source de Railway, sucesor de Nixpacks), que se apoya en un daemon local [moby/buildkit](https://github.com/moby/buildkit). Ambos se aprovisionan automáticamente desde `install.sh`. Guía completa en [from-railpack](https://pier.team/docs/applications/from-railpack).

> ### ⚠ Requisitos del servidor — léelos antes de activarlo
>
> Auto-build es **considerablemente más pesado** que los otros métodos de despliegue. Compilar código de usuario en el host es fundamentalmente distinto a ejecutar un contenedor preconstruido, por lo que el perfil de recursos cambia:
>
> |              | Dockerfile / Compose / Docker Image | Auto-build (Railpack) |
> |---|---|---|
> | RAM mínima   | 512 MB                              | **4 GB** (8 GB para Rust) |
> | Disco libre  | unos pocos GB por stack             | **40+ GB** (caché de BuildKit) |
> | Primer deploy| segundos                            | 1–10 minutos |
>
> **Si tu VPS tiene menos de 4 GB de RAM, usa la fuente Dockerfile o Docker Image en su lugar.** La UI muestra una advertencia clara cuando el host tiene &lt;4 GB — el build casi con seguridad acabará en OOM-kill (de sí mismo o de otro proceso). Pier-core poda la caché de BuildKit diariamente a ~10 GB / retención de 7 días. También puedes ejecutar `PIER_SKIP_RAILPACK=1 bash install.sh` para saltarte la instalación por completo.

**Qué detecta Railpack** (sin configuración manual):

| Lenguaje / framework | Detectado a partir de |
|---|---|
| Node.js / Bun / Deno | `package.json`, `bun.lockb`, `deno.json` |
| Python | `requirements.txt`, `pyproject.toml`, `Pipfile` |
| Go | `go.mod` |
| Rust | `Cargo.toml` |
| PHP | `composer.json` |
| Java | `pom.xml`, `build.gradle` |
| Ruby | `Gemfile` |
| Elixir | `mix.exs` |
| Sitios estáticos Vite / Astro / CRA | configuración del bundler + carpeta de salida del build |

Si tu proyecto necesita anular el comportamiento por defecto, deja un [`railpack.json`](https://railpack.com/configuration/file) en la raíz del repo — Railpack lo recoge automáticamente.

**Ajustes finos** (en el unit de systemd o antes de `install.sh`):

- `PIER_RAILPACK_MAX_PARALLEL_BUILDS=N` — límite de builds en paralelo (1 por defecto). También se puede cambiar en la UI desde `Configuración → Auto-build (Railpack)`.
- `PIER_BUILDKIT_MEMORY=4g` — límite de RAM para el contenedor buildkit (4g por defecto).
- `PIER_SKIP_RAILPACK=1` — salta la instalación. La tarjeta permanece en la UI pero al intentar construir aparece el mensaje "railpack binary not found".

**FAQ**

- **¿Por qué no Nixpacks?** Railpack es el sucesor activo (Railway migró en marzo de 2025); Nixpacks está en modo mantenimiento. Railpack produce imágenes Node ~38% más pequeñas e imágenes Python ~77% más pequeñas gracias a su enfoque basado en grafos de BuildKit.
- **¿Funciona en ARM/aarch64?** Sí — tanto `railpack` como `moby/buildkit` distribuyen binarios linux/arm64. El install.sh elige la arquitectura correcta automáticamente.
- **¿Puedo desactivarlo?** Sí — `PIER_SKIP_RAILPACK=1 bash install.sh` salta la instalación. Las fuentes Dockerfile / Compose / Docker Image siguen funcionando sin cambios.

## Plantillas

**Bases de datos** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**Servicios** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**Juegos** — Minecraft, Terraria

**VPN** — AmneziaWG

**Aplicaciones** — Despliegue desde Dockerfile, imagen Docker o Docker Compose

> ¿No encuentras lo que necesitas? Despliega cualquier imagen Docker o stack de Compose manualmente.

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
- [x] Editor de datos integrado (PostgreSQL, MySQL/MariaDB, MongoDB, Redis)
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
