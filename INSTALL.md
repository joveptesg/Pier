# Pier — Установка на чистый Ubuntu-сервер

## Быстрая установка (one-liner)

Если нужен Pier «здесь и сейчас», без ручной сборки из исходников:

```bash
curl -fsSL https://pier.team/install | sudo bash
```

Скрипт ставит Docker, скачивает готовый бинарь из [GitHub Releases](https://github.com/joveptesg/pier/releases/tag/latest) (с проверкой sha256) и запускает [`install.sh`](scripts/install.sh). Подходит для свежей Ubuntu/Debian. Дальнейшие шаги (создание admin-аккаунта на `http://SERVER_IP:8443/setup`) — см. §8.

> Альтернативные варианты установки (Docker, ручная сборка) — см. [README.md](README.md#quick-start).

Если нужен полный контроль над каждым шагом (security hardening, firewall, hardening SSH, ручная сборка из исходников) — следуй секциям §0-§9 ниже.

---

## 0. Безопасность сервера

### 0.1 Создать sudo-юзера (на сервере под root)

```bash
adduser deploy
usermod -aG sudo deploy
```

### 0.2 Скопировать SSH-ключ (с локальной машины)

Если SSH-ключа ещё нет — сначала сгенерировать:

```bash
ssh-keygen -t ed25519
```

Скопировать на сервер:

```bash
ssh-copy-id deploy@SERVER_IP
```

### 0.3 Проверить вход по ключу

**Не закрывая текущую сессию**, в новом терминале:

```bash
ssh deploy@SERVER_IP
```

Должен пустить **без пароля**. Если нет — не переходить к следующему шагу.

### 0.4 SSH hardening (только после успешной проверки 0.3)

```bash
sudo sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin no/' /etc/ssh/sshd_config
sudo sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config
sudo sed -i 's/^#\?PubkeyAuthentication.*/PubkeyAuthentication yes/' /etc/ssh/sshd_config
sudo systemctl restart sshd
```

### Firewall

```bash
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw allow 22/tcp
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw allow 8443/tcp
sudo ufw --force enable
```

### fail2ban

```bash
sudo apt install -y fail2ban
sudo systemctl enable --now fail2ban
```

### Автообновления безопасности

```bash
sudo apt install -y unattended-upgrades
sudo dpkg-reconfigure -plow unattended-upgrades
```

---

## 1. Обновление системы

```bash
sudo apt update && sudo apt upgrade -y
```

## 2. Зависимости для сборки

```bash
sudo apt install -y curl git build-essential pkg-config libssl-dev
```

## 3. Docker

```bash
# Удалить старые версии
sudo apt remove -y docker docker-engine docker.io containerd runc 2>/dev/null

# Добавить репозиторий Docker
sudo install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/ubuntu/gpg | sudo gpg --dearmor -o /etc/apt/keyrings/docker.gpg
sudo chmod a+r /etc/apt/keyrings/docker.gpg

echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | sudo tee /etc/apt/sources.list.d/docker.list > /dev/null

# Установить
sudo apt update
sudo apt install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

# Добавить юзера в docker group
sudo usermod -aG docker $USER
newgrp docker

# Проверить
docker --version
docker compose version
```

## 4. Rust

Минимальная версия — **Rust 1.93+** (см. `rust-version` в [Cargo.toml](Cargo.toml)). `rustup` ставит свежий stable, этого достаточно. Если используется `rustup` из apt — сначала `rustup update stable`.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
rustc --version   # должно быть >= 1.93
```

## 5. Сборка и установка Pier

```bash
git clone https://github.com/joveptesg/pier.git /tmp/pier
cd /tmp/pier
sudo bash scripts/build-from-source.sh
```

Скрипт сам определит RAM, при необходимости создаст swap (постоянный, через `/etc/fstab`) и подберёт профиль сборки и `--jobs`. На сервере с ≥ 6 ГБ работает в полном режиме (`profile = release`), на 1–2 ГБ — в `release-lowmem` с `jobs = 1` и swap до 4 ГБ суммарно. По окончании автоматически вызывает `install.sh`.

Флаги: `--no-swap`, `--profile NAME`, `--jobs N`, `--no-install`, `--port PORT`, `-y` (без подтверждения swap).

> Сборка занимает ~5–15 минут в зависимости от мощности сервера.
>
> Если не нужен исходник — можно пропустить шаги 4–5 и взять готовый бинарь:
> ```bash
> mkdir -p /tmp/pier && cd /tmp/pier
> curl -fsSL -o pier https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64
> curl -fsSL -o pier.sha256 https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64.sha256
> sha256sum -c <(awk -v f=pier '{print $1"  "f}' pier.sha256)
> chmod +x pier
> curl -fsSL -o install.sh https://raw.githubusercontent.com/joveptesg/pier/main/scripts/install.sh
> sudo bash install.sh --binary /tmp/pier/pier
> ```

## 6. Проверка

```bash
systemctl status pier
curl localhost:8443/health
journalctl -u pier -f
```

## 7. Первый вход

Открыть в браузере:

```
http://SERVER_IP:8443/setup
```

Создать admin-аккаунт. После этого Pier готов к работе.

---

## 8. Docker Hub / приватные registry

Чтобы Pier мог тянуть образы из Docker Hub без rate-limit (или из приватных registry), есть два пути:

### Вариант A — `docker login` под root

```bash
sudo docker login -u YOUR_USERNAME
# (использовать PAT, не пароль: https://app.docker.com/settings)
```

`install.sh` настраивает systemd unit так, что `/root/.docker/config.json` через bind-mount виден pier-сервису (read-only, в `/opt/pier/host-docker`). При ротации PAT повторяешь `docker login` — Pier сразу подхватывает, рестарт не нужен.

> **Требуется пакет `acl`** — `install.sh` ставит его автоматически (apt/dnf/yum/apk). Если не получилось — увидишь warn:
>
> ```
> [WARN] setfacl not found — пакет 'acl' не установлен...
> ```
>
> В этом случае сделай:
> ```bash
> apt install -y acl
> sudo bash /tmp/pier/scripts/install.sh --binary /tmp/pier/target/release/pier
> ```
>
> Без `acl` сработает fallback `chmod 644` на `config.json`, но **следующий `docker login` сбросит права** и Pier снова перестанет видеть. Установка `acl` решает это навсегда — default ACL наследуется любым будущим `config.json`.

### Вариант B — через UI Pier

Settings → Registries → **«+ Add Docker Hub»** → username + PAT → Save.

Креды хранятся в БД Pier (зашифрованы), per-project или global. Подходит, когда не хочется давать pier-сервису доступ к `/root/.docker` или нужны разные креды для разных проектов.

---

## Порты

| Порт | Назначение |
|------|-----------|
| 22 | SSH |
| 80 | Traefik (HTTP → HTTPS redirect, ACME) |
| 443 | Traefik (reverse proxy для сервисов) |
| 8443 | Pier dashboard |
| 10000+ | Автовыделяемые порты контейнеров |

---

## Обновление Pier

```bash
cd /tmp/pier
git pull
sudo bash scripts/build-from-source.sh
```

---

## Управление

```bash
# Логи
journalctl -u pier -f
journalctl -u pier --since "1h ago"
journalctl -u pier -p err

# Управление сервисом
sudo systemctl restart pier
sudo systemctl stop pier
sudo systemctl start pier

# Конфигурация
sudo nano /opt/pier/.env

# Данные
ls /opt/pier/data/
```
