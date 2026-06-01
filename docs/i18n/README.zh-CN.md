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

## 快速开始

### 方式 A：一键安装（Ubuntu/Debian）

```bash
curl -fsSL https://pier.team/install | sudo bash
```

> 该短链接会重定向到 [`scripts/bootstrap.sh`](../../scripts/bootstrap.sh)。脚本会安装 Docker，下载最新发行版二进制文件（带 sha256 校验），并运行 `install.sh`。随时重新运行即可更新到最新发行版。

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

### 方式 D：从预构建发行版安装（无需构建）

已经装好了 Docker？直接获取最新的预构建二进制文件 — 无需 Rust 工具链，无需编译：

```bash
# 1. 下载预构建二进制文件 + 校验和（linux/amd64）
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64 -o pier-linux-amd64
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64.sha256 -o pier-linux-amd64.sha256
sha256sum -c pier-linux-amd64.sha256          # 校验完整性

# 2. 获取安装脚本并运行
curl -fL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/install.sh -o install.sh
chmod +x pier-linux-amd64
sudo bash install.sh --binary ./pier-linux-amd64
```

> 这是方式 A 的手动等价流程，但不会自动安装 Docker。要求系统中已存在 Docker + Compose（参见 [INSTALL.md](../../INSTALL.md)）。二进制文件名必须保持为 `pier-linux-amd64`，这样 `sha256sum -c` 才能匹配。

### 更新 Pier

更新会拉取全新的**预构建二进制文件** — 无需从源码重新构建。`install.sh` 会检测正在运行的服务、停止它、替换二进制文件并重新启动，同时保留你的 `.env` 和 `/opt/pier/data`。

```bash
# 最简单的方式 —— 重新运行一键安装脚本（会重新下载最新发行版）：
curl -fsSL https://pier.team/install | sudo bash

# 或手动操作，流程与方式 D 相同（下载 → 校验 → install.sh）。
```

然后打开 `http://YOUR_SERVER_IP:8443/setup` 创建管理员账号。

> 如需详细的服务器配置指南（安全加固、防火墙、Docker 安装），请参阅 [INSTALL.md](../../INSTALL.md)。

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
- ✨ **自动构建 (Railpack)** — 直接从源代码零配置构建,支持 Node、Python、Go、PHP、Java、Ruby、Rust、Vite/Astro/CRA 等,无需编写 Dockerfile
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

## npm 仓库

**内置私有 + 代理 npm 仓库，直接编入二进制文件。** 无需单独的 Verdaccio 容器或额外数据库 — Pier 在 `/registry/npm/` 提供 npm 兼容 API，透明镜像 `registry.npmjs.org`，并与所有现代包管理器协同工作。

### 支持的客户端

| 客户端 | 版本 | 说明 |
|---|---|---|
| **npm** | 7 – 11 | 开箱即用 |
| **yarn classic** | 1.22.x | 在 `.npmrc` 中添加 `always-auth=true` |
| **yarn berry** | 2 · 3 · 4 | `.yarnrc.yml` 中设置 `npmAlwaysAuth: true` |
| **pnpm** | 9 · 10 | 开箱即用 |
| **bun** | latest | 开箱即用 |

### 支持的命令

| 命令 | 状态 |
|---|---|
| `npm install` / `yarn add` / `pnpm add` / `bun add` | ✓ |
| `npm publish`(scoped + unscoped) | ✓ |
| `npm login`(CouchDB + `--auth-type=web`) | ✓ |
| `npm dist-tag add / rm / ls` | ✓ |
| `npm deprecate` | ✓ |
| `npm unpublish`(单版本 + 整包) | ✓ |
| `npm whoami` · `npm ping` | ✓ |

### Upstream 代理模式

Pier 可以透明镜像 **npmjs.org**(或任何 npm 兼容 upstream)— 整个团队使用一个 URL。packument 元数据被缓存并通过 `If-None-Match` 进行重新验证,tarball 在首次 `install` 时按需拉取,后台 LRU GC 将磁盘缓存控制在可配置上限内。在 **Packages → Upstream proxy** 中管理。

- 整个团队的 `.npmrc` 只用一个 URL — 无需 scope routing
- 即使 `npmjs.org` 宕机也能继续 install
- 审计:清晰可见团队实际使用了哪些公共包
- 基于 TTL 的重新验证,304 直接短路

### 快速开始

```ini
# 项目中的 .npmrc
registry=https://YOUR-PIER-HOST/registry/npm/
//YOUR-PIER-HOST/registry/npm/:_authToken=pier_npm_xxx
always-auth=true
```

在 **Packages → Manage tokens** 中创建令牌,然后:

```bash
npm publish                  # 私有包
npm install left-pad         # 从 npmjs.org 代理 + 缓存
```

完整的客户端指南:[npm](https://pier.team/docs/registry/clients/npm) · [yarn 1.x](https://pier.team/docs/registry/clients/yarn-classic) · [yarn 2/3/4](https://pier.team/docs/registry/clients/yarn-berry) · [pnpm](https://pier.team/docs/registry/clients/pnpm) · [bun](https://pier.team/docs/registry/clients/bun)。

## 自动构建 (Railpack)

**自动构建**源类型让你可以从 Git 仓库部署应用而**无需编写 Dockerfile**。Pier 底层调用 [Railpack](https://github.com/railwayapp/railpack)（Railway 开源的构建器，Nixpacks 的继任者），与本地 [moby/buildkit](https://github.com/moby/buildkit) 守护进程协作。这两个组件由 `install.sh` 自动配置。完整指南见 [from-railpack 文档](https://pier.team/docs/applications/from-railpack)。

> ### ⚠ 服务器要求 — 启用前请先阅读
>
> 自动构建**比其他部署方式重得多**。在主机上编译用户代码的资源需求与直接运行预构建容器有本质区别：
>
> |              | Dockerfile / Compose / Docker Image | 自动构建 (Railpack) |
> |---|---|---|
> | 最低内存     | 512 MB                              | **4 GB**（Rust 项目 8 GB） |
> | 可用磁盘     | 每个堆栈几个 GB                     | **40+ GB**（BuildKit 缓存） |
> | 首次部署     | 数秒                                | 1–10 分钟 |
>
> **如果您的 VPS 内存少于 4 GB,请改用 Dockerfile 或 Docker Image 源类型。** 当主机内存 &lt;4 GB 时 UI 会显示醒目的警告——构建几乎肯定会因 OOM 被内核杀死(可能影响自身或其他进程)。Pier-core 每天将 BuildKit 缓存修剪到 ~10 GB / 7 天保留期。如果完全不需要,可以用 `PIER_SKIP_RAILPACK=1 bash install.sh` 跳过安装。

**Railpack 自动识别的语言**（无需手动配置）：

| 语言 / 框架 | 检测依据 |
|---|---|
| Node.js / Bun / Deno | `package.json`、`bun.lockb`、`deno.json` |
| Python | `requirements.txt`、`pyproject.toml`、`Pipfile` |
| Go | `go.mod` |
| Rust | `Cargo.toml` |
| PHP | `composer.json` |
| Java | `pom.xml`、`build.gradle` |
| Ruby | `Gemfile` |
| Elixir | `mix.exs` |
| Vite / Astro / CRA 静态站点 | 打包器配置 + 构建输出目录 |

如需覆盖默认行为,在仓库根目录放置 [`railpack.json`](https://railpack.com/configuration/file),Railpack 会自动识别。

**调优参数**（在 systemd 单元中设置,或在 `install.sh` 之前导出）：

- `PIER_RAILPACK_MAX_PARALLEL_BUILDS=N` — 并发构建上限（默认 1）。也可在 UI 中通过`设置 → Auto-build (Railpack)`修改。
- `PIER_BUILDKIT_MEMORY=4g` — buildkit 容器的内存限制（默认 4g）。
- `PIER_SKIP_RAILPACK=1` — 完全跳过安装。UI 中的卡片仍会显示,但点击构建会提示"railpack binary not found"。

**FAQ**

- **为什么不用 Nixpacks?** Railpack 是积极维护的继任者(Railway 于 2025 年 3 月切换),Nixpacks 已进入维护模式。Railpack 借助 BuildKit 图模型,Node 镜像减小约 38%,Python 镜像减小约 77%。
- **支持 ARM/aarch64 吗?** 支持 —— `railpack` 和 `moby/buildkit` 都提供 linux/arm64 构建产物。install.sh 自动选择正确架构。
- **可以关闭吗?** 可以 —— `PIER_SKIP_RAILPACK=1 bash install.sh` 会跳过安装。Dockerfile / Compose / Docker Image 源类型不受影响。

## 模板

**数据库** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**服务** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**游戏** — Minecraft, Terraria

**VPN** — AmneziaWG

**应用程序** — 从 Dockerfile、Docker 镜像或 Docker Compose 部署

> 没找到需要的？可手动部署任意 Docker 镜像或 Compose 堆栈。

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
