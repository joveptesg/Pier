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

## Быстрый старт

### Вариант A: Установка одной командой (Ubuntu/Debian)

```bash
curl -fsSL https://pier.team/install | sudo bash
```

> Короткий URL перенаправляет на [`scripts/bootstrap.sh`](../../scripts/bootstrap.sh). Скрипт устанавливает Docker, скачивает последний релизный бинарник (с проверкой sha256) и запускает `install.sh`. Запускайте его повторно в любой момент, чтобы обновиться до последнего релиза.

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

### Вариант D: Установка из готового релиза (без сборки)

Docker уже установлен? Скачайте готовый бинарник напрямую — без тулчейна Rust и без компиляции:

```bash
# 1. Скачиваем готовый бинарник + контрольную сумму (linux/amd64)
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64 -o pier-linux-amd64
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64.sha256 -o pier-linux-amd64.sha256
sha256sum -c pier-linux-amd64.sha256          # проверяем целостность

# 2. Получаем установщик и запускаем его
curl -fL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/install.sh -o install.sh
chmod +x pier-linux-amd64
sudo bash install.sh --binary ./pier-linux-amd64
```

> Ручной эквивалент Варианта A, но без автоматической установки Docker. Требуется уже установленные Docker + Compose (см. [INSTALL.md](../../INSTALL.md)). Имя файла бинарника должно оставаться `pier-linux-amd64`, чтобы `sha256sum -c` совпал.

### Обновление Pier

Обновления подтягивают свежий **готовый бинарник** — пересборка из исходников не нужна. `install.sh` определяет запущенный сервис, останавливает его, заменяет бинарник и перезапускает его, сохраняя ваш `.env` и `/opt/pier/data`.

```bash
# Проще всего — повторно запустить установку одной командой (заново скачает последний релиз):
curl -fsSL https://pier.team/install | sudo bash

# Или вручную, тот же процесс, что и в Варианте D (скачать → проверить → install.sh).
```

Затем откройте `http://YOUR_SERVER_IP:8443/setup` для создания учётной записи администратора.

> Подробная настройка сервера (усиление безопасности, файрвол, установка Docker) описана в [INSTALL.md](../../INSTALL.md).

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
- ✨ **Авто-сборка (Railpack)** — сборка из исходников без Dockerfile для Node, Python, Go, PHP, Java, Ruby, Rust, Vite/Astro/CRA и других
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

## npm-реестр

**Приватный + proxy-реестр npm встроен прямо в бинарник.** Без отдельного Verdaccio-контейнера, без отдельной БД — Pier отдаёт npm-совместимый API на `/registry/npm/`, прозрачно зеркалит `registry.npmjs.org` и работает со всеми современными пакетными менеджерами.

### Поддерживаемые клиенты

| Клиент | Версии | Примечания |
|---|---|---|
| **npm** | 7 – 11 | Работает «из коробки» |
| **yarn classic** | 1.22.x | Добавить `always-auth=true` в `.npmrc` |
| **yarn berry** | 2 · 3 · 4 | `.yarnrc.yml` с `npmAlwaysAuth: true` |
| **pnpm** | 9 · 10 | Работает «из коробки» |
| **bun** | latest | Работает «из коробки» |

### Поддерживаемые команды

| Команда | Статус |
|---|---|
| `npm install` / `yarn add` / `pnpm add` / `bun add` | ✓ |
| `npm publish` (scoped + unscoped) | ✓ |
| `npm login` (CouchDB-flow + `--auth-type=web`) | ✓ |
| `npm dist-tag add / rm / ls` | ✓ |
| `npm deprecate` | ✓ |
| `npm unpublish` (одна версия + весь пакет) | ✓ |
| `npm whoami` · `npm ping` | ✓ |

### Режим upstream-proxy

Pier может прозрачно зеркалить **npmjs.org** (или любой совместимый upstream) — вся команда использует один URL. Packument-метаданные кешируются и ревалидируются через `If-None-Match`, tarball'ы тянутся лениво при первом `install`, фоновый LRU GC удерживает кеш в пределах настраиваемого размера. Управление — в **Packages → Upstream proxy**.

- Один URL в `.npmrc` на всю команду — без scope-routing
- Install работает даже когда `npmjs.org` лежит
- Аудит: видно какие публичные пакеты реально используются
- TTL-ревалидация с короткозамыканием 304

### Быстрый старт

```ini
# .npmrc в проекте
registry=https://YOUR-PIER-HOST/registry/npm/
//YOUR-PIER-HOST/registry/npm/:_authToken=pier_npm_xxx
always-auth=true
```

Создать токен — **Packages → Manage tokens**, далее:

```bash
npm publish                  # приватный пакет
npm install left-pad         # проксируется с npmjs.org + кешируется
```

Подробные гайды по клиентам: [npm](https://pier.team/docs/registry/clients/npm) · [yarn 1.x](https://pier.team/docs/registry/clients/yarn-classic) · [yarn 2/3/4](https://pier.team/docs/registry/clients/yarn-berry) · [pnpm](https://pier.team/docs/registry/clients/pnpm) · [bun](https://pier.team/docs/registry/clients/bun).

## Авто-сборка (Railpack)

Источник **Авто-сборка** позволяет разворачивать приложение из Git-репозитория **без написания Dockerfile**. Pier вызывает [Railpack](https://github.com/railwayapp/railpack) (открытый сборщик от Railway, преемник Nixpacks), который работает с локальным [moby/buildkit](https://github.com/moby/buildkit). Оба компонента устанавливаются автоматически через `install.sh`. Полное руководство — в [from-railpack](https://pier.team/docs/applications/from-railpack).

> ### ⚠ Требования к серверу — прочитайте перед включением
>
> Авто-сборка **существенно тяжелее** остальных способов деплоя. Компиляция кода пользователя на хосте принципиально отличается от запуска готового контейнера, поэтому профиль нагрузки меняется:
>
> |              | Dockerfile / Compose / Docker Image | Авто-сборка (Railpack) |
> |---|---|---|
> | Минимум RAM   | 512 МБ                              | **4 ГБ** (8 ГБ для Rust) |
> | Свободный диск| несколько ГБ на стек                | **40+ ГБ** (кэш BuildKit) |
> | Первый деплой | секунды                             | 1–10 минут |
>
> **Если на вашем VPS меньше 4 ГБ RAM — используйте источник Dockerfile или Docker Image.** UI выводит жёсткое предупреждение, когда у хоста &lt;4 ГБ — сборка почти наверняка получит OOM-kill для себя или другого процесса. Pier-core ежесуточно чистит кэш BuildKit до ~10 ГБ / 7 дней хранения. Можно вообще отключить установку Railpack: `PIER_SKIP_RAILPACK=1 bash install.sh`.

**Какие языки Railpack распознаёт автоматически** (без ручной настройки):

| Язык / фреймворк | Детектится по |
|---|---|
| Node.js / Bun / Deno | `package.json`, `bun.lockb`, `deno.json` |
| Python | `requirements.txt`, `pyproject.toml`, `Pipfile` |
| Go | `go.mod` |
| Rust | `Cargo.toml` |
| PHP | `composer.json` |
| Java | `pom.xml`, `build.gradle` |
| Ruby | `Gemfile` |
| Elixir | `mix.exs` |
| Vite / Astro / CRA (статика) | конфигурация бандлера + директория сборки |

Если нужны переопределения — положите в корень репозитория [`railpack.json`](https://railpack.com/configuration/file), Railpack подхватит его автоматически.

**Параметры тюнинга** (задаются в systemd unit или перед `install.sh`):

- `PIER_RAILPACK_MAX_PARALLEL_BUILDS=N` — лимит параллельных сборок (по умолчанию 1). Также настраивается в UI: `Настройки → Auto-build (Railpack)`.
- `PIER_BUILDKIT_MEMORY=4g` — лимит RAM для контейнера buildkit (по умолчанию 4g).
- `PIER_SKIP_RAILPACK=1` — пропустить установку. Карточка в UI останется, но при попытке сборки появится сообщение «railpack binary not found».

**FAQ**

- **Почему не Nixpacks?** Railpack — это активный преемник (Railway перешёл на него в марте 2025), Nixpacks находится в режиме maintenance. Railpack даёт ~38% меньшие Node-образы и ~77% меньшие Python-образы благодаря подходу через BuildKit-граф.
- **Работает ли на ARM/aarch64?** Да — и `railpack`, и `moby/buildkit` имеют сборки под linux/arm64. install.sh выбирает нужную архитектуру автоматически.
- **Можно отключить?** Да — `PIER_SKIP_RAILPACK=1 bash install.sh` пропустит установку. Источники Dockerfile / Compose / Docker Image будут работать как обычно.

## Шаблоны

**Базы данных** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**Сервисы** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**Игры** — Minecraft, Terraria

**VPN** — AmneziaWG

**Приложения** — развёртывание из Dockerfile, Docker-образа или Docker Compose

> Не нашли нужное? Разверните любой Docker-образ или Compose-стек вручную.

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
