# Pier — Scripts

## Файлы

| Файл | Назначение |
|------|------------|
| `build-bundle.sh` | Сборка бандла для дистрибуции (сторона разработчика) |
| `build-from-source.sh` | Адаптивная сборка из исходников на сервере: подбирает профиль/jobs по RAM, гарантирует swap, затем вызывает install.sh |
| `lib-swap.sh` | Общая идемпотентная функция `ensure_swap` (подключается через `source` из build-from-source.sh и install.sh) |
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

---

## Сборка из исходников на сервере (адаптивная)

Когда на сервере нужно собрать `pier` из исходников (например, после `git pull`),
используйте `build-from-source.sh` — он сам подбирает стратегию под железо и **гарантирует
swap до сборки**, чтобы тяжёлые крейты (`aws-sdk-s3`, `aws-lc-sys`, `ring`) не падали по OOM
(`rustc ... signal: 9, SIGKILL`).

```bash
cd Pier
sudo bash scripts/build-from-source.sh -y      # подберёт профиль/jobs, дольёт swap, соберёт и установит
```

Стратегия по **физической** RAM:

| RAM | Профиль | Jobs |
|-----|---------|------|
| ≥ 6 ГБ | `release` | `nproc` |
| 3–6 ГБ | `release` | `ram_gb / 2` |
| < 3 ГБ | `release-lowmem` | 1 |

Полезные флаги: `--no-swap`, `--profile <release\|release-lowmem>`, `--jobs N`, `--no-install`, `--port`.

### Поведение swap (`lib-swap.sh`)

`ensure_swap FLOOR_MB TARGET_MB` создаёт `/swapfile`, добивая память на **дефицит**.
Желаемый **суммарный** swap = `min( max(FLOOR, TARGET − RAM), 2 × RAM )`, далее доливается
только разница к уже имеющемуся swap (округление вверх до ГиБ):

- **Floor 4 ГБ** (`SWAP_FLOOR_MB`) — аварийный клапан против OOM **даже на сильном сервере**
  (16 ГБ RAM → всё равно 4 ГБ swap; 16 ГБ + уже 2 ГБ swap → добавит 2 ГБ, **итого 4 ГБ**).
- **Build target** — `RAM + swap` доводится до цели: `release` → 6 ГБ, `release-lowmem` → 4 ГБ.
- **Cap 2×RAM** — крошечные VPS не получают избыточный swap: **1 ГБ RAM → 2 ГБ swap** (а не 5 ГБ).
- `swap = RAM` сознательно не используется (недобор на слабых VPS, перерасход диска на жирных).
- **Disk-cap:** держит запас под артефакты сборки (`SWAP_DISK_HEADROOM_MB`, дефолт 4096). Если
  желаемый swap не влезает — **урезает** до помещающегося (вниз до ГиБ), а не пропускает молча;
  если свободно <1 ГБ сверх запаса — пропускает с `WARN`.
- **Идемпотентно:** повторный запуск не пересоздаёт swap и не дублирует строку в `/etc/fstab`.
- При создании swap выставляет `vm.swappiness=10` (best practice для серверов) — но **только если**
  текущее значение дефолтное `60`; кастомную настройку оператора не трогает. `0` не используется
  (риск OOM при наличии swap).

Переменные окружения: `SWAP_DISK_HEADROOM_MB` (запас диска под сборку), `PIER_SKIP_SWAP=1` (отключить
swap в install.sh).

`install.sh` тоже подключает `lib-swap.sh` и держит **floor 4 ГБ** как рантайм-страховку
(для pier + buildkit-сборок образов). Это **не лечит** OOM самой сборки `pier` — для этого нужен
`build-from-source.sh` (swap **до** компиляции). Отключить swap в install.sh: `PIER_SKIP_SWAP=1`.
