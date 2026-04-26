> この文書は [README.md](../../README.md) の翻訳です。内容に相違がある場合は、英語版を参照してください。

<p align="center">
  <img src="../logo.svg" alt="Pier" height="120">
</p>

<h3 align="center">軽量なセルフホスト型 PaaS。<br>単一バイナリ。20 MB RAM。何でもデプロイ。</h3>

<p align="center">
  <a href="https://github.com/joveptesg/pier/blob/main/LICENSE"><img src="https://img.shields.io/github/license/joveptesg/pier?color=blue" alt="License"></a>
  <a href="https://github.com/joveptesg/pier/stargazers"><img src="https://img.shields.io/github/stars/joveptesg/pier?style=flat" alt="Stars"></a>
  <a href="https://github.com/joveptesg/pier/releases"><img src="https://img.shields.io/github/v/release/joveptesg/pier" alt="Release"></a>
  <img src="https://img.shields.io/badge/rust-1.93%2B-orange" alt="Rust">
</p>

<p align="center">
  <a href="../../README.md">English</a> |
  <a href="README.ru.md">Русский</a> |
  <a href="README.zh-CN.md">中文</a> |
  <a href="README.de.md">Deutsch</a> |
  <strong>日本語</strong> |
  <a href="README.es.md">Español</a> |
  <a href="README.fr.md">Français</a> |
  <a href="README.pt-BR.md">Português</a>
</p>

---

## Pier とは？

**Pier は Coolify / Heroku / Vercel のオープンソース・セルフホスト型代替ツールです — $5 の VPS でも動作するほど軽量。**

コンテナ、Docker Compose スタック、Git リポジトリを、自動 SSL、リバースプロキシ、モダンな Web ダッシュボード付きでデプロイ — すべて単一の Rust バイナリから、わずか **20〜40 MB の RAM** で動作します。

<!-- 
<p align="center">
  <img src="docs/screenshots/dashboard.png" alt="Pier Dashboard" width="800">
</p>
-->

## なぜ Pier なのか？

[Coolify](https://coolify.io) は優れたツールですが、**6 つ以上のコンテナ**を実行し、アイドル時に **750 MB〜1.2 GB の RAM** を消費します。Pier は同等のコア機能を単一バイナリで提供します。

| | Coolify | Pier |
|---|---|---|
| **アイドル時 RAM** | 750 MB – 1.2 GB | 20–40 MB (+Traefik) |
| **ディスク** | ~1 GB | ~15–30 MB |
| **実行コンテナ数** | 6+ (Laravel, PostgreSQL, Redis, Soketi, Horizon, Traefik) | 1 binary + Traefik |
| **最小 VPS** | 2 GB RAM, 2 vCPU | 512 MB RAM, 1 vCPU |
| **データベース** | External PostgreSQL | Embedded SQLite |
| **言語** | PHP / Laravel | Rust |
| **フロントエンド JS** | ~300 KB+ | ~30 KB (HTMX + Alpine.js) |

## 機能

**コンテナ & スタック**
- 📦 Docker コンテナ管理 — 作成、起動、停止、再起動、削除、ログ、統計
- 🐳 Docker Compose スタック（内蔵 YAML エディタ付き）
- 🚀 **30 以上のテンプレート**からワンクリックデプロイ

**Git & デプロイメント**
- 🔄 GitHub & GitLab Webhook による Git デプロイパイプライン
- 🛠 Dockerfile、Docker イメージ、または Compose からのビルド
- ⏪ ロールバック付きデプロイ履歴

**ネットワーク & SSL**
- 🌐 Traefik によるリバースプロキシと自動 HTTPS
- 🔒 Let's Encrypt SSL 証明書（自動プロビジョニング）
- 🔗 自動生成サービス URL 付きカスタムドメイン

**インフラストラクチャ**
- 🖥 リモートエージェントによるマルチサーバー管理
- 💾 S3 連携によるスケジュールバックアップ
- 📊 リアルタイムモニタリング — CPU、RAM、ディスク、ネットワーク

**開発者体験**
- ⚡ HTMX + Alpine.js で構築された Web UI — ダークモード、リアルタイム、レスポンシブ
- 🔑 bcrypt パスワードハッシュによる JWT 認証
- 🗃 組み込み SQLite — 外部データベース不要
- ⚙️ ワンコマンドでサーバーセットアップ

## テンプレート

**データベース** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**サービス** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**ゲーム** — Minecraft, Terraria

**VPN** — AmneziaWG

**アプリケーション** — Dockerfile、Docker イメージ、または Docker Compose からデプロイ

> 必要なものが見つかりませんか？任意の Docker イメージや Compose スタックを手動でデプロイできます。

## クイックスタート

### オプション A: ワンコマンドインストール (Ubuntu/Debian)

```bash
curl -fsSL https://pier.team/install | sudo bash
```

### オプション B: ソースからビルド

```bash
git clone https://github.com/joveptesg/pier.git
cd pier
cargo build --release
sudo bash scripts/install.sh --binary target/release/pier
```

### オプション C: Docker

```bash
docker run -d \
  --name pier \
  -p 8443:8443 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v pier-data:/app/data \
  ghcr.io/joveptesg/pier:latest
```

次に `http://YOUR_SERVER_IP:8443/setup` を開いて管理者アカウントを作成します。

> 詳細なサーバーセットアップ（セキュリティ強化、ファイアウォール、Docker インストール）については、[INSTALL.md](../../INSTALL.md) を参照してください。

## 技術スタック

| レイヤー | 技術 | 用途 |
|---|---|---|
| 言語 | [Rust](https://www.rust-lang.org) | パフォーマンス、安全性、単一バイナリ |
| HTTP | [Axum](https://github.com/tokio-rs/axum) | 非同期 API + WebSocket |
| Docker | [Bollard](https://github.com/fussybeaver/bollard) | Docker Engine API |
| データベース | [SQLite](https://github.com/rusqlite/rusqlite) | 組み込み永続化 |
| プロキシ | [Traefik](https://traefik.io) | 自動ルーティング + Let's Encrypt |
| テンプレート | [MiniJinja](https://github.com/mitsuhiko/minijinja) | サーバーサイドレンダリング |
| フロントエンド | [HTMX](https://htmx.org) + [Alpine.js](https://alpinejs.dev) | 最小限の JS、リアルタイム |
| スタイリング | [Tailwind CSS](https://tailwindcss.com) | ダークモード、レスポンシブ |
| ランタイム | [Tokio](https://tokio.rs) | 非同期 I/O |
| ストレージ | [AWS S3](https://crates.io/crates/aws-sdk-s3) | バックアップストレージ |
| 認証 | JWT + bcrypt | ステートレス認証 |

## アーキテクチャ

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

> 詳細なアーキテクチャについては、[ARCHITECTURE.md](../../ARCHITECTURE.md) を参照してください。

## ロードマップ

- [x] コンテナ管理 (Docker API)
- [x] Docker Compose スタック（YAML エディタ付き）
- [x] ワンクリックサービステンプレート (30+)
- [x] リバースプロキシ + 自動 SSL (Traefik + Let's Encrypt)
- [x] Git Webhook + 自動デプロイ (GitHub, GitLab)
- [x] エージェントによるマルチサーバー管理
- [x] S3 対応バックアップスケジューラ
- [x] Web ダッシュボード (HTMX + Tailwind、ダークモード)
- [x] S3 バケット管理
- [x] アーキテクチャ可視化 (Canvas)
- [ ] RBAC（ロールベースアクセス制御）
- [ ] 2FA（TOTP + WebAuthn）
- [ ] 負荷分散 + 水平スケーリング
- [ ] アラート通知 (Telegram, Discord, Slack)
- [ ] 自動アップデート機能
- [ ] プロジェクトごとの Docker ネットワーク分離
- [ ] Pingora ベースのリバースプロキシ (Traefik 置換)

## コントリビューション

コントリビューションを歓迎します！プルリクエストを送る前に [CONTRIBUTING.md](../../CONTRIBUTING.md) をお読みください。すべてのコントリビューターは [CLA](../../CLA.md) に同意していただく必要があります。

```bash
cargo fmt          # Format code
cargo clippy       # Lint
cargo test         # Run tests
cargo build        # Build
```

## ライセンス

[AGPL-3.0](../../LICENSE)

Pier はセルフホストおよび改変が自由に行えます。改変版をネットワークサービスとして提供する場合は、同一ライセンスの下で改変部分を公開する必要があります。

商用ライセンス（AGPL 義務なしでの利用）については、[info@devcom.app](mailto:info@devcom.app) までお問い合わせください。

---

<p align="center">
  <sub>🦀 Rust で構築 — 高速、安全、軽量。</sub>
</p>
