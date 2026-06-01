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

## クイックスタート

### オプション A: ワンコマンドインストール (Ubuntu/Debian)

```bash
curl -fsSL https://pier.team/install | sudo bash
```

> この短縮 URL は [`scripts/bootstrap.sh`](../../scripts/bootstrap.sh) にリダイレクトされます。スクリプトは Docker をインストールし、最新リリースのバイナリをダウンロード（sha256 検証付き）して `install.sh` を実行します。最新リリースに更新したいときは、いつでも再実行できます。

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

### オプション D: ビルド済みリリースからインストール（ビルド不要）

すでに Docker をお持ちですか？最新のビルド済みバイナリを直接取得できます — Rust ツールチェーンもコンパイルも不要です:

```bash
# 1. ビルド済みバイナリ + チェックサムをダウンロード (linux/amd64)
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64 -o pier-linux-amd64
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64.sha256 -o pier-linux-amd64.sha256
sha256sum -c pier-linux-amd64.sha256          # 整合性を検証

# 2. インストーラーを取得して実行
curl -fL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/install.sh -o install.sh
chmod +x pier-linux-amd64
sudo bash install.sh --binary ./pier-linux-amd64
```

> オプション A の手動版で、Docker の自動インストールを省いたものです。Docker + Compose が既にインストールされている必要があります（[INSTALL.md](../../INSTALL.md) を参照）。`sha256sum -c` が一致するように、バイナリのファイル名は `pier-linux-amd64` のままにしてください。

### Pier の更新

更新では新しい**ビルド済みバイナリ**を取得します — ソースからの再ビルドは不要です。`install.sh` は実行中のサービスを検出して停止し、バイナリを入れ替えてから再起動します。その際、`.env` と `/opt/pier/data` は保持されます。

```bash
# 最も簡単 — ワンコマンドインストーラーを再実行（最新リリースを再ダウンロード）:
curl -fsSL https://pier.team/install | sudo bash

# または手動で、オプション D と同じ流れ（ダウンロード → 検証 → install.sh）。
```

次に `http://YOUR_SERVER_IP:8443/setup` を開いて管理者アカウントを作成します。

> 詳細なサーバーセットアップ（セキュリティ強化、ファイアウォール、Docker インストール）については、[INSTALL.md](../../INSTALL.md) を参照してください。

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
- ✨ **自動ビルド (Railpack)** — Node、Python、Go、PHP、Java、Ruby、Rust、Vite/Astro/CRA など、Dockerfile 不要でソースから直接ゼロコンフィグでビルド
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

## npm レジストリ

**プライベート + プロキシ npm レジストリをバイナリに内蔵。** 別途の Verdaccio コンテナや外部 DB は不要 — Pier は `/registry/npm/` で npm 互換 API を提供し、`registry.npmjs.org` を透過的にミラーし、すべての主要なパッケージマネージャーで動作します。

### 対応クライアント

| クライアント | バージョン | 備考 |
|---|---|---|
| **npm** | 7 – 11 | 設定不要で動作 |
| **yarn classic** | 1.22.x | `.npmrc` に `always-auth=true` を追加 |
| **yarn berry** | 2 · 3 · 4 | `.yarnrc.yml` に `npmAlwaysAuth: true` |
| **pnpm** | 9 · 10 | 設定不要で動作 |
| **bun** | latest | 設定不要で動作 |

### 対応コマンド

| コマンド | ステータス |
|---|---|
| `npm install` / `yarn add` / `pnpm add` / `bun add` | ✓ |
| `npm publish`(scoped + unscoped) | ✓ |
| `npm login`(CouchDB フロー + `--auth-type=web`) | ✓ |
| `npm dist-tag add / rm / ls` | ✓ |
| `npm deprecate` | ✓ |
| `npm unpublish`(単一バージョン + パッケージ全体) | ✓ |
| `npm whoami` · `npm ping` | ✓ |

### Upstream プロキシモード

Pier は **npmjs.org**(または npm 互換の任意の upstream)を透過的にミラーし、チーム全体が 1 つの URL を使えるようにします。packument メタデータはキャッシュされ `If-None-Match` で再検証、tarball は `install` 時に遅延取得、バックグラウンドの LRU GC が設定上限内にディスクキャッシュを保ちます。**Packages → Upstream proxy** で管理。

- チーム全体の `.npmrc` で 1 つの URL — scope routing 不要
- `npmjs.org` がダウンしても `install` は動き続ける
- 監査: チームが実際に使う公開パッケージが可視化
- 304 ショートカットによる TTL ベースの再検証

### クイックスタート

```ini
# プロジェクトの .npmrc
registry=https://YOUR-PIER-HOST/registry/npm/
//YOUR-PIER-HOST/registry/npm/:_authToken=pier_npm_xxx
always-auth=true
```

**Packages → Manage tokens** でトークンを発行、次に:

```bash
npm publish                  # プライベートパッケージ
npm install left-pad         # npmjs.org からプロキシ + キャッシュ
```

クライアント別の詳細ガイド: [npm](https://pier.team/docs/registry/clients/npm) · [yarn 1.x](https://pier.team/docs/registry/clients/yarn-classic) · [yarn 2/3/4](https://pier.team/docs/registry/clients/yarn-berry) · [pnpm](https://pier.team/docs/registry/clients/pnpm) · [bun](https://pier.team/docs/registry/clients/bun)。

## 自動ビルド (Railpack)

**自動ビルド**ソースを使うと、**Dockerfile を書かずに** Git リポジトリからアプリをデプロイできます。内部的に Pier は [Railpack](https://github.com/railwayapp/railpack) (Railway がオープンソース化したビルダー、Nixpacks の後継) を呼び出し、ローカルの [moby/buildkit](https://github.com/moby/buildkit) デーモンと連携します。両コンポーネントは `install.sh` が自動でセットアップします。詳しい手順は [from-railpack ガイド](https://pier.team/docs/applications/from-railpack) を参照してください。

> ### ⚠ サーバー要件 — 有効化前に必ず読んでください
>
> 自動ビルドは他のデプロイ方法よりも**大幅に重い**処理です。ホスト上でユーザーコードをコンパイルするのは、ビルド済みコンテナを実行するのとは根本的に異なるため、リソースプロファイルも変わります:
>
> |              | Dockerfile / Compose / Docker Image | 自動ビルド (Railpack) |
> |---|---|---|
> | 最小 RAM      | 512 MB                              | **4 GB**（Rust は 8 GB） |
> | 空きディスク  | スタックあたり数 GB                  | **40 GB 以上**（BuildKit キャッシュ） |
> | 初回デプロイ  | 数秒                                | 1〜10 分 |
>
> **VPS の RAM が 4 GB 未満の場合は、代わりに Dockerfile または Docker Image ソースを使ってください。** ホストが 4 GB 未満のとき UI は強い警告を表示します — ビルドはほぼ確実に OOM-kill されます（ビルド自身か別プロセスが終了させられます）。pier-core は毎日 BuildKit キャッシュを ~10 GB / 保持期間 7 日に整理します。完全に無効化するには `PIER_SKIP_RAILPACK=1 bash install.sh` を実行してください。

**Railpack が自動検出する言語**（手動設定不要）:

| 言語 / フレームワーク | 検出元 |
|---|---|
| Node.js / Bun / Deno | `package.json`、`bun.lockb`、`deno.json` |
| Python | `requirements.txt`、`pyproject.toml`、`Pipfile` |
| Go | `go.mod` |
| Rust | `Cargo.toml` |
| PHP | `composer.json` |
| Java | `pom.xml`、`build.gradle` |
| Ruby | `Gemfile` |
| Elixir | `mix.exs` |
| Vite / Astro / CRA 静的サイト | バンドラ設定 + ビルド出力ディレクトリ |

オーバーライドが必要なプロジェクトは、リポジトリ直下に [`railpack.json`](https://railpack.com/configuration/file) を置けば Railpack が自動で読み取ります。

**チューニング項目**（systemd unit または `install.sh` 実行前に設定）:

- `PIER_RAILPACK_MAX_PARALLEL_BUILDS=N` — 並列ビルド上限（デフォルト 1）。UI の `設定 → Auto-build (Railpack)` からも変更できます。
- `PIER_BUILDKIT_MEMORY=4g` — buildkit コンテナの RAM 上限（デフォルト 4g）。
- `PIER_SKIP_RAILPACK=1` — セットアップをスキップ。UI のカードは残りますが、ビルド試行時に「railpack binary not found」と明示されます。

**FAQ**

- **なぜ Nixpacks ではないのですか？** Railpack は活発に開発されている後継（Railway は 2025 年 3 月に移行）で、Nixpacks はメンテナンスモードです。Railpack は BuildKit グラフアプローチにより Node イメージで約 38%、Python イメージで約 77% の縮小を実現しています。
- **ARM / aarch64 で動きますか？** はい — `railpack` と `moby/buildkit` の両方が linux/arm64 バイナリを提供しています。インストールスクリプトが自動で正しいアーキテクチャを選択します。
- **無効にできますか？** はい — `PIER_SKIP_RAILPACK=1 bash install.sh` でセットアップをスキップできます。Dockerfile / Compose / Docker Image ソースは引き続き利用可能です。

## テンプレート

**データベース** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**サービス** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**ゲーム** — Minecraft, Terraria

**VPN** — AmneziaWG

**アプリケーション** — Dockerfile、Docker イメージ、または Docker Compose からデプロイ

> 必要なものが見つかりませんか？任意の Docker イメージや Compose スタックを手動でデプロイできます。

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
