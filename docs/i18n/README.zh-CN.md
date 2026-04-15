> 本文档翻译自 [README.md](../../README.md)。如有出入，请以英文版为准。

<p align="center">
  <img src="../logo.svg" alt="Pier" height="120">
</p>

<h3 align="center">轻量级自托管 PaaS 平台。<br>单一二进制文件。20 MB 内存。部署一切。</h3>

<p align="center">
  <a href="https://github.com/joveptesg/pier/blob/main/LICENSE"><img src="https://img.shields.io/github/license/joveptesg/pier?color=blue" alt="License"></a>
  <a href="https://github.com/joveptesg/pier/stargazers"><img src="https://img.shields.io/github/stars/joveptesg/pier?style=flat" alt="Stars"></a>
  <a href="https://github.com/joveptesg/pier/releases"><img src="https://img.shields.io/github/v/release/joveptesg/pier" alt="Release"></a>
  <img src="https://img.shields.io/badge/rust-1.93%2B-orange" alt="Rust">
</p>

<p align="center">
  <a href="../../README.md">English</a> |
  <a href="README.ru.md">Русский</a> |
  <strong>中文</strong> |
  <a href="README.de.md">Deutsch</a> |
  <a href="README.ja.md">日本語</a> |
  <a href="README.es.md">Español</a> |
  <a href="README.fr.md">Français</a> |
  <a href="README.pt-BR.md">Português</a>
</p>

---

## 什么是 Pier？

**Pier 是 Coolify / Heroku / Vercel 的开源自托管替代方案 — 轻量到可以运行在 $5 的 VPS 上。**

部署容器、Docker Compose 堆栈和 Git 仓库，自动配置 SSL、反向代理和现代化 Web 控制面板 — 一切来自单个 Rust 二进制文件，仅需 **20–40 MB 内存**。

<!-- 
<p align="center">
  <img src="../screenshots/dashboard.png" alt="Pier Dashboard" width="800">
</p>
-->

## 为什么选择 Pier？

[Coolify](https://coolify.io) 是一款出色的工具，但它需要运行 **6 个以上的容器**，空闲时消耗 **750 MB – 1.2 GB 内存**。Pier 以单一二进制文件提供同等核心功能。

| | Coolify | Pier |
|---|---|---|
| **空闲内存** | 750 MB – 1.2 GB | 20–40 MB (+Traefik) |
| **磁盘占用** | ~1 GB | ~15–30 MB |
| **运行容器数** | 6+（Laravel、PostgreSQL、Redis、Soketi、Horizon、Traefik） | 1 个二进制文件 + Traefik |
| **最低 VPS 配置** | 2 GB RAM，2 vCPU | 512 MB RAM，1 vCPU |
| **数据库** | 外部 PostgreSQL | 内嵌 SQLite |
| **开发语言** | PHP / Laravel | Rust |
| **前端 JS** | ~300 KB+ | ~30 KB（HTMX + Alpine.js） |

## 功能特性

**容器与堆栈**
- 📦 Docker 容器管理 — 创建、启动、停止、重启、删除、日志、统计
- 🐳 Docker Compose 堆栈，内置 YAML 编辑器
- 🚀 **30 多个模板**一键部署

**Git 与部署**
- 🔄 Git 到部署流水线，支持 GitHub 和 GitLab Webhooks
- 🛠 支持从 Dockerfile、Docker 镜像或 Compose 构建
- ⏪ 部署历史与回滚

**网络与 SSL**
- 🌐 通过 Traefik 反向代理，自动 HTTPS
- 🔒 Let's Encrypt SSL 证书（自动签发）
- 🔗 自定义域名与自动生成的服务 URL

**基础设施**
- 🖥 多服务器管理，支持远程代理
- 💾 定时备份，集成 S3 存储
- 📊 实时监控 — CPU、内存、磁盘、网络

**开发者体验**
- ⚡ 基于 HTMX + Alpine.js 的 Web UI — 暗色模式、实时更新、响应式
- 🔑 JWT 认证与 bcrypt 密码哈希
- 🗃 内嵌 SQLite — 无需外部数据库
- ⚙️ 一条命令完成服务器配置

## 模板

**数据库** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**服务** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**游戏** — Minecraft, Terraria

**VPN** — AmneziaWG

**应用程序** — 从 Dockerfile、Docker 镜像或 Docker Compose 部署

> 没找到需要的？可手动部署任意 Docker 镜像或 Compose 堆栈。

## 快速开始

### 方式 A：一键安装（Ubuntu/Debian）

```bash
curl -fsSL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/setup.sh | sudo bash
```

### 方式 B：从源码构建

```bash
git clone https://github.com/joveptesg/pier.git
cd pier
cargo build --release
sudo bash scripts/install.sh --binary target/release/pier
```

### 方式 C：Docker

```bash
docker run -d \
  --name pier \
  -p 8443:8443 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v pier-data:/app/data \
  ghcr.io/joveptesg/pier:latest
```

然后打开 `http://YOUR_SERVER_IP:8443/setup` 创建管理员账号。

> 如需详细的服务器配置指南（安全加固、防火墙、Docker 安装），请参阅 [INSTALL.md](../../INSTALL.md)。

## 技术栈

| 层级 | 技术 | 用途 |
|---|---|---|
| Language | [Rust](https://www.rust-lang.org) | 高性能、安全、单一二进制 |
| HTTP | [Axum](https://github.com/tokio-rs/axum) | 异步 API + WebSocket |
| Docker | [Bollard](https://github.com/fussybeaver/bollard) | Docker Engine API |
| Database | [SQLite](https://github.com/rusqlite/rusqlite) | 内嵌持久化存储 |
| Proxy | [Traefik](https://traefik.io) | 自动路由 + Let's Encrypt |
| Templates | [MiniJinja](https://github.com/mitsuhiko/minijinja) | 服务端渲染 |
| Frontend | [HTMX](https://htmx.org) + [Alpine.js](https://alpinejs.dev) | 极简 JS、实时交互 |
| Styling | [Tailwind CSS](https://tailwindcss.com) | 暗色模式、响应式 |
| Runtime | [Tokio](https://tokio.rs) | 异步 I/O |
| Storage | [AWS S3](https://crates.io/crates/aws-sdk-s3) | 备份存储 |
| Auth | JWT + bcrypt | 无状态认证 |

## 架构

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

> 如需了解详细架构，请参阅 [ARCHITECTURE.md](../../ARCHITECTURE.md)。

## 路线图

- [x] 容器管理（Docker API）
- [x] Docker Compose 堆栈与 YAML 编辑器
- [x] 一键服务模板（30+）
- [x] 反向代理 + 自动 SSL（Traefik + Let's Encrypt）
- [x] Git Webhooks + 自动部署（GitHub、GitLab）
- [x] 多服务器管理与代理
- [x] 定时备份与 S3 支持
- [x] Web 控制面板（HTMX + Tailwind，暗色模式）
- [x] S3 存储桶管理
- [x] 架构可视化（Canvas）
- [ ] RBAC（基于角色的访问控制）
- [ ] 双因素认证（TOTP + WebAuthn）
- [ ] 负载均衡 + 水平扩展
- [ ] 告警通知（Telegram、Discord、Slack）
- [ ] 自动更新机制
- [ ] 按项目隔离 Docker 网络
- [ ] 基于 Pingora 的反向代理（替代 Traefik）

## 参与贡献

欢迎贡献！请在提交 Pull Request 之前阅读 [CONTRIBUTING.md](../../CONTRIBUTING.md)。所有贡献者须同意我们的 [CLA](../../CLA.md)。

```bash
cargo fmt          # Format code
cargo clippy       # Lint
cargo test         # Run tests
cargo build        # Build
```

## 许可证

[AGPL-3.0](../../LICENSE)

Pier 可免费自托管和修改。如果您将修改后的版本作为网络服务对外提供，则必须在相同许可证下公开您的修改。

如需商业许可（免除 AGPL 义务），请联系 [info@devcom.app](mailto:info@devcom.app)。

---

<p align="center">
  <sub>使用 🦀 Rust 构建 — 快速、安全、轻量。</sub>
</p>
