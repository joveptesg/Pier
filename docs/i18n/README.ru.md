> Перевод [README.md](../../README.md). При расхождениях ориентируйтесь на английскую версию.

<p align="center">
  <img src="../logo.svg" alt="Pier" height="120">
</p>

<h3 align="center">Легковесная самостоятельно размещаемая PaaS.<br>Один бинарник. 20 МБ RAM. Разворачивайте что угодно.</h3>

<p align="center">
  <a href="https://github.com/joveptesg/pier/blob/main/LICENSE"><img src="https://img.shields.io/github/license/joveptesg/pier?color=blue" alt="Лицензия"></a>
  <a href="https://github.com/joveptesg/pier/stargazers"><img src="https://img.shields.io/github/stars/joveptesg/pier?style=flat" alt="Звёзды"></a>
  <a href="https://github.com/joveptesg/pier/releases"><img src="https://img.shields.io/github/v/release/joveptesg/pier" alt="Релиз"></a>
  <img src="https://img.shields.io/badge/rust-1.93%2B-orange" alt="Rust">
</p>

<p align="center">
  <a href="../../README.md">English</a> |
  <strong>Русский</strong> |
  <a href="README.zh-CN.md">中文</a> |
  <a href="README.de.md">Deutsch</a> |
  <a href="README.ja.md">日本語</a> |
  <a href="README.es.md">Español</a> |
  <a href="README.fr.md">Français</a> |
  <a href="README.pt-BR.md">Português</a>
</p>

---

## Что такое Pier?

**Pier — это open-source и self-hosted альтернатива Coolify / Heroku / Vercel, достаточно лёгкая для VPS за $5.**

Разворачивайте контейнеры, Docker Compose стеки и Git-репозитории с автоматическим SSL, обратным прокси и современным веб-дашбордом — всё из одного Rust-бинарника, потребляя всего **20–40 МБ RAM**.

<!-- 
<p align="center">
  <img src="../../docs/screenshots/dashboard.png" alt="Панель управления Pier" width="800">
</p>
-->

## Почему Pier?

[Coolify](https://coolify.io) — отличный инструмент, но он запускает **6+ контейнеров** и потребляет **750 МБ – 1,2 ГБ RAM** в простое. Pier предоставляет те же ключевые возможности в одном бинарнике.

| | Coolify | Pier |
|---|---|---|
| **RAM в простое** | 750 MB – 1.2 GB | 20–40 MB (+Traefik) |
| **Диск** | ~1 GB | ~15–30 MB |
| **Запущенные контейнеры** | 6+ (Laravel, PostgreSQL, Redis, Soketi, Horizon, Traefik) | 1 binary + Traefik |
| **Минимальный VPS** | 2 GB RAM, 2 vCPU | 512 MB RAM, 1 vCPU |
| **База данных** | External PostgreSQL | Embedded SQLite |
| **Язык** | PHP / Laravel | Rust |
| **JS фронтенда** | ~300 KB+ | ~30 KB (HTMX + Alpine.js) |

## Возможности

**Контейнеры и стеки**
- 📦 Управление Docker-контейнерами — создание, запуск, остановка, перезапуск, удаление, логи, статистика
- 🐳 Стеки Docker Compose со встроенным YAML-редактором
- 🚀 Развёртывание в один клик из **30+ шаблонов**

**Git и развёртывание**
- 🔄 Конвейер развёртывания из Git с вебхуками GitHub и GitLab
- 🛠 Сборка из Dockerfile, Docker-образа или Compose
- ⏪ История развёртываний с откатом

**Сеть и SSL**
- 🌐 Обратный прокси через Traefik с автоматическим HTTPS
- 🔒 SSL-сертификаты Let's Encrypt (автоматическое получение)
- 🔗 Пользовательские домены с автоматически генерируемыми URL сервисов

**Инфраструктура**
- 🖥 Управление несколькими серверами через удалённых агентов
- 💾 Планируемые резервные копии с интеграцией S3
- 📊 Мониторинг в реальном времени — CPU, RAM, диск, сеть

**Удобство для разработчиков**
- ⚡ Веб-интерфейс на HTMX + Alpine.js — тёмная тема, реальное время, адаптивность
- 🔑 JWT-аутентификация с хешированием паролей bcrypt
- 🗃 Встроенная SQLite — внешняя база данных не требуется
- ⚙️ Настройка сервера одной командой

## Шаблоны

**Базы данных** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**Сервисы** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**Игры** — Minecraft, Terraria

**VPN** — AmneziaWG

**Приложения** — развёртывание из Dockerfile, Docker-образа или Docker Compose

> Не нашли нужное? Разверните любой Docker-образ или Compose-стек вручную.

## Быстрый старт

### Вариант A: Установка одной командой (Ubuntu/Debian)

```bash
curl -fsSL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/setup.sh | sudo bash
```

### Вариант B: Сборка из исходного кода

```bash
git clone https://github.com/joveptesg/pier.git
cd pier
cargo build --release
sudo bash scripts/install.sh --binary target/release/pier
```

### Вариант C: Docker

```bash
docker run -d \
  --name pier \
  -p 8443:8443 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v pier-data:/app/data \
  ghcr.io/joveptesg/pier:latest
```

Затем откройте `http://YOUR_SERVER_IP:8443/setup` для создания учётной записи администратора.

> Подробная настройка сервера (усиление безопасности, файрвол, установка Docker) описана в [INSTALL.md](../../INSTALL.md).

## Технологический стек

| Уровень | Технология | Назначение |
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

## Архитектура

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

> Подробная архитектура описана в [ARCHITECTURE.md](../../ARCHITECTURE.md).

## Дорожная карта

- [x] Управление контейнерами (Docker API)
- [x] Стеки Docker Compose с YAML-редактором
- [x] Шаблоны сервисов в один клик (30+)
- [x] Обратный прокси + авто-SSL (Traefik + Let's Encrypt)
- [x] Git-вебхуки + автоматическое развёртывание (GitHub, GitLab)
- [x] Управление несколькими серверами через агентов
- [x] Планировщик резервного копирования с поддержкой S3
- [x] Веб-панель (HTMX + Tailwind, тёмная тема)
- [x] Управление S3-бакетами
- [x] Визуализация архитектуры (Canvas)
- [ ] RBAC (управление доступом на основе ролей)
- [ ] 2FA (TOTP + WebAuthn)
- [ ] Балансировка нагрузки + горизонтальное масштабирование
- [ ] Уведомления об оповещениях (Telegram, Discord, Slack)
- [ ] Механизм автообновления
- [ ] Изоляция Docker-сетей по проектам
- [ ] Обратный прокси на базе Pingora (замена Traefik)

## Участие в проекте

Мы приветствуем вклад в проект! Пожалуйста, прочитайте [CONTRIBUTING.md](../../CONTRIBUTING.md) перед отправкой pull request. Все участники должны принять наше [CLA](../../CLA.md).

```bash
cargo fmt          # Format code
cargo clippy       # Lint
cargo test         # Run tests
cargo build        # Build
```

## Лицензия

[AGPL-3.0](../../LICENSE)

Pier можно бесплатно размещать на своём сервере и модифицировать. Если вы предоставляете модифицированную версию как сетевой сервис, вы обязаны опубликовать ваши изменения под той же лицензией.

По вопросам коммерческого лицензирования (использование без обязательств AGPL) обращайтесь по адресу [info@devcom.app](mailto:info@devcom.app).

---

<p align="center">
  <sub>Создано на 🦀 Rust — быстро, безопасно, легковесно.</sub>
</p>
