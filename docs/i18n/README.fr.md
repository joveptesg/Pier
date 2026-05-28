> Traduction de [README.md](../../README.md). En cas de divergence, la version anglaise fait foi.

<p align="center">
  <img src="../logo.svg" alt="Pier" height="120">
</p>

<h3 align="center">Un PaaS léger et auto-hébergé.<br>Un seul binaire. 20 Mo de RAM. Déployez n'importe quoi.</h3>

<p align="center">
  <a href="https://github.com/joveptesg/pier/blob/main/LICENSE"><img src="https://img.shields.io/github/license/joveptesg/pier?color=blue" alt="Licence"></a>
  <a href="https://github.com/joveptesg/pier/stargazers"><img src="https://img.shields.io/github/stars/joveptesg/pier?style=flat" alt="Stars"></a>
  <a href="https://github.com/joveptesg/pier/releases"><img src="https://img.shields.io/github/v/release/joveptesg/pier" alt="Release"></a>
  <img src="https://img.shields.io/badge/rust-1.93%2B-orange" alt="Rust">
</p>

<p align="center">
  <a href="../../README.md">English</a> |
  <a href="README.ru.md">Русский</a> |
  <a href="README.zh-CN.md">中文</a> |
  <a href="README.de.md">Deutsch</a> |
  <a href="README.ja.md">日本語</a> |
  <a href="README.es.md">Español</a> |
  <strong>Français</strong> |
  <a href="README.pt-BR.md">Português</a>
</p>

---

## Qu'est-ce que Pier ?

**Pier est une alternative open source et auto-hébergée à Coolify / Heroku / Vercel — suffisamment légère pour un VPS à 5 $.**

Déployez des conteneurs, des stacks Docker Compose et des dépôts Git avec SSL automatique, reverse proxy et un tableau de bord web moderne — le tout depuis un unique binaire Rust avec seulement **20 à 40 Mo de RAM**.

<!-- 
<p align="center">
  <img src="../../docs/screenshots/dashboard.png" alt="Tableau de bord Pier" width="800">
</p>
-->

## Pourquoi Pier ?

[Coolify](https://coolify.io) est un excellent outil, mais il exécute **6+ conteneurs** et consomme **750 Mo à 1,2 Go de RAM** au repos. Pier offre les mêmes fonctionnalités essentielles dans un seul binaire.

| | Coolify | Pier |
|---|---|---|
| **RAM au repos** | 750 Mo – 1,2 Go | 20–40 Mo (+Traefik) |
| **Disque** | ~1 Go | ~15–30 Mo |
| **Conteneurs en exécution** | 6+ (Laravel, PostgreSQL, Redis, Soketi, Horizon, Traefik) | 1 binaire + Traefik |
| **VPS minimum** | 2 Go RAM, 2 vCPU | 512 Mo RAM, 1 vCPU |
| **Base de données** | PostgreSQL externe | SQLite intégré |
| **Langage** | PHP / Laravel | Rust |
| **JS frontend** | ~300 Ko+ | ~30 Ko (HTMX + Alpine.js) |

## Fonctionnalités

**Conteneurs et stacks**
- 📦 Gestion des conteneurs Docker — créer, démarrer, arrêter, redémarrer, supprimer, journaux, statistiques
- 🐳 Stacks Docker Compose avec éditeur YAML intégré
- 🚀 Déploiement en un clic depuis **30+ modèles**

**Git et déploiements**
- 🔄 Pipeline Git-to-deploy avec webhooks GitHub et GitLab
- 🛠 Construction depuis un Dockerfile, une image Docker ou Compose
- ✨ **Auto-build (Railpack)** — builds zéro-config directement depuis le code source pour Node, Python, Go, PHP, Java, Ruby, Rust, Vite/Astro/CRA et plus, sans Dockerfile
- ⏪ Historique des déploiements avec retour en arrière

**Réseau et SSL**
- 🌐 Reverse proxy via Traefik avec HTTPS automatique
- 🔒 Certificats SSL Let's Encrypt (provisionnés automatiquement)
- 🔗 Domaines personnalisés avec URLs de service générées automatiquement

**Infrastructure**
- 🖥 Gestion multi-serveurs avec agents distants
- 💾 Sauvegardes planifiées avec intégration S3
- 📊 Supervision en temps réel — CPU, RAM, disque, réseau

**Expérience développeur**
- ⚡ Interface web construite avec HTMX + Alpine.js — mode sombre, temps réel, responsive
- 🔑 Authentification JWT avec hachage de mots de passe bcrypt
- 🗃 SQLite intégré — aucune base de données externe requise
- ⚙️ Configuration du serveur en une seule commande

## Registre npm

**Registre npm privé + proxy, intégré directement au binaire.** Sans conteneur Verdaccio séparé, sans base de données supplémentaire — Pier expose une API compatible npm sur `/registry/npm/`, met en miroir transparent `registry.npmjs.org` et fonctionne avec tous les gestionnaires de paquets modernes.

### Clients pris en charge

| Client | Versions | Notes |
|---|---|---|
| **npm** | 7 – 11 | Fonctionne sans configuration |
| **yarn classic** | 1.22.x | Ajouter `always-auth=true` au `.npmrc` |
| **yarn berry** | 2 · 3 · 4 | `.yarnrc.yml` avec `npmAlwaysAuth: true` |
| **pnpm** | 9 · 10 | Fonctionne sans configuration |
| **bun** | latest | Fonctionne sans configuration |

### Commandes prises en charge

| Commande | Statut |
|---|---|
| `npm install` / `yarn add` / `pnpm add` / `bun add` | ✓ |
| `npm publish` (scoped + unscoped) | ✓ |
| `npm login` (flux CouchDB + `--auth-type=web`) | ✓ |
| `npm dist-tag add / rm / ls` | ✓ |
| `npm deprecate` | ✓ |
| `npm unpublish` (version unique + paquet entier) | ✓ |
| `npm whoami` · `npm ping` | ✓ |

### Mode proxy en amont

Pier peut mettre en miroir de manière transparente **npmjs.org** (ou tout autre amont compatible npm) — toute l'équipe utilise une seule URL. Les packuments sont mis en cache et revalidés via `If-None-Match`, les tarballs sont récupérés paresseusement au premier `install`, et un GC LRU en arrière-plan maintient le cache disque sous une limite configurable. Tout se gère dans **Packages → Upstream proxy**.

- Une seule URL dans `.npmrc` pour toute l'équipe — pas de scope routing
- Les `install` continuent de fonctionner même si `npmjs.org` est en panne
- Audit : visibilité des paquets publics réellement utilisés par l'équipe
- Revalidation TTL avec court-circuit 304

### Démarrage rapide

```ini
# .npmrc dans votre projet
registry=https://YOUR-PIER-HOST/registry/npm/
//YOUR-PIER-HOST/registry/npm/:_authToken=pier_npm_xxx
always-auth=true
```

Créez le token dans **Packages → Manage tokens**, puis :

```bash
npm publish                  # paquet privé
npm install left-pad         # proxy depuis npmjs.org + cache
```

Guides complets par client : [npm](https://pier.team/docs/registry/clients/npm) · [yarn 1.x](https://pier.team/docs/registry/clients/yarn-classic) · [yarn 2/3/4](https://pier.team/docs/registry/clients/yarn-berry) · [pnpm](https://pier.team/docs/registry/clients/pnpm) · [bun](https://pier.team/docs/registry/clients/bun).

## Auto-build (Railpack)

La source **Auto-build** vous permet de déployer depuis un dépôt Git **sans écrire de Dockerfile**. Sous le capot, Pier délègue à [Railpack](https://github.com/railwayapp/railpack) (le constructeur open-source de Railway, successeur de Nixpacks), qui s'appuie sur un démon local [moby/buildkit](https://github.com/moby/buildkit). Les deux composants sont provisionnés automatiquement par `install.sh`. Guide complet dans [from-railpack](https://pier.team/docs/applications/from-railpack).

> ### ⚠ Configuration serveur requise — à lire avant l'activation
>
> Auto-build est **nettement plus gourmand** que les autres méthodes de déploiement. Compiler du code utilisateur sur l'hôte est fondamentalement différent du simple lancement d'un conteneur préconstruit, donc le profil de ressources change :
>
> |              | Dockerfile / Compose / Docker Image | Auto-build (Railpack) |
> |---|---|---|
> | RAM minimale | 512 Mo                              | **4 Go** (8 Go pour Rust) |
> | Disque libre | quelques Go par stack               | **40+ Go** (cache BuildKit) |
> | Premier déploiement | secondes                     | 1–10 minutes |
>
> **Si votre VPS dispose de moins de 4 Go de RAM, utilisez plutôt les sources Dockerfile ou Docker Image.** L'UI affiche un avertissement franc lorsque l'hôte est en dessous de 4 Go — le build va presque certainement déclencher un OOM-kill (lui-même ou un autre processus). Pier-core nettoie le cache BuildKit quotidiennement à ~10 Go / rétention 7 jours. Vous pouvez aussi exécuter `PIER_SKIP_RAILPACK=1 bash install.sh` pour sauter complètement l'installation.

**Ce que Railpack détecte automatiquement** (sans config manuelle) :

| Langage / framework | Détecté à partir de |
|---|---|
| Node.js / Bun / Deno | `package.json`, `bun.lockb`, `deno.json` |
| Python | `requirements.txt`, `pyproject.toml`, `Pipfile` |
| Go | `go.mod` |
| Rust | `Cargo.toml` |
| PHP | `composer.json` |
| Java | `pom.xml`, `build.gradle` |
| Ruby | `Gemfile` |
| Elixir | `mix.exs` |
| Sites statiques Vite / Astro / CRA | configuration du bundler + dossier de sortie du build |

Pour les projets qui nécessitent des overrides, déposez un [`railpack.json`](https://railpack.com/configuration/file) à la racine du dépôt — Railpack le prend en compte automatiquement.

**Paramètres de réglage** (à définir dans l'unit systemd ou avant `install.sh`) :

- `PIER_RAILPACK_MAX_PARALLEL_BUILDS=N` — plafond de builds parallèles (défaut 1). Modifiable aussi depuis l'UI : `Paramètres → Auto-build (Railpack)`.
- `PIER_BUILDKIT_MEMORY=4g` — limite RAM du conteneur buildkit (défaut 4g).
- `PIER_SKIP_RAILPACK=1` — saute l'installation. La carte reste visible dans l'UI, mais toute tentative de build renvoie le message clair « railpack binary not found ».

**FAQ**

- **Pourquoi pas Nixpacks ?** Railpack est le successeur actif (Railway a migré en mars 2025) ; Nixpacks est en mode maintenance. Railpack produit des images Node ~38 % plus petites et des images Python ~77 % plus petites grâce à son approche par graphe BuildKit.
- **Fonctionne-t-il sur ARM/aarch64 ?** Oui — `railpack` et `moby/buildkit` proposent tous deux des binaires linux/arm64. Le script d'installation sélectionne automatiquement la bonne architecture.
- **Puis-je le désactiver ?** Oui — `PIER_SKIP_RAILPACK=1 bash install.sh` saute la provisioning. Les sources Dockerfile / Compose / Docker Image continuent de fonctionner normalement.

## Modèles

**Bases de données** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**Services** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**Jeux** — Minecraft, Terraria

**VPN** — AmneziaWG

**Applications** — Déploiement depuis un Dockerfile, une image Docker ou Docker Compose

> Vous ne trouvez pas ce qu'il vous faut ? Déployez n'importe quelle image Docker ou stack Compose manuellement.

## Démarrage rapide

### Option A : Installation en une commande (Ubuntu/Debian)

```bash
curl -fsSL https://pier.team/install | sudo bash
```

### Option B : Compiler depuis les sources

```bash
git clone https://github.com/joveptesg/pier.git
cd pier
cargo build --release
sudo bash scripts/install.sh --binary target/release/pier
```

### Option C : Docker

```bash
docker run -d \
  --name pier \
  -p 8443:8443 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v pier-data:/app/data \
  ghcr.io/joveptesg/pier:latest
```

Ouvrez ensuite `http://IP_DE_VOTRE_SERVEUR:8443/setup` pour créer votre compte administrateur.

> Pour une configuration détaillée du serveur (renforcement de la sécurité, pare-feu, installation de Docker), consultez [INSTALL.md](../../INSTALL.md).

## Stack technique

| Couche | Technologie | Rôle |
|---|---|---|
| Langage | [Rust](https://www.rust-lang.org) | Performance, sécurité, binaire unique |
| HTTP | [Axum](https://github.com/tokio-rs/axum) | API asynchrone + WebSocket |
| Docker | [Bollard](https://github.com/fussybeaver/bollard) | API Docker Engine |
| Base de données | [SQLite](https://github.com/rusqlite/rusqlite) | Persistance intégrée |
| Proxy | [Traefik](https://traefik.io) | Routage automatique + Let's Encrypt |
| Templates | [MiniJinja](https://github.com/mitsuhiko/minijinja) | Rendu côté serveur |
| Frontend | [HTMX](https://htmx.org) + [Alpine.js](https://alpinejs.dev) | JS minimal, temps réel |
| Style | [Tailwind CSS](https://tailwindcss.com) | Mode sombre, responsive |
| Runtime | [Tokio](https://tokio.rs) | E/S asynchrones |
| Stockage | [AWS S3](https://crates.io/crates/aws-sdk-s3) | Stockage des sauvegardes |
| Auth | JWT + bcrypt | Authentification sans état |

## Architecture

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

> Pour une architecture détaillée, consultez [ARCHITECTURE.md](../../ARCHITECTURE.md).

## Feuille de route

- [x] Gestion des conteneurs (API Docker)
- [x] Stacks Docker Compose avec éditeur YAML
- [x] Modèles de services en un clic (30+)
- [x] Reverse proxy + SSL automatique (Traefik + Let's Encrypt)
- [x] Webhooks Git + déploiement automatique (GitHub, GitLab)
- [x] Gestion multi-serveurs avec agents
- [x] Planificateur de sauvegardes avec support S3
- [x] Tableau de bord web (HTMX + Tailwind, mode sombre)
- [x] Gestion des buckets S3
- [x] Visualisation de l'architecture (Canvas)
- [ ] RBAC (contrôle d'accès basé sur les rôles)
- [ ] 2FA (TOTP + WebAuthn)
- [ ] Répartition de charge + mise à l'échelle horizontale
- [ ] Notifications d'alertes (Telegram, Discord, Slack)
- [ ] Mécanisme de mise à jour automatique
- [ ] Isolation réseau Docker par projet
- [ ] Reverse proxy basé sur Pingora (remplacement de Traefik)

## Contribuer

Les contributions sont les bienvenues ! Veuillez lire [CONTRIBUTING.md](../../CONTRIBUTING.md) avant de soumettre une pull request. Tous les contributeurs doivent accepter notre [CLA](../../CLA.md).

```bash
cargo fmt          # Format code
cargo clippy       # Lint
cargo test         # Run tests
cargo build        # Build
```

## Licence

[AGPL-3.0](../../LICENSE)

Pier est libre pour l'auto-hébergement et la modification. Si vous proposez une version modifiée en tant que service réseau, vous devez partager vos modifications sous la même licence.

Pour une licence commerciale (utilisation sans les obligations AGPL), contactez [info@devcom.app](mailto:info@devcom.app).

---

<p align="center">
  <sub>Construit avec 🦀 Rust — rapide, sûr, léger.</sub>
</p>
