# Pier — Установка на чистый Ubuntu-сервер

## 0. Безопасность сервера

### Создать sudo-юзера (под root)

```bash
adduser deploy
usermod -aG sudo deploy
```

### Скопировать SSH-ключ (с локальной машины)

```bash
ssh-copy-id deploy@SERVER_IP
```

### SSH hardening

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

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
rustc --version
```

## 5. Сборка Pier

```bash
git clone https://github.com/joveptesg/pier.git /tmp/pier
cd /tmp/pier
cargo build --release
```

> Сборка занимает ~5-10 минут.

## 6. Установка

```bash
sudo bash scripts/install.sh --binary target/release/pier
```

## 7. Проверка

```bash
systemctl status pier
curl localhost:8443/health
journalctl -u pier -f
```

## 8. Первый вход

Открыть в браузере:

```
http://SERVER_IP:8443/setup
```

Создать admin-аккаунт. После этого Pier готов к работе.

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
cargo build --release
sudo bash scripts/install.sh --binary target/release/pier
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
