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
- ✨ **Auto-build (Railpack)** — builds zero-config direto do código-fonte para Node, Python, Go, PHP, Java, Ruby, Rust, Vite/Astro/CRA e outras, sem precisar de Dockerfile
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

## Registro npm

**Registro npm privado + proxy, embutido no binário.** Sem container Verdaccio separado, sem banco de dados extra — o Pier serve uma API compatível com npm em `/registry/npm/`, espelha `registry.npmjs.org` de forma transparente e funciona com todos os gerenciadores de pacotes modernos.

### Clientes suportados

| Cliente | Versões | Observações |
|---|---|---|
| **npm** | 7 – 11 | Funciona out-of-the-box |
| **yarn classic** | 1.22.x | Adicionar `always-auth=true` no `.npmrc` |
| **yarn berry** | 2 · 3 · 4 | `.yarnrc.yml` com `npmAlwaysAuth: true` |
| **pnpm** | 9 · 10 | Funciona out-of-the-box |
| **bun** | latest | Funciona out-of-the-box |

### Comandos suportados

| Comando | Status |
|---|---|
| `npm install` / `yarn add` / `pnpm add` / `bun add` | ✓ |
| `npm publish` (scoped + unscoped) | ✓ |
| `npm login` (fluxo CouchDB + `--auth-type=web`) | ✓ |
| `npm dist-tag add / rm / ls` | ✓ |
| `npm deprecate` | ✓ |
| `npm unpublish` (versão única + pacote inteiro) | ✓ |
| `npm whoami` · `npm ping` | ✓ |

### Modo proxy upstream

O Pier pode espelhar de forma transparente o **npmjs.org** (ou qualquer upstream compatível com npm) — toda a equipe usa uma única URL. Packuments são cacheados e revalidados via `If-None-Match`, tarballs são baixados sob demanda no primeiro `install`, e um GC LRU em segundo plano mantém o cache em disco abaixo de um limite configurável. Gerenciamento em **Packages → Upstream proxy**.

- Uma única URL no `.npmrc` para toda a equipe — sem scope routing
- O `install` continua funcionando mesmo se `npmjs.org` cair
- Auditoria: quais pacotes públicos a equipe realmente usa
- Revalidação por TTL com curto-circuito em 304

### Início rápido

```ini
# .npmrc no seu projeto
registry=https://YOUR-PIER-HOST/registry/npm/
//YOUR-PIER-HOST/registry/npm/:_authToken=pier_npm_xxx
always-auth=true
```

Crie o token em **Packages → Manage tokens**, depois:

```bash
npm publish                  # pacote privado
npm install left-pad         # via proxy do npmjs.org + cache
```

Guias completos por cliente: [npm](https://pier.team/docs/registry/clients/npm) · [yarn 1.x](https://pier.team/docs/registry/clients/yarn-classic) · [yarn 2/3/4](https://pier.team/docs/registry/clients/yarn-berry) · [pnpm](https://pier.team/docs/registry/clients/pnpm) · [bun](https://pier.team/docs/registry/clients/bun).

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

### Auto-build (Railpack) — o que é e o que precisa

A fonte **Auto-build** permite implantar a partir de um repositório Git **sem escrever um Dockerfile**. Por baixo dos panos, o Pier delega ao [Railpack](https://github.com/railwayapp/railpack) (o builder open-source da Railway, sucessor do Nixpacks), que se apoia em um daemon local do [moby/buildkit](https://github.com/moby/buildkit). Ambos os componentes são provisionados automaticamente pelo `install.sh`. Guia completo em [from-railpack](https://pier.team/docs/applications/from-railpack).

> ### ⚠ Requisitos do servidor — leia antes de habilitar
>
> O Auto-build é **substancialmente mais pesado** do que os outros caminhos de implantação. Compilar código do usuário no host é fundamentalmente diferente de apenas executar um contêiner pré-construído, então o perfil de recursos muda:
>
> |              | Dockerfile / Compose / Docker Image | Auto-build (Railpack) |
> |---|---|---|
> | RAM mínima   | 512 MB                              | **4 GB** (8 GB para Rust) |
> | Disco livre  | poucos GB por stack                 | **40+ GB** (cache do BuildKit) |
> | Primeiro deploy | segundos                          | 1–10 minutos |
>
> **Se seu VPS tem menos de 4 GB de RAM, use a fonte Dockerfile ou Docker Image em vez desta.** A UI mostra um aviso explícito quando o host tem &lt;4 GB — o build vai quase certamente sofrer OOM-kill (de si mesmo ou de outro processo). O pier-core poda o cache do BuildKit diariamente para ~10 GB / retenção de 7 dias. Você também pode usar `PIER_SKIP_RAILPACK=1 bash install.sh` para pular o provisionamento por completo.

**O que o Railpack detecta automaticamente** (sem configuração manual):

| Linguagem / framework | Detectado a partir de |
|---|---|
| Node.js / Bun / Deno | `package.json`, `bun.lockb`, `deno.json` |
| Python | `requirements.txt`, `pyproject.toml`, `Pipfile` |
| Go | `go.mod` |
| Rust | `Cargo.toml` |
| PHP | `composer.json` |
| Java | `pom.xml`, `build.gradle` |
| Ruby | `Gemfile` |
| Elixir | `mix.exs` |
| Sites estáticos Vite / Astro / CRA | config do bundler + diretório de saída do build |

Para projetos que precisam de overrides, coloque um [`railpack.json`](https://railpack.com/configuration/file) na raiz do repositório — o Railpack carrega automaticamente.

**Ajustes finos** (no unit do systemd ou antes de `install.sh`):

- `PIER_RAILPACK_MAX_PARALLEL_BUILDS=N` — limite de builds paralelos (padrão 1). Também ajustável na UI: `Configurações → Auto-build (Railpack)`.
- `PIER_BUILDKIT_MEMORY=4g` — limite de RAM para o contêiner buildkit (padrão 4g).
- `PIER_SKIP_RAILPACK=1` — pula o provisionamento. O card permanece na UI, mas qualquer tentativa de build retorna a mensagem clara "railpack binary not found".

**FAQ**

- **Por que não Nixpacks?** O Railpack é o sucessor ativo (a Railway migrou em março de 2025); o Nixpacks está em modo de manutenção. Pela abordagem baseada em grafo do BuildKit, o Railpack produz imagens Node ~38% menores e imagens Python ~77% menores.
- **Funciona em ARM/aarch64?** Sim — tanto `railpack` quanto `moby/buildkit` distribuem binários linux/arm64. O install.sh escolhe a arquitetura correta automaticamente.
- **Posso desabilitar?** Sim — `PIER_SKIP_RAILPACK=1 bash install.sh` pula o provisionamento. As fontes Dockerfile / Compose / Docker Image continuam funcionando normalmente.

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
