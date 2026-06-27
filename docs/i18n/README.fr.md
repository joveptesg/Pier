> Traduction de [README.md](../../README.md). En cas de divergence, la version anglaise fait foi.

<p align="center">
  <img src="../logo.svg" alt="Pier" height="120">
</p>

<h3 align="center">Un PaaS léger et auto-hébergé.<br>Un seul binaire. 20 Mo de RAM. Déployez n'importe quoi.</h3>

<p align="center">
  <a href="https://github.com/joveptesg/pier/blob/main/LICENSE"><img src="https://img.shields.io/github/license/joveptesg/pier?color=blue" alt="Licence"></a>
  <a href="https://github.com/joveptesg/pier/stargazers"><img src="https://img.shields.io/github/stars/joveptesg/pier?style=flat" alt="Stars"></a>
  <a href="https://github.com/joveptesg/pier/releases"><img src="https://img.shields.io/github/v/release/joveptesg/pier?include_prereleases" alt="Release"></a>
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

## Démarrage rapide

### Option A : Installation en une commande (Ubuntu/Debian)

```bash
curl -fsSL https://pier.team/install | sudo bash
```

> L'URL courte redirige vers [`scripts/bootstrap.sh`](../../scripts/bootstrap.sh). Le script installe Docker, télécharge le binaire de la dernière version (avec vérification sha256) et exécute `install.sh`. Relancez-le à tout moment pour passer à la dernière version.

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

### Option D : Installation depuis une version préconstruite (sans compilation)

Vous avez déjà Docker ? Récupérez directement le dernier binaire préconstruit — pas de toolchain Rust, pas de compilation :

```bash
# 1. Télécharger le binaire préconstruit + la somme de contrôle (linux/amd64)
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64 -o pier-linux-amd64
curl -fL https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64.sha256 -o pier-linux-amd64.sha256
sha256sum -c pier-linux-amd64.sha256          # vérifier l'intégrité

# 2. Récupérer l'installeur et l'exécuter
curl -fL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/install.sh -o install.sh
chmod +x pier-linux-amd64
sudo bash install.sh --binary ./pier-linux-amd64
```

> L'équivalent manuel de l'Option A, sans l'installation automatique de Docker. Nécessite que Docker + Compose soient déjà présents (voir [INSTALL.md](../../INSTALL.md)). Le nom de fichier du binaire doit rester `pier-linux-amd64` pour que `sha256sum -c` corresponde.

### Mettre à jour Pier

Les mises à jour récupèrent un nouveau **binaire préconstruit** — aucune recompilation depuis les sources n'est nécessaire. `install.sh` détecte le service en cours d'exécution, l'arrête, remplace le binaire et le redémarre, en préservant votre `.env` et `/opt/pier/data`.

```bash
# Le plus simple — relancer l'installeur en une commande (re-télécharge la dernière version) :
curl -fsSL https://pier.team/install | sudo bash

# Ou manuellement, selon le même flux que l'Option D (télécharger → vérifier → install.sh).
```

Ouvrez ensuite `http://IP_DE_VOTRE_SERVEUR:8443/setup` pour créer votre compte administrateur.

> Pour une configuration détaillée du serveur (renforcement de la sécurité, pare-feu, installation de Docker), consultez [INSTALL.md](../../INSTALL.md).

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
- 🗄 **Éditeur de données** intégré — parcourez les tables/collections et exécutez des requêtes SQL/Mongo/Redis depuis le tableau de bord (PostgreSQL, MySQL/MariaDB, MongoDB, Redis)

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

## Éditeur de données

**Parcourez et interrogez vos bases de données depuis le tableau de bord — sans Adminer, sans pgweb, sans client externe.** Chaque service de base de données obtient un onglet **Data** : explorez le schéma, parcourez les lignes et exécutez des requêtes en ligne. Intégré au binaire, protégé par RBAC, et chaque requête est auditée.

### Moteurs pris en charge

| Moteur | Pilote | Explorer | Exécuteur de requêtes |
|---|---|---|---|
| **PostgreSQL** (incl. PostGIS, TimescaleDB) | `sqlx` natif | schémas · tables · vues · structure · lignes | SQL arbitraire |
| **MySQL / MariaDB** | `sqlx` natif | bases · tables · vues · structure · lignes | SQL arbitraire |
| **MongoDB** | `mongosh` (docker-exec) | bases · collections · documents | scripts `mongosh` |
| **Redis / Valkey** | `redis` natif | clés (SCAN) · valeurs selon le type · TTL | commandes brutes |

### Explorer

- **SQL** — arborescence schémas/tables, structure de chaque table (colonnes, types, nullabilité, valeurs par défaut, clés primaires, index) et lignes paginées avec un total.
- **MongoDB** — arborescence « base → collection », documents paginés rendus en EJSON.
- **Redis** — explorateur de clés basé sur `SCAN` avec le type de chaque clé, une vue de la valeur selon le type (string / list / set / zset / hash / stream) et le TTL ; basculez entre les bases 0–15.

### Requêtes

- **SQL Runner** — exécutez n'importe quelle instruction sur PostgreSQL ou MySQL/MariaDB. Les lectures renvoient une grille (limitée à 1 000 lignes) ; les écritures indiquent le nombre de lignes affectées. Un délai d'expiration de 15 secondes par instruction empêche une requête incontrôlée de saturer la base.
- **Mongo Shell** — exécutez n'importe quel script `mongosh` sur la base sélectionnée.
- **Commandes Redis** — exécutez n'importe quelle commande (`GET`, `HGETALL`, `TTL`, …) et lisez la réponse en JSON.

### Accès et audit

- La **lecture** (exploration, structure, lignes) nécessite `Viewer` ; l'**écriture** (tout exécuteur) nécessite `Editor` — appliqué par ressource via le RBAC de Pier.
- Chaque exécution d'un exécuteur est enregistrée dans la table d'audit `db_query_log` — qui a exécuté quoi, contre quelle base, avec le statut, le nombre de lignes et la durée.
- Les connexions utilisent des identifiants déchiffrés depuis l'environnement chiffré du service. Les bases privées sont jointes via le réseau Docker `pier-net`, donc aucun port n'a besoin d'être publié sur l'hôte.

## Modèles

**Bases de données** — PostgreSQL, MySQL, MariaDB, MongoDB, Redis, Valkey, ClickHouse, Cassandra, ScyllaDB

**Services** — Grafana, Gitea, Forgejo, Matrix Synapse, Elasticsearch, Kibana, RabbitMQ, Directus, Supabase, NocoDB, Portainer, Gotify, Audiobookshelf, Qdrant, Beszel

**Jeux** — Minecraft, Terraria

**VPN** — AmneziaWG

**Applications** — Déploiement depuis un Dockerfile, une image Docker ou Docker Compose

> Vous ne trouvez pas ce qu'il vous faut ? Déployez n'importe quelle image Docker ou stack Compose manuellement.

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

## Contribuer

Les contributions sont les bienvenues ! Veuillez lire [CONTRIBUTING.md](../../CONTRIBUTING.md) avant de soumettre une pull request. Tous les contributeurs doivent accepter notre [CLA](../../CLA.md).

Avant de soumettre une pull request, exécutez :

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

## Historique des étoiles

<p align="center">
  <a href="https://star-history.com/#joveptesg/pier&Date">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=joveptesg/pier&type=Date&theme=dark" />
      <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=joveptesg/pier&type=Date" />
      <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=joveptesg/pier&type=Date" width="720" />
    </picture>
  </a>
</p>

---

<p align="center">
  <sub>Construit avec 🦀 Rust — rapide, sûr, léger.</sub>
</p>
