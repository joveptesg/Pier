# Pier PaaS — TODO: Фазы 8–11

## Обзор

| Phase | Название | Приоритет | Статус |
|-------|----------|-----------|--------|
| **8** | Reverse Proxy (Traefik) + Домены + SSL + Служебные домены | CRITICAL | TODO |
| **9** | Git Webhooks + Auto-Deploy Pipeline | HIGH | TODO |
| **10** | Multi-Server — завершение | HIGH | TODO |
| **11** | Auto-Update + Load Balancing + RBAC + Alerts | MEDIUM | TODO |

---

## Phase 8: Reverse Proxy + Домены + SSL

### 8.1 Traefik интеграция

- [ ] **8.1.1** Модуль `src/proxy/mod.rs` — lifecycle Traefik контейнера
  - Deploy `traefik:v3` через Bollard (порты 80, 443)
  - Создать Docker network `pier-net`
  - Stop/restart/status Traefik
  - Проверка что Traefik контейнер healthy
- [ ] **8.1.2** Модуль `src/proxy/config.rs` — генерация конфигов
  - Static config `{data_dir}/traefik/traefik.yml`:
    - entryPoints: web (80), websecure (443)
    - certificatesResolvers: letsencrypt (ACME HTTP-01)
    - providers.file.directory: `{data_dir}/traefik/dynamic/`
    - providers.file.watch: true
  - Dynamic config per domain: `{data_dir}/traefik/dynamic/{service_id}.yml`
    - Router: `Host(\`domain\`)` → service
    - TLS certResolver: letsencrypt
    - Service: loadBalancer → `http://host.docker.internal:{port}`
- [ ] **8.1.3** API `src/api/proxy.rs`
  - `POST /api/v1/proxy/enable` — запустить Traefik
  - `POST /api/v1/proxy/disable` — остановить Traefik
  - `GET /api/v1/proxy/status` — статус + информация о сертификатах
  - `PUT /api/v1/proxy/settings` — ACME email, dashboard toggle
- [ ] **8.1.4** Настройки прокси в таблице `settings`
  - `proxy.enabled` = true/false
  - `proxy.acme_email` = admin@example.com
  - `proxy.dashboard` = true/false
  - `proxy.wildcard_domain` = *.example.com (опционально)

### 8.2 Домены

- [ ] **8.2.1** DB миграция 7 — таблица `domains`
  ```sql
  CREATE TABLE domains (
      id TEXT PRIMARY KEY,
      domain TEXT NOT NULL UNIQUE,
      service_id TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
      ssl_status TEXT NOT NULL DEFAULT 'pending',
      ssl_expires_at TEXT,
      ssl_provider TEXT NOT NULL DEFAULT 'letsencrypt',
      is_generated INTEGER NOT NULL DEFAULT 0,
      created_at TEXT NOT NULL DEFAULT (datetime('now')),
      updated_at TEXT NOT NULL DEFAULT (datetime('now'))
  );
  ```
- [ ] **8.2.2** API `src/api/domains.rs`
  - `GET /api/v1/domains` — список всех доменов
  - `POST /api/v1/domains` — привязать домен к ресурсу
  - `DELETE /api/v1/domains/{id}` — удалить привязку
  - При создании/удалении — автоматически генерить/удалять Traefik dynamic config
- [ ] **8.2.3** Роуты в `src/api/mod.rs`

### 8.3 Служебные домены (как в Coolify)

Coolify генерирует `{uuid}.{server-ip}.sslip.io` — домен, который резолвится в IP сервера без настройки DNS.

- [ ] **8.3.1** Генерация служебного домена при деплое ресурса
  - Формат: `{service-name}-{short-id}.{server-ip}.sslip.io`
  - Пример: `my-postgres-a1b2c3.203.0.113.42.sslip.io`
  - Если настроен wildcard domain: `{service-name}-{short-id}.custom.domain.com`
  - Автоматически создаётся запись в `domains` с `is_generated = 1`
- [ ] **8.3.2** Определение IP сервера
  - Для localhost: определить публичный IP через `https://api.ipify.org` или `sysinfo`
  - Для remote server: использовать `servers.host`
  - Сохранить в `settings` как `server.public_ip`
- [ ] **8.3.3** Traefik route для служебного домена
  - Автоматически создавать dynamic config
  - SSL через Let's Encrypt (sslip.io поддерживает ACME HTTP-01)
- [ ] **8.3.4** UI: показывать служебный домен на странице ресурса
  - "Service URL: https://my-app-a1b2c3.203.0.113.42.sslip.io" с кнопкой копирования
  - Пометка "(auto-generated)" рядом со служебным доменом
  - Возможность добавить кастомный домен дополнительно

### 8.4 Фоновый SSL мониторинг

- [ ] **8.4.1** Background task (tokio::spawn, интервал 5 мин)
  - Читать Traefik ACME JSON (`{data_dir}/traefik/acme.json`)
  - Парсить сертификаты, извлечь expiry date
  - Обновить `ssl_status` и `ssl_expires_at` в таблице `domains`
  - `pending` → `active` когда сертификат получен
  - `active` → `expired` когда истёк

### 8.5 UI

- [ ] **8.5.1** Страница `assets/templates/settings/proxy.html`
  - Enable/Disable Traefik toggle
  - ACME Email input
  - Wildcard domain input (опционально)
  - Dashboard access toggle
  - Статус: Traefik running/stopped, кол-во сертификатов
- [ ] **8.5.2** Страница `assets/templates/domains/list.html`
  - Таблица всех доменов: domain, service, SSL status (🔒/⏳/❌), expires
  - Фильтр: все / auto-generated / custom
  - Добавить домен (модалка)
- [ ] **8.5.3** Секция доменов на `resources/detail.html`
  - Список привязанных доменов (служебный + кастомные)
  - SSL badge (зелёный/жёлтый/красный)
  - Кнопка "Add Custom Domain"
- [ ] **8.5.4** Сайдбар — добавить "Domains" пункт в `base.html`

### 8.6 Компиляция и тест

- [ ] **8.6.1** `cargo build --release`
- [ ] **8.6.2** `docker compose build && docker compose up -d`
- [ ] **8.6.3** Тест: Enable proxy → deploy PostgreSQL → служебный домен появился → открывается в браузере → SSL active
- [ ] **8.6.4** Тест: Добавить кастомный домен → Traefik подхватил → SSL получен

---

## Phase 9: Git Webhooks + Auto-Deploy

### 9.1 Зависимости

- [ ] **9.1.1** Добавить в workspace Cargo.toml:
  ```toml
  hmac = "0.12"
  sha2 = "0.10"
  hex = "0.4"
  ```

### 9.2 DB миграция 8

- [ ] **9.2.1** Расширить таблицу `services`:
  ```sql
  ALTER TABLE services ADD COLUMN git_source_id TEXT;
  ALTER TABLE services ADD COLUMN git_repo_url TEXT;
  ALTER TABLE services ADD COLUMN git_branch TEXT DEFAULT 'main';
  ALTER TABLE services ADD COLUMN git_webhook_secret TEXT;
  ALTER TABLE services ADD COLUMN build_strategy TEXT DEFAULT 'dockerfile';
  ALTER TABLE services ADD COLUMN previous_image_tag TEXT;
  ```
- [ ] **9.2.2** Таблица `deployments`:
  ```sql
  CREATE TABLE deployments (
      id TEXT PRIMARY KEY,
      service_id TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
      commit_sha TEXT,
      commit_message TEXT,
      branch TEXT,
      status TEXT NOT NULL DEFAULT 'pending',
      build_log TEXT NOT NULL DEFAULT '',
      image_tag TEXT,
      triggered_by TEXT NOT NULL DEFAULT 'webhook',
      duration_secs INTEGER,
      started_at TEXT NOT NULL DEFAULT (datetime('now')),
      finished_at TEXT
  );
  ```

### 9.3 Webhook приёмники

- [ ] **9.3.1** `src/api/webhooks.rs` — GitHub webhook
  - `POST /api/v1/webhooks/github`
  - Верификация `X-Hub-Signature-256` (HMAC-SHA256)
  - Парсинг push event → извлечь repo URL, branch, commit SHA
  - Поиск service по `git_repo_url` + `git_branch`
  - Запуск deploy pipeline
- [ ] **9.3.2** `src/api/webhooks.rs` — GitLab webhook
  - `POST /api/v1/webhooks/gitlab`
  - Верификация через `X-Gitlab-Token` header
  - Аналогичная логика

### 9.4 Deploy pipeline

- [ ] **9.4.1** `src/deploy/mod.rs` — оркестратор
  - `async fn run_pipeline(service_id, commit_info, state) -> Result<()>`
  - Создание записи `deployments` (status: pending)
  - tokio::broadcast channel для стрима логов
  - Обновление статуса на каждом шаге
  - Запись duration при завершении
- [ ] **9.4.2** `src/deploy/build.rs` — стратегии билда
  - **Dockerfile**: `docker build -t {name}:{sha_short} .` через Bollard build API
  - **docker-compose**: обновить image tag в compose, `docker compose up -d`
  - Стрим build output в `tokio::broadcast`
  - Сохранение лога в `deployments.build_log`
- [ ] **9.4.3** `src/deploy/rollback.rs` — откат
  - Восстановить `previous_image_tag`
  - Обновить compose, `docker compose up -d`
  - Создать deployment запись с `triggered_by = 'rollback'`

### 9.5 API

- [ ] **9.5.1** Git config endpoints в `src/api/resources.rs`:
  - `PUT /api/v1/resources/{id}/git` — настроить repo, branch, build strategy, webhook secret
  - `GET /api/v1/resources/{id}/git` — получить git config
- [ ] **9.5.2** Deploy/Rollback endpoints:
  - `POST /api/v1/resources/{id}/deploy` — ручной редеплой
  - `POST /api/v1/resources/{id}/rollback` — откат
- [ ] **9.5.3** `src/api/deployments.rs` — история деплоев:
  - `GET /api/v1/resources/{id}/deployments` — список
  - `GET /api/v1/resources/{id}/deployments/{id}` — детали + лог
  - `GET /api/v1/resources/{id}/deployments/{id}/logs` — SSE стрим

### 9.6 UI

- [ ] **9.6.1** Git config секция на `resources/detail.html`
  - Repo URL, branch, build strategy (dropdown)
  - Webhook URL (auto-generated, с кнопкой копирования)
  - "Deploy Now" кнопка
- [ ] **9.6.2** Deployments вкладка на `resources/detail.html`
  - Таблица: #, commit SHA, branch, status, triggered_by, duration, time
  - Клик → полный build log (терминальный стиль)
  - "Rollback" кнопка на последнем успешном деплое
- [ ] **9.6.3** Live build log viewer
  - SSE подключение при активном билде
  - Auto-scroll, monospace шрифт, зелёный/красный по статусу

### 9.7 Компиляция и тест

- [ ] **9.7.1** `cargo build --release`
- [ ] **9.7.2** Тест: настроить git source → push в репо → webhook → auto-deploy → success
- [ ] **9.7.3** Тест: ручной deploy → build log в реальном времени
- [ ] **9.7.4** Тест: rollback → предыдущая версия восстановлена

---

## Phase 10: Multi-Server — завершение

### 10.1 DB миграция 9

- [ ] **10.1.1** Расширить таблицы:
  ```sql
  ALTER TABLE deployments ADD COLUMN server_id TEXT REFERENCES servers(id);
  ALTER TABLE servers ADD COLUMN labels_json TEXT DEFAULT '{}';
  ALTER TABLE servers ADD COLUMN max_containers INTEGER DEFAULT 100;
  ```

### 10.2 Рефакторинг deploy_stack

- [ ] **10.2.1** Создать `ServerInfo` struct в `src/db/models.rs`
  ```rust
  pub struct ServerInfo {
      pub id: String,
      pub host: String,
      pub port: i64,
      pub agent_token: String,
      pub is_local: bool,
  }
  ```
- [ ] **10.2.2** Рефакторинг `docker::compose::deploy_stack`
  - Добавить параметр `target_server: Option<&ServerInfo>`
  - Local: существующая логика
  - Remote: `reqwest::Client::post` → `http://{host}:{port}/api/v1/agent/deploy`
  - Передать compose YAML + stack_name в body
- [ ] **10.2.3** Обновить все call sites в `src/api/resources.rs`
  - Резолвить `server_id` → `ServerInfo`
  - Передавать в `deploy_stack`

### 10.3 Agent proxy

- [ ] **10.3.1** `src/api/agent_proxy.rs`
  - `GET /api/v1/servers/{id}/containers` → проксирует к агенту `/api/v1/agent/status`
  - `GET /api/v1/servers/{id}/stacks` → проксирует к агенту
  - `POST /api/v1/servers/{id}/deploy` → проксирует к агенту `/api/v1/agent/deploy`

### 10.4 Agent install script

- [ ] **10.4.1** `GET /api/v1/servers/install-script`
  - Генерирует bash скрипт с embedded token и core URL
  - Скрипт: install Docker → download pier-agent → systemd service → register
  - Формат: `curl -fsSL https://pier-host:8443/api/v1/servers/install-script?token=XXX | sh`

### 10.5 UI

- [ ] **10.5.1** Server detail page `assets/templates/servers/detail.html`
  - Real-time метрики (CPU, RAM, Disk — прогресс-бары)
  - Список контейнеров на этом сервере
  - Agent version, uptime, last heartbeat
- [ ] **10.5.2** Agent install page `assets/templates/servers/install.html`
  - Команда `curl | sh` с pre-filled token
  - Инструкция по шагам
- [ ] **10.5.3** Server selector на `resources/create.html`
  - Dropdown "Target Server" (default: localhost)
  - Показывать load (CPU%, RAM%) для каждого сервера
- [ ] **10.5.4** Dashboard — карточки серверов
  - Mini-card для каждого сервера: name, status, CPU, RAM
  - Клик → server detail page

### 10.6 Компиляция и тест

- [ ] **10.6.1** `cargo build --release`
- [ ] **10.6.2** Тест: деплой ресурса с `server_id` = remote → compose отправлен агенту → контейнер запущен
- [ ] **10.6.3** Тест: server detail → метрики обновляются в реальном времени

---

## Phase 11: Auto-Update + Load Balancing + RBAC + Alerts

### 11.1 Auto-Update платформы

- [ ] **11.1.1** `src/update/mod.rs` — проверка обновлений
  - Фоновая задача (1 раз в 24ч, настраиваемо через cron)
  - `GET https://api.github.com/repos/joveptesg/Pier/releases/latest`
  - Сравнить tag_name с текущей версией (embedded в бинарник через `env!("CARGO_PKG_VERSION")`)
  - Если есть новая версия → записать в `settings` (update.available, update.version, update.url)
- [ ] **11.1.2** `src/update/apply.rs` — применение обновления
  - Поскольку Pier в Docker: скачать новый image tag → `docker compose pull && docker compose up -d`
  - Или: скачать новый бинарник → заменить → restart контейнера
  - 3 режима: auto (применять сразу), notify (показать badge), manual (только по клику)
- [ ] **11.1.3** API endpoints:
  - `GET /api/v1/system/update` — текущая версия + доступное обновление
  - `POST /api/v1/system/update` — применить обновление
  - `PUT /api/v1/system/update/settings` — режим обновления (auto/notify/manual), cron
- [ ] **11.1.4** UI: badge "Update available" в header/sidebar
  - Страница Settings → Updates: текущая версия, доступная, режим, changelog
  - Кнопка "Update Now"

### 11.2 Load Balancing (через Traefik)

Traefik уже IS load balancer. Нужно дать пользователю контроль.

- [ ] **11.2.1** DB: расширить `services` для multi-instance
  ```sql
  ALTER TABLE services ADD COLUMN replicas INTEGER DEFAULT 1;
  ALTER TABLE services ADD COLUMN lb_strategy TEXT DEFAULT 'round-robin';
  ALTER TABLE services ADD COLUMN lb_sticky INTEGER DEFAULT 0;
  ```
- [ ] **11.2.2** Scale up/down ресурса
  - `POST /api/v1/resources/{id}/scale` — `{ replicas: 3 }`
  - Pier генерирует compose с `deploy.replicas: N`
  - Traefik dynamic config: несколько серверов в loadBalancer
- [ ] **11.2.3** Стратегии балансировки
  - **Round Robin** (default) — Traefik WRR
  - **Weighted** — вес на каждый инстанс (для разных серверов)
  - **Sticky Sessions** — cookie-based affinity
- [ ] **11.2.4** Multi-server load balancing
  - Один ресурс на нескольких серверах
  - Traefik dynamic config с несколькими URL:
    ```yaml
    services:
      my-app:
        loadBalancer:
          servers:
            - url: "http://server1:3000"
            - url: "http://server2:3000"
          sticky:
            cookie: {}
    ```
- [ ] **11.2.5** UI: Scale section на resource detail
  - Replicas slider (1–10)
  - LB strategy dropdown
  - Sticky sessions toggle
  - Visual: показать на каких серверах инстансы

### 11.3 RBAC (Role-Based Access Control)

- [ ] **11.3.1** DB миграция 10 — таблицы:
  ```sql
  CREATE TABLE permissions (
      id TEXT PRIMARY KEY, role TEXT, resource TEXT, action TEXT,
      UNIQUE(role, resource, action)
  );
  -- Seed: admin=*, manager=projects+domains+resources RW, viewer=read-only

  CREATE TABLE api_tokens (
      id TEXT PRIMARY KEY, user_id TEXT, name TEXT, token_hash TEXT,
      prefix TEXT, permissions TEXT DEFAULT '*',
      last_used_at TEXT, expires_at TEXT, created_at TEXT
  );
  ```
- [ ] **11.3.2** `src/auth/rbac.rs` — middleware проверки прав
  - `check_permission(role, resource, action) -> bool`
  - Axum middleware layer для protected routes
- [ ] **11.3.3** `src/auth/api_token.rs` — Bearer token auth
  - `Authorization: Bearer pier_abc123...`
  - Lookup token_hash, verify, check permissions, update last_used_at
  - Работает параллельно с session auth
- [ ] **11.3.4** `src/api/users.rs` — управление пользователями (admin only)
  - `GET /api/v1/users` — список
  - `POST /api/v1/users` — создать с ролью
  - `PUT /api/v1/users/{id}` — изменить роль
  - `DELETE /api/v1/users/{id}` — деактивировать
- [ ] **11.3.5** `src/api/tokens.rs` — API токены
  - `GET /api/v1/tokens` — список для текущего пользователя
  - `POST /api/v1/tokens` — создать (вернуть токен один раз!)
  - `DELETE /api/v1/tokens/{id}` — отозвать
- [ ] **11.3.6** UI:
  - `settings/users.html` — таблица пользователей, invite, роли
  - `settings/tokens.html` — API токены, create/revoke
  - Скрывать кнопки deploy/delete/settings для viewer роли

### 11.4 Activity Log

- [ ] **11.4.1** DB таблица `activity_log`
  ```sql
  CREATE TABLE activity_log (
      id TEXT PRIMARY KEY, user_id TEXT, action TEXT,
      target_type TEXT, target_id TEXT, details TEXT,
      ip_address TEXT, created_at TEXT
  );
  ```
- [ ] **11.4.2** Вставить logging calls во все хендлеры:
  - deploy, stop, restart, delete resource
  - create/delete domain
  - create/delete server
  - user login/logout
  - settings changes
- [ ] **11.4.3** `GET /api/v1/activity` — filterable log
- [ ] **11.4.4** UI: `activity/list.html` — лента событий с фильтрами

### 11.5 Alerts + Notifications

- [ ] **11.5.1** DB таблица `alert_rules`
  ```sql
  CREATE TABLE alert_rules (
      id TEXT PRIMARY KEY, name TEXT, metric TEXT,
      threshold REAL, comparison TEXT DEFAULT 'gt',
      server_id TEXT, webhook_url TEXT,
      webhook_type TEXT DEFAULT 'generic',
      cooldown_mins INTEGER DEFAULT 30,
      is_active INTEGER DEFAULT 1, last_triggered TEXT,
      created_at TEXT
  );
  ```
- [ ] **11.5.2** `src/alerts/mod.rs` — фоновая задача (интервал 30с)
  - Проверить метрики каждого сервера
  - CPU > threshold, RAM > threshold, Disk > threshold
  - Container restart count > N за M минут
  - Учитывать cooldown (не слать повторно)
- [ ] **11.5.3** `src/alerts/webhook.rs` — форматирование и отправка
  - **Telegram**: `POST https://api.telegram.org/bot{token}/sendMessage`
  - **Discord**: `POST webhook_url` с embed JSON
  - **Slack**: `POST webhook_url` с blocks JSON
  - **Generic**: `POST webhook_url` с JSON payload
- [ ] **11.5.4** API:
  - `GET /api/v1/alerts` — список правил
  - `POST /api/v1/alerts` — создать
  - `PUT /api/v1/alerts/{id}` — изменить
  - `DELETE /api/v1/alerts/{id}` — удалить
  - `POST /api/v1/alerts/{id}/test` — отправить тестовое уведомление
- [ ] **11.5.5** UI: `settings/alerts.html`
  - Список правил с toggle on/off
  - Создание: metric dropdown, threshold, webhook URL, type
  - Кнопка "Test" для проверки webhook

### 11.6 Компиляция и тест

- [ ] **11.6.1** `cargo build --release`
- [ ] **11.6.2** `docker compose build && docker compose up -d`
- [ ] **11.6.3** Тест auto-update: подменить версию → проверить badge → update
- [ ] **11.6.4** Тест LB: scale ресурса до 3 реплик → трафик распределяется
- [ ] **11.6.5** Тест RBAC: viewer не может deploy/delete
- [ ] **11.6.6** Тест alerts: CPU > 1% → webhook → Telegram получил

---

## Сводка файлов по фазам

### Phase 8 (Proxy + Домены + SSL)

| Действие | Файл |
|----------|------|
| Создать | `src/proxy/mod.rs` |
| Создать | `src/proxy/config.rs` |
| Создать | `src/api/domains.rs` |
| Создать | `src/api/proxy.rs` |
| Создать | `assets/templates/settings/proxy.html` |
| Создать | `assets/templates/domains/list.html` |
| Изменить | `src/main.rs` — `mod proxy;` |
| Изменить | `src/api/mod.rs` — роуты |
| Изменить | `src/db/schema.rs` — миграция 7 |
| Изменить | `assets/templates/base.html` — сайдбар |
| Изменить | `assets/templates/resources/detail.html` — домены |
| Изменить | `src/api/resources.rs` — авто-генерация служебного домена при deploy |

### Phase 9 (Git Webhooks + Auto-Deploy)

| Действие | Файл |
|----------|------|
| Создать | `src/deploy/mod.rs` |
| Создать | `src/deploy/build.rs` |
| Создать | `src/deploy/rollback.rs` |
| Создать | `src/api/webhooks.rs` |
| Создать | `src/api/deployments.rs` |
| Изменить | `Cargo.toml` (workspace + pier-core) — hmac, sha2, hex |
| Изменить | `src/main.rs` — `mod deploy;` |
| Изменить | `src/api/mod.rs` — роуты (webhooks публичные!) |
| Изменить | `src/db/schema.rs` — миграция 8 |
| Изменить | `assets/templates/resources/detail.html` — git config + deployments |

### Phase 10 (Multi-Server)

| Действие | Файл |
|----------|------|
| Создать | `src/api/agent_proxy.rs` |
| Создать | `assets/templates/servers/detail.html` |
| Создать | `assets/templates/servers/install.html` |
| Изменить | `src/docker/compose.rs` — deploy_stack refactor |
| Изменить | `src/api/resources.rs` — server routing |
| Изменить | `src/api/mod.rs` — роуты |
| Изменить | `src/db/schema.rs` — миграция 9 |
| Изменить | `assets/templates/resources/create.html` — server selector |
| Изменить | `assets/templates/dashboard.html` — server cards |

### Phase 11 (Auto-Update + LB + RBAC + Alerts)

| Действие | Файл |
|----------|------|
| Создать | `src/update/mod.rs` |
| Создать | `src/update/apply.rs` |
| Создать | `src/auth/rbac.rs` |
| Создать | `src/auth/api_token.rs` |
| Создать | `src/api/users.rs` |
| Создать | `src/api/tokens.rs` |
| Создать | `src/api/activity.rs` |
| Создать | `src/api/alerts.rs` |
| Создать | `src/alerts/mod.rs` |
| Создать | `src/alerts/webhook.rs` |
| Создать | `assets/templates/settings/users.html` |
| Создать | `assets/templates/settings/tokens.html` |
| Создать | `assets/templates/settings/alerts.html` |
| Создать | `assets/templates/settings/updates.html` |
| Создать | `assets/templates/activity/list.html` |
| Изменить | `src/auth/middleware.rs` — RBAC + API token |
| Изменить | `src/api/mod.rs` — роуты |
| Изменить | `src/db/schema.rs` — миграция 10 |
| Изменить | `assets/templates/base.html` — activity + alerts sidebar |
| Изменить | Все существующие хендлеры — activity logging |

---

## Зависимости

```
Phase 8 (Proxy)     ←── CRITICAL, без этого нельзя выставить приложение в интернет
    ↕ (независимы)
Phase 9 (Webhooks)  ←── HIGH, без этого нет auto-deploy

Phase 10 (Servers)  ←── зависит от Phase 9 (deploy pipeline)

Phase 11 (Polish)   ←── независима, можно делать параллельно
```

## Статистика

| Метрика | Значение |
|---------|----------|
| Новых Rust модулей | ~20 |
| Новых HTML шаблонов | ~10 |
| Новых DB миграций | 4 (7–10) |
| Новых API эндпоинтов | ~35 |
| Новых зависимостей | hmac, sha2, hex |
