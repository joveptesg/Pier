> Tradução de [README.md](../../README.md). Em caso de divergências, consulte a versão em inglês.

<p align="center">
  <img src="../logo.svg" alt="Pier" height="120">
</p>

<h3 align="center">Uma PaaS leve e auto-hospedada.<br>Binário único. 20 MB de RAM. Implante qualquer coisa.</h3>

<p align="center">
  <a href="https://github.com/joveptesg/pier/blob/main/LICENSE"><img src="https://img.shields.io/github/license/joveptesg/pier?color=blue" alt="Licença"></a>
  <a href="https://github.com/joveptesg/pier/stargazers"><img src="https://img.shields.io/github/stars/joveptesg/pier?style=flat" alt="Estrelas"></a>
  <a href="https://github.com/joveptesg/pier/releases"><img src="https://img.shields.io/github/v/release/joveptesg/pier" alt="Versão"></a>
  <img src="https://img.shields.io/badge/rust-1.93%2B-orange" alt="Rust">
</p>

<p align="center">
  <a href="../../README.md">English</a> |
  <a href="README.ru.md">Русский</a> |
  <a href="README.zh-CN.md">中文</a> |
  <a href="README.de.md">Deutsch</a> |
  <a href="README.ja.md">日本語</a> |
  <a href="README.es.md">Español</a> |
  <a href="README.fr.md">Français</a> |
  <strong>Português</strong>
</p>

---

## O que é o Pier?

**Pier é uma alternativa open-source e auto-hospedada ao Coolify / Heroku / Vercel — leve o suficiente para um VPS de $5.**

Implante contêineres, stacks Docker Compose e repositórios Git com SSL automático, proxy reverso e um painel web moderno — tudo a partir de um único binário Rust usando apenas **20–40 MB de RAM**.

<!-- 
<p align="center">
  <img src="docs/screenshots/dashboard.png" alt="Painel do Pier" width="800">
</p>
-->

## Por que o Pier?

O [Coolify](https://coolify.io) é ótimo, mas roda **6+ contêineres** e consome **750 MB – 1,2 GB de RAM** em repouso. O Pier entrega as mesmas funcionalidades essenciais em um único binário.

| | Coolify | Pier |
|---|---|---|
| **RAM em repouso** | 750 MB – 1.2 GB | 20–40 MB (+Traefik) |
| **Disco** | ~1 GB | ~15–30 MB |
| **Contêineres em execução** | 6+ (Laravel, PostgreSQL, Redis, Soketi, Horizon, Traefik) | 1 binário + Traefik |
| **VPS mínimo** | 2 GB RAM, 2 vCPU | 512 MB RAM, 1 vCPU |
| **Banco de dados** | PostgreSQL externo | SQLite embutido |
| **Linguagem** | PHP / Laravel | Rust |
| **JS do frontend** | ~300 KB+ | ~30 KB (HTMX + Alpine.js) |

## Funcionalidades

**Contêineres e Stacks**
- 📦 Gerenciamento de contêineres Docker — criar, iniciar, parar, reiniciar, remover, logs, estatísticas
- 🐳 Stacks Docker Compose com editor YAML integrado
- 🚀 Implantação com um clique a partir de **mais de 30 templates**

**Git e Implantações**
- 🔄 Pipeline Git-to-deploy com webhooks do GitHub e GitLab
- 🛠 Build a partir de Dockerfile, imagem Docker ou Compose
- ⏪ Histórico de implantações com rollback

**Rede e SSL**
- 🌐 Proxy reverso via Traefik com HTTPS automático
- 🔒 Certificados SSL Let's Encrypt (provisionados automaticamente)
- 🔗 Domínios personalizados com URLs de serviço geradas automaticamente

**Infraestrutura**
- 🖥 Gerenciamento multi-servidor com agentes remotos
- 💾 Backups agendados com integração S3
- 📊 Monitoramento em tempo real — CPU, RAM, Disco, Rede

**Experiência do Desenvolvedor**
- ⚡ Interface web construída com HTMX + Alpine.js — modo escuro, tempo real, responsiva
- 🔑 Autenticação JWT com hash de senha bcrypt
- 🗃 SQLite embutido — sem necessidade de banco de dados externo
- ⚙️ Configuração do servidor com um único comando

## Templates

**Bancos de Dados** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**Serviços** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**Jogos** — Minecraft, Terraria

**VPN** — AmneziaWG

**Aplicações** — Implante a partir de Dockerfile, imagem Docker ou Docker Compose

> Não encontrou o que precisa? Implante qualquer imagem Docker ou stack Compose manualmente.

## Início Rápido

### Opção A: Instalação com um comando (Ubuntu/Debian)

```bash
curl -fsSL https://pier.team/install | sudo bash
```

### Opção B: Compilar a partir do código-fonte

```bash
git clone https://github.com/joveptesg/pier.git
cd pier
cargo build --release
sudo bash scripts/install.sh --binary target/release/pier
```

### Opção C: Docker

```bash
docker run -d \
  --name pier \
  -p 8443:8443 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v pier-data:/app/data \
  ghcr.io/joveptesg/pier:latest
```

Em seguida, abra `http://IP_DO_SEU_SERVIDOR:8443/setup` para criar sua conta de administrador.

> Para configuração detalhada do servidor (hardening de segurança, firewall, instalação do Docker), consulte [INSTALL.md](../../INSTALL.md).

## Stack Tecnológica

| Camada | Tecnologia | Finalidade |
|---|---|---|
| Linguagem | [Rust](https://www.rust-lang.org) | Desempenho, segurança, binário único |
| HTTP | [Axum](https://github.com/tokio-rs/axum) | API assíncrona + WebSocket |
| Docker | [Bollard](https://github.com/fussybeaver/bollard) | API do Docker Engine |
| Banco de dados | [SQLite](https://github.com/rusqlite/rusqlite) | Persistência embutida |
| Proxy | [Traefik](https://traefik.io) | Roteamento automático + Let's Encrypt |
| Templates | [MiniJinja](https://github.com/mitsuhiko/minijinja) | Renderização no servidor |
| Frontend | [HTMX](https://htmx.org) + [Alpine.js](https://alpinejs.dev) | JS mínimo, tempo real |
| Estilização | [Tailwind CSS](https://tailwindcss.com) | Modo escuro, responsivo |
| Runtime | [Tokio](https://tokio.rs) | I/O assíncrono |
| Armazenamento | [AWS S3](https://crates.io/crates/aws-sdk-s3) | Armazenamento de backups |
| Autenticação | JWT + bcrypt | Autenticação stateless |

## Arquitetura

```
                    ┌──────────────────────────────────┐
                    │       Pier  (binário único)       │
                    │                                    │
  Navegador ─────►  │  Axum ──► Rotas da API (100+)      │
                    │    │                                │
                    │    ├──► MiniJinja ──► HTML (HTMX)   │
                    │    ├──► Bollard ──► Docker Engine    │
                    │    ├──► rusqlite ──► SQLite          │
                    │    └──► reqwest ──► Agentes Remotos  │
                    └──────────────────────────────────┘
                                    │
                    ┌───────────────┴────────────────┐
                    │     Traefik  (proxy reverso)    │
                    │   Let's Encrypt · Roteamento     │
                    │           automático              │
                    └────────────────────────────────┘
```

> Para arquitetura detalhada, consulte [ARCHITECTURE.md](../../ARCHITECTURE.md).

## Roadmap

- [x] Gerenciamento de contêineres (API Docker)
- [x] Stacks Docker Compose com editor YAML
- [x] Templates de serviço com um clique (30+)
- [x] Proxy reverso + SSL automático (Traefik + Let's Encrypt)
- [x] Webhooks Git + implantação automática (GitHub, GitLab)
- [x] Gerenciamento multi-servidor com agentes
- [x] Agendador de backups com suporte a S3
- [x] Painel web (HTMX + Tailwind, modo escuro)
- [x] Gerenciamento de buckets S3
- [x] Visualização de arquitetura (Canvas)
- [ ] RBAC (controle de acesso baseado em papéis)
- [ ] 2FA (TOTP + WebAuthn)
- [ ] Balanceamento de carga + escalabilidade horizontal
- [ ] Notificações de alerta (Telegram, Discord, Slack)
- [ ] Mecanismo de atualização automática
- [ ] Isolamento de rede Docker por projeto
- [ ] Proxy reverso baseado em Pingora (substituir Traefik)

## Contribuindo

Contribuições são bem-vindas! Por favor, leia [CONTRIBUTING.md](../../CONTRIBUTING.md) antes de enviar um pull request. Todos os contribuidores devem concordar com nosso [CLA](../../CLA.md).

```bash
cargo fmt          # Formatar código
cargo clippy       # Lint
cargo test         # Executar testes
cargo build        # Compilar
```

## Licença

[AGPL-3.0](../../LICENSE)

O Pier é gratuito para auto-hospedagem e modificação. Se você oferece uma versão modificada como serviço de rede, deve compartilhar suas modificações sob a mesma licença.

Para licenciamento comercial (uso sem obrigações AGPL), entre em contato com [info@devcom.app](mailto:info@devcom.app).

---

<p align="center">
  <sub>Feito com 🦀 Rust — rápido, seguro, leve.</sub>
</p>
