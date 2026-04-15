# Pier — Scripts

## Файлы

| Файл | Назначение |
|------|------------|
| `build-bundle.sh` | Сборка бандла для дистрибуции (сторона разработчика) |
| `setup.sh` | Установка на чистый сервер — ставит Docker, вызывает install.sh |
| `install.sh` | Установка Pier как systemd-сервис (требует Docker) |
| `pier.service` | Systemd unit file |
| `pier.env.example` | Пример конфигурации |

---

## Сборка бандла (разработчик)

```bash
cd Pier
bash scripts/build-bundle.sh
```

Результат: `dist/pier-bundle.tar.gz` (~15-20 MB)

Содержимое бандла:
- `pier` — скомпилированный бинарник (Linux x86_64)
- `setup.sh` — автоустановка всего на чистом сервере
- `install.sh` — установка Pier
- `pier.service` — systemd unit

### С загрузкой на S3

```bash
# AWS S3
bash scripts/build-bundle.sh --upload s3://my-bucket/pier/

# Bunny CDN / HTTP PUT
STORAGE_API_KEY=xxx bash scripts/build-bundle.sh --upload https://storage.bunnycdn.com/pier/
```

---

## Установка на сервер (клиент)

### Одна команда на чистом Ubuntu

```bash
wget https://storage.example.com/pier/pier-bundle.tar.gz && tar xzf pier-bundle.tar.gz && sudo bash setup.sh
```

`setup.sh` автоматически:
1. Устанавливает Docker CE + Docker Compose plugin
2. Устанавливает git, curl, ca-certificates
3. Создает пользователя `pier` + каталоги `/opt/pier/`
4. Устанавливает бинарник в `/opt/pier/bin/pier`
5. Регистрирует systemd-сервис `pier`
6. Запускает Pier

### Если Docker уже установлен

```bash
sudo bash install.sh --binary ./pier
```

### С кастомным портом

```bash
sudo bash setup.sh --port 9000
```

---

## После установки

Dashboard: `http://<IP>:8443`

Первый визит: `http://<IP>:8443/setup` — создание admin-аккаунта.

```bash
# Логи
journalctl -u pier -f

# Статус
systemctl status pier

# Рестарт
systemctl restart pier

# Конфигурация
cat /opt/pier/.env

# Данные
ls /opt/pier/data/
```

---

## Обновление Pier

```bash
# Загрузить новый бандл
wget https://storage.example.com/pier/pier-bundle.tar.gz
tar xzf pier-bundle.tar.gz

# Обновить (остановит, заменит бинарник, запустит)
sudo bash install.sh --binary ./pier
```

Конфигурация `/opt/pier/.env` сохраняется при обновлении.
