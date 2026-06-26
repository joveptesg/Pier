# Pier — Installation on a Clean Ubuntu Server

## Quick install (one-liner)

If you need Pier right here and now, without building from source manually:

```bash
curl -fsSL https://pier.team/install | sudo bash
```

The script installs Docker, downloads the pre-built binary from [GitHub Releases](https://github.com/joveptesg/pier/releases/tag/latest) (with sha256 verification), and runs [`install.sh`](scripts/install.sh). Works on a fresh Ubuntu/Debian. For the next steps (creating an admin account at `http://SERVER_IP:8443/setup`) — see §8.

> For alternative installation options (Docker, building from source) — see [README.md](README.md#quick-start).

If you want full control over every step (security hardening, firewall, SSH hardening, building from source) — follow sections §0–§9 below.

---

## 0. Server security

### 0.1 Create a sudo user (on the server, as root)

```bash
adduser deploy
usermod -aG sudo deploy
```

### 0.2 Copy your SSH key (from your local machine)

If you don't have an SSH key yet, generate one first:

```bash
ssh-keygen -t ed25519
```

Copy it to the server:

```bash
ssh-copy-id deploy@SERVER_IP
```

### 0.3 Verify key-based login

**Without closing your current session**, in a new terminal:

```bash
ssh deploy@SERVER_IP
```

It should log you in **without a password**. If it doesn't — do not proceed to the next step.

### 0.4 SSH hardening (only after a successful check in 0.3)

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

### Automatic security updates

```bash
sudo apt install -y unattended-upgrades
sudo dpkg-reconfigure -plow unattended-upgrades
```

---

## 1. Update the system

```bash
sudo apt update && sudo apt upgrade -y
```

## 2. Build dependencies

```bash
sudo apt install -y curl git build-essential pkg-config libssl-dev
```

## 3. Docker

```bash
# Remove old versions
sudo apt remove -y docker docker-engine docker.io containerd runc 2>/dev/null

# Add the Docker repository
sudo install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/ubuntu/gpg | sudo gpg --dearmor -o /etc/apt/keyrings/docker.gpg
sudo chmod a+r /etc/apt/keyrings/docker.gpg

echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | sudo tee /etc/apt/sources.list.d/docker.list > /dev/null

# Install
sudo apt update
sudo apt install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

# Add the user to the docker group
sudo usermod -aG docker $USER
newgrp docker

# Verify
docker --version
docker compose version
```

## 4. Rust

Minimum version — **Rust 1.93+** (see `rust-version` in [Cargo.toml](Cargo.toml)). `rustup` installs the latest stable, which is enough. If you use `rustup` from apt — run `rustup update stable` first.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
rustc --version   # should be >= 1.93
```

## 5. Build and install Pier

```bash
git clone https://github.com/joveptesg/pier.git /tmp/pier
cd /tmp/pier
sudo bash scripts/build-from-source.sh
```

The script detects your RAM, creates swap if needed (persistent, via `/etc/fstab`), and picks the build profile and `--jobs` accordingly. On a server with ≥ 6 GB it runs in full mode (`profile = release`); on 1–2 GB it uses `release-lowmem` with `jobs = 1` and up to 4 GB of swap total. When finished, it automatically runs `install.sh`.

Flags: `--no-swap`, `--profile NAME`, `--jobs N`, `--no-install`, `--port PORT`, `-y` (skip swap confirmation).

> The build takes ~5–15 minutes depending on the server's power.
>
> If you don't need the source — you can skip steps 4–5 and grab the pre-built binary:
> ```bash
> mkdir -p /tmp/pier && cd /tmp/pier
> curl -fsSL -o pier https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64
> curl -fsSL -o pier.sha256 https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64.sha256
> sha256sum -c <(awk -v f=pier '{print $1"  "f}' pier.sha256)
> chmod +x pier
> curl -fsSL -o install.sh https://raw.githubusercontent.com/joveptesg/pier/main/scripts/install.sh
> sudo bash install.sh --binary /tmp/pier/pier
> ```

## 6. Verify

```bash
systemctl status pier
curl localhost:8443/health
journalctl -u pier -f
```

## 7. First login

Open in your browser:

```
http://SERVER_IP:8443/setup
```

Create an admin account. After that, Pier is ready to use.

---

## 8. Docker Hub / private registries

To let Pier pull images from Docker Hub without rate limits (or from private registries), there are two ways:

### Option A — `docker login` as root

```bash
sudo docker login -u YOUR_USERNAME
# (use a PAT, not your password: https://app.docker.com/settings)
```

`install.sh` configures the systemd unit so that `/root/.docker/config.json` is visible to the pier service via a bind-mount (read-only, at `/opt/pier/host-docker`). When you rotate the PAT, just re-run `docker login` — Pier picks it up immediately, no restart needed.

> **The `acl` package is required** — `install.sh` installs it automatically (apt/dnf/yum/apk). If that fails, you'll see a warning:
>
> ```
> [WARN] setfacl not found — the 'acl' package is not installed and could not be installed automatically...
> ```
>
> In that case, run:
> ```bash
> apt install -y acl
> sudo bash /tmp/pier/scripts/install.sh --binary /tmp/pier/target/release/pier
> ```
>
> Without `acl`, a `chmod 644` fallback is applied to `config.json`, but **the next `docker login` will reset the permissions** and Pier will stop seeing it again. Installing `acl` fixes this permanently — the default ACL is inherited by any future `config.json`.

### Option B — via the Pier UI

Settings → Registries → **"+ Add Docker Hub"** → username + PAT → Save.

Credentials are stored in Pier's database (encrypted), per-project or global. This is a good fit when you don't want to give the pier service access to `/root/.docker`, or when you need different credentials for different projects.

---

## Ports

| Port | Purpose |
|------|-----------|
| 22 | SSH |
| 80 | Traefik (HTTP → HTTPS redirect, ACME) |
| 443 | Traefik (reverse proxy for services) |
| 8443 | Pier dashboard |
| 10000+ | Auto-allocated container ports |

---

## Updating Pier

```bash
cd /tmp/pier
git pull
sudo bash scripts/build-from-source.sh
```

---

## Management

```bash
# Logs
journalctl -u pier -f
journalctl -u pier --since "1h ago"
journalctl -u pier -p err

# Service management
sudo systemctl restart pier
sudo systemctl stop pier
sudo systemctl start pier

# Configuration
sudo nano /opt/pier/.env

# Data
ls /opt/pier/data/
```
