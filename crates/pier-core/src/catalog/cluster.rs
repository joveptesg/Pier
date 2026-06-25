use std::collections::HashMap;

use anyhow::{bail, Result};

/// Node role within a cluster.
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClusterNode {
    pub index: usize,
    pub role: String,
    pub server_id: String,
    pub container_name: String,
}

/// Cluster configuration stored in services.cluster_config_json.
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClusterState {
    pub node_count: usize,
    pub nodes: Vec<ClusterNode>,
}

/// Build docker-compose YAML for a database cluster running on a single server.
/// For multi-server clusters, call this per server with only that server's nodes.
pub fn build_cluster_compose(
    catalog_id: &str,
    node_count: usize,
    vars: &HashMap<String, String>,
) -> Result<String> {
    match catalog_id {
        "postgresql" => build_postgresql(node_count, vars),
        "mysql" => build_mysql(node_count, vars),
        "mariadb" => build_mariadb(node_count, vars),
        "mongodb" => build_mongodb(node_count, vars),
        "cassandra" => build_cassandra(node_count, vars),
        "scylladb" => build_scylladb(node_count, vars),
        "redis" => build_redis(node_count, vars),
        _ => bail!("Cluster mode not supported for {catalog_id}"),
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL — Bitnami streaming replication
// ---------------------------------------------------------------------------

fn build_postgresql(nodes: usize, vars: &HashMap<String, String>) -> Result<String> {
    let name = vars.get("name").map(|s| s.as_str()).unwrap_or("pg");
    let version = vars.get("version").map(|s| s.as_str()).unwrap_or("17");
    let password = vars
        .get("password")
        .map(|s| s.as_str())
        .unwrap_or("changeme");
    let db_name = vars
        .get("POSTGRES_DB")
        .map(|s| s.as_str())
        .unwrap_or("postgres");
    let port = vars.get("port").map(|s| s.as_str()).unwrap_or("5432");
    let repl_password = vars
        .get("repl_password")
        .map(|s| s.as_str())
        .unwrap_or(password);

    let mut yaml = String::from("services:\n");

    // Primary
    yaml.push_str(&format!(
        r#"  postgresql-primary:
    image: bitnami/postgresql:{version}
    container_name: pier-{name}-primary
    ports:
      - "{port}:5432"
    environment:
      POSTGRESQL_REPLICATION_MODE: master
      POSTGRESQL_REPLICATION_USER: repl_user
      POSTGRESQL_REPLICATION_PASSWORD: "{repl_password}"
      POSTGRESQL_USERNAME: postgres
      POSTGRESQL_PASSWORD: "{password}"
      POSTGRESQL_DATABASE: "{db_name}"
    volumes:
      - primary_data:/bitnami/postgresql
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U postgres"]
      interval: 10s
      timeout: 5s
      retries: 5
      start_period: 30s
    restart: unless-stopped
    labels:
      pier.cluster.role: primary
"#
    ));

    // Replicas
    for i in 1..nodes {
        yaml.push_str(&format!(
            r#"  postgresql-replica-{i}:
    image: bitnami/postgresql:{version}
    container_name: pier-{name}-replica-{i}
    environment:
      POSTGRESQL_REPLICATION_MODE: slave
      POSTGRESQL_REPLICATION_USER: repl_user
      POSTGRESQL_REPLICATION_PASSWORD: "{repl_password}"
      POSTGRESQL_MASTER_HOST: postgresql-primary
      POSTGRESQL_MASTER_PORT_NUMBER: 5432
      POSTGRESQL_PASSWORD: "{password}"
    volumes:
      - replica{i}_data:/bitnami/postgresql
    depends_on:
      postgresql-primary:
        condition: service_healthy
    restart: unless-stopped
    labels:
      pier.cluster.role: replica
"#
        ));
    }

    // Volumes
    yaml.push_str("volumes:\n  primary_data:\n");
    for i in 1..nodes {
        yaml.push_str(&format!("  replica{i}_data:\n"));
    }

    Ok(yaml)
}

// ---------------------------------------------------------------------------
// MySQL — Bitnami streaming replication
// ---------------------------------------------------------------------------

fn build_mysql(nodes: usize, vars: &HashMap<String, String>) -> Result<String> {
    let name = vars.get("name").map(|s| s.as_str()).unwrap_or("mysql");
    let version = vars.get("version").map(|s| s.as_str()).unwrap_or("9.3");
    let password = vars
        .get("password")
        .map(|s| s.as_str())
        .unwrap_or("changeme");
    let db_name = vars
        .get("MYSQL_DATABASE")
        .map(|s| s.as_str())
        .unwrap_or("mydb");
    let port = vars.get("port").map(|s| s.as_str()).unwrap_or("3306");
    let repl_password = vars
        .get("repl_password")
        .map(|s| s.as_str())
        .unwrap_or(password);

    let mut yaml = String::from("services:\n");

    yaml.push_str(&format!(
        r#"  mysql-primary:
    image: bitnami/mysql:{version}
    container_name: pier-{name}-primary
    ports:
      - "{port}:3306"
    environment:
      MYSQL_REPLICATION_MODE: master
      MYSQL_REPLICATION_USER: repl_user
      MYSQL_REPLICATION_PASSWORD: "{repl_password}"
      MYSQL_ROOT_PASSWORD: "{password}"
      MYSQL_DATABASE: "{db_name}"
    volumes:
      - primary_data:/bitnami/mysql/data
    healthcheck:
      test: ["CMD", "mysqladmin", "ping", "-h", "localhost", "-uroot", "-p{password}"]
      interval: 10s
      timeout: 5s
      retries: 5
      start_period: 30s
    restart: unless-stopped
    labels:
      pier.cluster.role: primary
"#
    ));

    for i in 1..nodes {
        yaml.push_str(&format!(
            r#"  mysql-replica-{i}:
    image: bitnami/mysql:{version}
    container_name: pier-{name}-replica-{i}
    environment:
      MYSQL_REPLICATION_MODE: slave
      MYSQL_MASTER_HOST: mysql-primary
      MYSQL_MASTER_ROOT_PASSWORD: "{password}"
      MYSQL_REPLICATION_USER: repl_user
      MYSQL_REPLICATION_PASSWORD: "{repl_password}"
    volumes:
      - replica{i}_data:/bitnami/mysql/data
    depends_on:
      mysql-primary:
        condition: service_healthy
    restart: unless-stopped
    labels:
      pier.cluster.role: replica
"#
        ));
    }

    yaml.push_str("volumes:\n  primary_data:\n");
    for i in 1..nodes {
        yaml.push_str(&format!("  replica{i}_data:\n"));
    }

    Ok(yaml)
}

// ---------------------------------------------------------------------------
// MariaDB — Bitnami streaming replication
// ---------------------------------------------------------------------------

fn build_mariadb(nodes: usize, vars: &HashMap<String, String>) -> Result<String> {
    let name = vars.get("name").map(|s| s.as_str()).unwrap_or("mariadb");
    let version = vars.get("version").map(|s| s.as_str()).unwrap_or("11.8");
    let password = vars
        .get("password")
        .map(|s| s.as_str())
        .unwrap_or("changeme");
    let db_name = vars
        .get("MARIADB_DATABASE")
        .map(|s| s.as_str())
        .unwrap_or("mydb");
    let port = vars.get("port").map(|s| s.as_str()).unwrap_or("3306");
    let repl_password = vars
        .get("repl_password")
        .map(|s| s.as_str())
        .unwrap_or(password);

    let mut yaml = String::from("services:\n");

    yaml.push_str(&format!(
        r#"  mariadb-primary:
    image: bitnami/mariadb:{version}
    container_name: pier-{name}-primary
    ports:
      - "{port}:3306"
    environment:
      MARIADB_REPLICATION_MODE: master
      MARIADB_REPLICATION_USER: repl_user
      MARIADB_REPLICATION_PASSWORD: "{repl_password}"
      MARIADB_ROOT_PASSWORD: "{password}"
      MARIADB_DATABASE: "{db_name}"
    volumes:
      - primary_data:/bitnami/mariadb/data
    healthcheck:
      test: ["CMD", "healthcheck.sh", "--connect", "--innodb_initialized"]
      interval: 10s
      timeout: 5s
      retries: 5
      start_period: 30s
    restart: unless-stopped
    labels:
      pier.cluster.role: primary
"#
    ));

    for i in 1..nodes {
        yaml.push_str(&format!(
            r#"  mariadb-replica-{i}:
    image: bitnami/mariadb:{version}
    container_name: pier-{name}-replica-{i}
    environment:
      MARIADB_REPLICATION_MODE: slave
      MARIADB_MASTER_HOST: mariadb-primary
      MARIADB_MASTER_ROOT_PASSWORD: "{password}"
      MARIADB_REPLICATION_USER: repl_user
      MARIADB_REPLICATION_PASSWORD: "{repl_password}"
    volumes:
      - replica{i}_data:/bitnami/mariadb/data
    depends_on:
      mariadb-primary:
        condition: service_healthy
    restart: unless-stopped
    labels:
      pier.cluster.role: replica
"#
        ));
    }

    yaml.push_str("volumes:\n  primary_data:\n");
    for i in 1..nodes {
        yaml.push_str(&format!("  replica{i}_data:\n"));
    }

    Ok(yaml)
}

// ---------------------------------------------------------------------------
// MongoDB — Replica Set with healthcheck-based rs.initiate()
// ---------------------------------------------------------------------------

fn build_mongodb(nodes: usize, vars: &HashMap<String, String>) -> Result<String> {
    let name = vars.get("name").map(|s| s.as_str()).unwrap_or("mongo");
    let version = vars.get("version").map(|s| s.as_str()).unwrap_or("8.0");
    let port = vars.get("port").map(|s| s.as_str()).unwrap_or("27017");

    let mut yaml = String::from("services:\n");

    // Build rs.initiate() members list
    let members: Vec<String> = (0..nodes)
        .map(|i| {
            format!(
                "{{_id:{i}, host:'mongo-{}:27017'{}}}",
                i + 1,
                if i == 0 {
                    ", priority:2"
                } else {
                    ", priority:1"
                }
            )
        })
        .collect();
    let members_str = members.join(",");

    // First node with rs.initiate healthcheck
    yaml.push_str(&format!(
        r#"  mongo-1:
    image: mongo:{version}
    container_name: pier-{name}-node-1
    command: ["mongod", "--replSet", "rs0", "--bind_ip_all"]
    ports:
      - "{port}:27017"
    volumes:
      - node1_data:/data/db
    healthcheck:
      test: ["CMD-SHELL", "mongosh --quiet --eval \"try {{ var s = rs.status(); if(s.ok) quit(0); }} catch(e) {{ rs.initiate({{_id:'rs0', members:[{members_str}]}}); }} quit(1);\""]
      interval: 10s
      timeout: 10s
      retries: 10
      start_period: 15s
    restart: unless-stopped
    labels:
      pier.cluster.role: primary
"#
    ));

    // Remaining nodes
    for i in 2..=nodes {
        yaml.push_str(&format!(
            r#"  mongo-{i}:
    image: mongo:{version}
    container_name: pier-{name}-node-{i}
    command: ["mongod", "--replSet", "rs0", "--bind_ip_all"]
    volumes:
      - node{i}_data:/data/db
    restart: unless-stopped
    labels:
      pier.cluster.role: secondary
"#
        ));
    }

    yaml.push_str("volumes:\n");
    for i in 1..=nodes {
        yaml.push_str(&format!("  node{i}_data:\n"));
    }

    Ok(yaml)
}

// ---------------------------------------------------------------------------
// Cassandra — Seeds-based gossip cluster
// ---------------------------------------------------------------------------

fn build_cassandra(nodes: usize, vars: &HashMap<String, String>) -> Result<String> {
    let name = vars.get("name").map(|s| s.as_str()).unwrap_or("cassandra");
    let version = vars.get("version").map(|s| s.as_str()).unwrap_or("5.0");
    let port = vars.get("port").map(|s| s.as_str()).unwrap_or("9042");
    let default_cluster = format!("pier-{name}");
    let cluster_name = vars
        .get("CASSANDRA_CLUSTER_NAME")
        .map(|s| s.as_str())
        .unwrap_or(&default_cluster);

    // Seeds: first 2 nodes (or 1 if only 2 total)
    let seed_count = std::cmp::min(2, nodes);
    let seeds: Vec<String> = (1..=seed_count).map(|i| format!("cassandra-{i}")).collect();
    let seeds_str = seeds.join(",");

    let mut yaml = String::from("services:\n");

    for i in 1..=nodes {
        let dep = if i > 1 {
            format!(
                "    depends_on:\n      cassandra-{}:\n        condition: service_healthy\n",
                i - 1
            )
        } else {
            String::new()
        };
        let port_mapping = if i == 1 {
            format!("    ports:\n      - \"{port}:9042\"\n")
        } else {
            String::new()
        };

        yaml.push_str(&format!(
            r#"  cassandra-{i}:
    image: cassandra:{version}
    container_name: pier-{name}-node-{i}
{port_mapping}    environment:
      CASSANDRA_SEEDS: "{seeds_str}"
      CASSANDRA_CLUSTER_NAME: "{cluster_name}"
      CASSANDRA_DC: dc1
      CASSANDRA_ENDPOINT_SNITCH: GossipingPropertyFileSnitch
      MAX_HEAP_SIZE: 512M
      HEAP_NEWSIZE: 100M
    volumes:
      - node{i}_data:/var/lib/cassandra
    healthcheck:
      test: ["CMD-SHELL", "nodetool status | grep -q 'UN'"]
      interval: 30s
      timeout: 10s
      retries: 10
      start_period: 90s
{dep}    restart: unless-stopped
    labels:
      pier.cluster.role: node
"#
        ));
    }

    yaml.push_str("volumes:\n");
    for i in 1..=nodes {
        yaml.push_str(&format!("  node{i}_data:\n"));
    }

    Ok(yaml)
}

// ---------------------------------------------------------------------------
// ScyllaDB — Seeds + --smp/--memory flags
// ---------------------------------------------------------------------------

fn build_scylladb(nodes: usize, vars: &HashMap<String, String>) -> Result<String> {
    let name = vars.get("name").map(|s| s.as_str()).unwrap_or("scylla");
    let version = vars.get("version").map(|s| s.as_str()).unwrap_or("6.2");
    let port = vars.get("port").map(|s| s.as_str()).unwrap_or("9042");
    let smp = vars.get("smp").map(|s| s.as_str()).unwrap_or("1");
    let memory = vars.get("memory").map(|s| s.as_str()).unwrap_or("750M");

    let seed_count = std::cmp::min(2, nodes);
    let seeds: Vec<String> = (1..=seed_count).map(|i| format!("scylla-{i}")).collect();
    let seeds_str = seeds.join(",");

    let mut yaml = String::from("services:\n");

    for i in 1..=nodes {
        let dep = if i > 1 {
            format!(
                "    depends_on:\n      scylla-{}:\n        condition: service_healthy\n",
                i - 1
            )
        } else {
            String::new()
        };
        let port_mapping = if i == 1 {
            format!("    ports:\n      - \"{port}:9042\"\n")
        } else {
            String::new()
        };

        yaml.push_str(&format!(
            r#"  scylla-{i}:
    image: scylladb/scylla:{version}
    container_name: pier-{name}-node-{i}
    command: --seeds={seeds_str} --smp {smp} --memory {memory} --overprovisioned 1 --api-address 0.0.0.0
{port_mapping}    volumes:
      - node{i}_data:/var/lib/scylla
    healthcheck:
      test: ["CMD-SHELL", "nodetool status | grep -E '^UN'"]
      interval: 15s
      timeout: 10s
      retries: 10
      start_period: 30s
{dep}    restart: unless-stopped
    labels:
      pier.cluster.role: node
"#
        ));
    }

    yaml.push_str("volumes:\n");
    for i in 1..=nodes {
        yaml.push_str(&format!("  node{i}_data:\n"));
    }

    Ok(yaml)
}

// ---------------------------------------------------------------------------
// Redis — Bitnami Sentinel (master + replicas + sentinels)
// ---------------------------------------------------------------------------

fn build_redis(nodes: usize, vars: &HashMap<String, String>) -> Result<String> {
    let name = vars.get("name").map(|s| s.as_str()).unwrap_or("redis");
    let version = vars.get("version").map(|s| s.as_str()).unwrap_or("8.0");
    let password = vars
        .get("password")
        .map(|s| s.as_str())
        .unwrap_or("changeme");
    let port = vars.get("port").map(|s| s.as_str()).unwrap_or("6379");

    let replica_count = nodes - 1; // 1 master + (nodes-1) replicas
    let sentinel_count = nodes; // same number of sentinels as total nodes
    let quorum = (sentinel_count / 2) + 1;

    let mut yaml = String::from("services:\n");

    // Master
    yaml.push_str(&format!(
        r#"  redis-master:
    image: bitnami/redis:{version}
    container_name: pier-{name}-master
    ports:
      - "{port}:6379"
    environment:
      REDIS_REPLICATION_MODE: master
      REDIS_PASSWORD: "{password}"
    volumes:
      - master_data:/bitnami/redis/data
    healthcheck:
      test: ["CMD", "redis-cli", "-a", "{password}", "ping"]
      interval: 10s
      timeout: 5s
      retries: 5
    restart: unless-stopped
    labels:
      pier.cluster.role: master
"#
    ));

    // Replicas
    for i in 1..=replica_count {
        yaml.push_str(&format!(
            r#"  redis-replica-{i}:
    image: bitnami/redis:{version}
    container_name: pier-{name}-replica-{i}
    environment:
      REDIS_REPLICATION_MODE: slave
      REDIS_MASTER_HOST: redis-master
      REDIS_MASTER_PASSWORD: "{password}"
      REDIS_PASSWORD: "{password}"
    volumes:
      - replica{i}_data:/bitnami/redis/data
    depends_on:
      redis-master:
        condition: service_healthy
    restart: unless-stopped
    labels:
      pier.cluster.role: replica
"#
        ));
    }

    // Sentinels
    for i in 1..=sentinel_count {
        yaml.push_str(&format!(
            r#"  redis-sentinel-{i}:
    image: bitnami/redis-sentinel:{version}
    container_name: pier-{name}-sentinel-{i}
    environment:
      REDIS_MASTER_SET: "pier-{name}"
      REDIS_MASTER_HOST: redis-master
      REDIS_MASTER_PASSWORD: "{password}"
      REDIS_SENTINEL_DOWN_AFTER_MILLISECONDS: "10000"
      REDIS_SENTINEL_QUORUM: "{quorum}"
    depends_on:
      redis-master:
        condition: service_healthy
    restart: unless-stopped
    labels:
      pier.cluster.role: sentinel
"#
        ));
    }

    yaml.push_str("volumes:\n  master_data:\n");
    for i in 1..=replica_count {
        yaml.push_str(&format!("  replica{i}_data:\n"));
    }

    Ok(yaml)
}

// ===========================================================================
// Cross-server (mesh-distributed) cluster generation
//
// Each node runs on its assigned server; nodes co-located with the primary
// use container DNS, nodes on OTHER servers reach the primary at its mesh
// address + published port (the primary publishes its DB port on 0.0.0.0).
// Mesh `extra_hosts` are injected by the deploy path; we use the mesh IP
// directly so no DNS resolution is required across servers.
// ===========================================================================

/// A planned cluster node with its server placement and mesh-reachable address.
#[derive(Debug, Clone)]
pub struct ClusterNodePlan {
    pub index: usize,
    pub role: String,
    pub server_id: String,
    /// Mesh IP of this node's server (e.g. "10.42.0.2") — how peers on OTHER
    /// servers reach this node's published DB port.
    pub mesh_addr: String,
    /// Host port this node publishes its DB port on (for cross-server access).
    pub host_port: u16,
}

/// Catalog IDs whose cluster mode supports CROSS-SERVER distribution today.
/// Streaming primary→replica only needs the replica to reach the primary, so
/// the addressing is simple. Quorum/replica-set/seed DBs (mongodb, cassandra,
/// scylladb, redis) need every-node-to-every-node addressing — a follow-up.
pub fn cross_server_cluster_supported(catalog_id: &str) -> bool {
    matches!(
        catalog_id,
        "postgresql" | "mysql" | "mariadb" | "mongodb" | "redis" | "cassandra" | "scylladb"
    )
}

/// Build the docker-compose for the nodes assigned to `target_server_id`,
/// wiring cross-server replicas to the primary's mesh address + published port.
pub fn build_cluster_compose_for_server(
    catalog_id: &str,
    target_server_id: &str,
    nodes: &[ClusterNodePlan],
    vars: &HashMap<String, String>,
) -> Result<String> {
    match catalog_id {
        "postgresql" => build_repl_distributed(target_server_id, nodes, vars, &PG_CFG),
        "mysql" => build_repl_distributed(target_server_id, nodes, vars, &MYSQL_CFG),
        "mariadb" => build_repl_distributed(target_server_id, nodes, vars, &MARIADB_CFG),
        "mongodb" => build_mongodb_distributed(target_server_id, nodes, vars),
        "redis" => build_redis_distributed(target_server_id, nodes, vars),
        "cassandra" => build_cassandra_distributed(target_server_id, nodes, vars),
        "scylladb" => build_scylladb_distributed(target_server_id, nodes, vars),
        _ => bail!("cross-server cluster distribution is not supported for {catalog_id} yet"),
    }
}

/// Reject co-located nodes for gossip databases. Cassandra/Scylla peers find
/// each other via seeds at a UNIFORM `storage_port` (7000), so two nodes on one
/// host would collide on that fixed port. Require one node per server.
fn ensure_one_node_per_server(nodes: &[ClusterNodePlan], kind: &str) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for n in nodes {
        if !seen.insert(n.server_id.as_str()) {
            bail!(
                "{kind} cross-server clusters require one node per server (uniform gossip \
                 port 7000); assign each node to a distinct server"
            );
        }
    }
    Ok(())
}

/// Cassandra gossip cluster across servers (official image). Each node lives on
/// its own server with fixed ports 7000 (storage/gossip) + 9042 (CQL) and
/// advertises its MESH IP via `CASSANDRA_BROADCAST_ADDRESS`, so cross-host
/// gossip rides the mesh. Seeds = the first 1–2 nodes' mesh IPs.
fn build_cassandra_distributed(
    target: &str,
    nodes: &[ClusterNodePlan],
    vars: &HashMap<String, String>,
) -> Result<String> {
    ensure_one_node_per_server(nodes, "cassandra")?;
    let name = vars.get("name").map(|s| s.as_str()).unwrap_or("cassandra");
    let version = vars.get("version").map(|s| s.as_str()).unwrap_or("5.0");
    let default_cluster = format!("pier-{name}");
    let cluster_name = vars
        .get("CASSANDRA_CLUSTER_NAME")
        .map(|s| s.as_str())
        .unwrap_or(&default_cluster);

    let seed_count = std::cmp::min(2, nodes.len());
    let seeds_str = nodes
        .iter()
        .take(seed_count)
        .map(|n| n.mesh_addr.clone())
        .collect::<Vec<_>>()
        .join(",");

    let mut yaml = String::from("services:\n");
    let mut vols: Vec<String> = Vec::new();
    for n in nodes.iter().filter(|n| n.server_id == target) {
        let num = n.index + 1;
        let mesh = &n.mesh_addr;
        yaml.push_str(&format!(
            r#"  cassandra-{num}:
    image: cassandra:{version}
    container_name: pier-{name}-node-{num}
    ports:
      - "0.0.0.0:7000:7000"
      - "0.0.0.0:9042:9042"
    environment:
      CASSANDRA_SEEDS: "{seeds_str}"
      CASSANDRA_CLUSTER_NAME: "{cluster_name}"
      CASSANDRA_BROADCAST_ADDRESS: "{mesh}"
      CASSANDRA_BROADCAST_RPC_ADDRESS: "{mesh}"
      CASSANDRA_DC: dc1
      CASSANDRA_ENDPOINT_SNITCH: GossipingPropertyFileSnitch
      MAX_HEAP_SIZE: 512M
      HEAP_NEWSIZE: 100M
    volumes:
      - node{num}_data:/var/lib/cassandra
    healthcheck:
      test: ["CMD-SHELL", "nodetool status | grep -q 'UN'"]
      interval: 30s
      timeout: 10s
      retries: 10
      start_period: 90s
    restart: unless-stopped
    labels:
      pier.cluster.role: node
"#
        ));
        vols.push(format!("node{num}_data"));
    }
    if !vols.is_empty() {
        yaml.push_str("volumes:\n");
        for v in &vols {
            yaml.push_str(&format!("  {v}:\n"));
        }
    }
    Ok(yaml)
}

/// ScyllaDB gossip cluster across servers. Same one-per-server + fixed-port
/// (7000/9042) model as Cassandra, but Scylla takes its addresses as CLI flags:
/// `--broadcast-address` / `--broadcast-rpc-address` = the node's mesh IP.
fn build_scylladb_distributed(
    target: &str,
    nodes: &[ClusterNodePlan],
    vars: &HashMap<String, String>,
) -> Result<String> {
    ensure_one_node_per_server(nodes, "scylladb")?;
    let name = vars.get("name").map(|s| s.as_str()).unwrap_or("scylla");
    let version = vars.get("version").map(|s| s.as_str()).unwrap_or("6.2");
    let smp = vars.get("smp").map(|s| s.as_str()).unwrap_or("1");
    let memory = vars.get("memory").map(|s| s.as_str()).unwrap_or("750M");

    let seed_count = std::cmp::min(2, nodes.len());
    let seeds_str = nodes
        .iter()
        .take(seed_count)
        .map(|n| n.mesh_addr.clone())
        .collect::<Vec<_>>()
        .join(",");

    let mut yaml = String::from("services:\n");
    let mut vols: Vec<String> = Vec::new();
    for n in nodes.iter().filter(|n| n.server_id == target) {
        let num = n.index + 1;
        let mesh = &n.mesh_addr;
        yaml.push_str(&format!(
            r#"  scylla-{num}:
    image: scylladb/scylla:{version}
    container_name: pier-{name}-node-{num}
    command: --seeds={seeds_str} --smp {smp} --memory {memory} --overprovisioned 1 --api-address 0.0.0.0 --listen-address 0.0.0.0 --broadcast-address {mesh} --broadcast-rpc-address {mesh}
    ports:
      - "0.0.0.0:7000:7000"
      - "0.0.0.0:9042:9042"
    volumes:
      - node{num}_data:/var/lib/scylla
    healthcheck:
      test: ["CMD-SHELL", "nodetool status | grep -E '^UN'"]
      interval: 15s
      timeout: 10s
      retries: 12
      start_period: 30s
    restart: unless-stopped
    labels:
      pier.cluster.role: node
"#
        ));
        vols.push(format!("node{num}_data"));
    }
    if !vols.is_empty() {
        yaml.push_str("volumes:\n");
        for v in &vols {
            yaml.push_str(&format!("  {v}:\n"));
        }
    }
    Ok(yaml)
}

/// Redis master/replica across servers (bitnami). node 0 is the master; each
/// replica dials it at its mesh IP + published port. One port per node, so
/// co-located replicas are fine (replica → master only, no inter-replica gossip).
fn build_redis_distributed(
    target: &str,
    nodes: &[ClusterNodePlan],
    vars: &HashMap<String, String>,
) -> Result<String> {
    let name = vars.get("name").map(|s| s.as_str()).unwrap_or("redis");
    let version = vars.get("version").map(|s| s.as_str()).unwrap_or("8.0");
    let password = vars
        .get("password")
        .map(|s| s.as_str())
        .unwrap_or("changeme");
    let master = nodes
        .first()
        .ok_or_else(|| anyhow::anyhow!("redis cluster needs at least one node"))?;

    let mut yaml = String::from("services:\n");
    let mut vols: Vec<String> = Vec::new();
    for n in nodes.iter().filter(|n| n.server_id == target) {
        let num = n.index + 1;
        let hp = n.host_port;
        let head = format!(
            "  redis-{num}:\n    image: bitnami/redis:{version}\n    container_name: pier-{name}-node-{num}\n    ports:\n      - \"0.0.0.0:{hp}:6379\"\n    environment:\n"
        );
        yaml.push_str(&head);
        if n.index == 0 {
            yaml.push_str(&format!(
                "      REDIS_REPLICATION_MODE: master\n      REDIS_PASSWORD: \"{password}\"\n    volumes:\n      - node{num}_data:/bitnami/redis/data\n    restart: unless-stopped\n    labels:\n      pier.cluster.role: master\n"
            ));
        } else {
            yaml.push_str(&format!(
                "      REDIS_REPLICATION_MODE: slave\n      REDIS_MASTER_HOST: \"{mh}\"\n      REDIS_MASTER_PORT_NUMBER: \"{mp}\"\n      REDIS_MASTER_PASSWORD: \"{password}\"\n      REDIS_PASSWORD: \"{password}\"\n    volumes:\n      - node{num}_data:/bitnami/redis/data\n    restart: unless-stopped\n    labels:\n      pier.cluster.role: replica\n",
                mh = master.mesh_addr,
                mp = master.host_port
            ));
        }
        vols.push(format!("node{num}_data"));
    }
    if !vols.is_empty() {
        yaml.push_str("volumes:\n");
        for v in &vols {
            yaml.push_str(&format!("  {v}:\n"));
        }
    }
    Ok(yaml)
}

/// MongoDB replica set across servers. Every member must reach every other, so
/// the `rs.initiate` member list addresses each node by its server's mesh IP +
/// published port. node 0 runs the `rs.initiate` from a healthcheck.
fn build_mongodb_distributed(
    target: &str,
    nodes: &[ClusterNodePlan],
    vars: &HashMap<String, String>,
) -> Result<String> {
    let name = vars.get("name").map(|s| s.as_str()).unwrap_or("mongo");
    let version = vars.get("version").map(|s| s.as_str()).unwrap_or("8.0");

    // Full member list by mesh address (used in the rs.initiate on node 0).
    let members: Vec<String> = nodes
        .iter()
        .map(|n| {
            format!(
                "{{_id:{}, host:'{}:{}'{}}}",
                n.index,
                n.mesh_addr,
                n.host_port,
                if n.index == 0 {
                    ", priority:2"
                } else {
                    ", priority:1"
                }
            )
        })
        .collect();
    let members_str = members.join(",");

    let mut yaml = String::from("services:\n");
    let mut vols: Vec<String> = Vec::new();
    for n in nodes.iter().filter(|n| n.server_id == target) {
        let hp = n.host_port;
        let num = n.index + 1; // 1-based service/container naming
        let common = format!(
            "  mongo-{num}:\n    image: mongo:{version}\n    container_name: pier-{name}-node-{num}\n    command: [\"mongod\", \"--replSet\", \"rs0\", \"--bind_ip_all\"]\n    ports:\n      - \"0.0.0.0:{hp}:27017\"\n    volumes:\n      - node{num}_data:/data/db\n"
        );
        yaml.push_str(&common);
        if n.index == 0 {
            yaml.push_str(&format!(
                "    healthcheck:\n      test: [\"CMD-SHELL\", \"mongosh --quiet --eval \\\"try {{ var s = rs.status(); if(s.ok) quit(0); }} catch(e) {{ rs.initiate({{_id:'rs0', members:[{members_str}]}}); }} quit(1);\\\"\"]\n      interval: 10s\n      timeout: 10s\n      retries: 15\n      start_period: 15s\n    restart: unless-stopped\n    labels:\n      pier.cluster.role: primary\n"
            ));
        } else {
            yaml.push_str(
                "    restart: unless-stopped\n    labels:\n      pier.cluster.role: secondary\n",
            );
        }
        vols.push(format!("node{num}_data"));
    }
    if !vols.is_empty() {
        yaml.push_str("volumes:\n");
        for v in &vols {
            yaml.push_str(&format!("  {v}:\n"));
        }
    }
    Ok(yaml)
}

/// Per-DB knobs for the Bitnami streaming-replication trio.
struct ReplCfg {
    catalog: &'static str,
    image: &'static str, // e.g. "bitnami/postgresql"
    default_version: &'static str,
    default_name: &'static str,
    internal_port: u16,        // in-container DB port
    data_path: &'static str,   // volume mount path
    primary_svc: &'static str, // co-located container DNS name
    db_var: &'static str,      // vars key holding the initial DB name
    default_db: &'static str,
    healthcheck: &'static str, // YAML `test:` line value
    // env key names (Bitnami)
    mode_key: &'static str,
    repl_user_key: &'static str,
    repl_pass_key: &'static str,
    master_host_key: &'static str,
    master_port_key: &'static str,
    // primary-only env block (username/password/database), built inline
    primary_env: fn(pw: &str, db: &str) -> String,
    // replica-only auth env (root/master password), built inline
    replica_auth_env: fn(pw: &str) -> String,
}

const PG_CFG: ReplCfg = ReplCfg {
    catalog: "postgresql",
    image: "bitnami/postgresql",
    default_version: "17",
    default_name: "pg",
    internal_port: 5432,
    data_path: "/bitnami/postgresql",
    primary_svc: "postgresql-primary",
    db_var: "POSTGRES_DB",
    default_db: "postgres",
    healthcheck: "[\"CMD-SHELL\", \"pg_isready -U postgres\"]",
    mode_key: "POSTGRESQL_REPLICATION_MODE",
    repl_user_key: "POSTGRESQL_REPLICATION_USER",
    repl_pass_key: "POSTGRESQL_REPLICATION_PASSWORD",
    master_host_key: "POSTGRESQL_MASTER_HOST",
    master_port_key: "POSTGRESQL_MASTER_PORT_NUMBER",
    primary_env: pg_primary_env,
    replica_auth_env: pg_replica_auth,
};
fn pg_primary_env(pw: &str, db: &str) -> String {
    format!("      POSTGRESQL_USERNAME: postgres\n      POSTGRESQL_PASSWORD: \"{pw}\"\n      POSTGRESQL_DATABASE: \"{db}\"\n")
}
fn pg_replica_auth(pw: &str) -> String {
    format!("      POSTGRESQL_PASSWORD: \"{pw}\"\n")
}

const MYSQL_CFG: ReplCfg = ReplCfg {
    catalog: "mysql",
    image: "bitnami/mysql",
    default_version: "9.3",
    default_name: "mysql",
    internal_port: 3306,
    data_path: "/bitnami/mysql/data",
    primary_svc: "mysql-primary",
    db_var: "MYSQL_DATABASE",
    default_db: "mydb",
    healthcheck: "[\"CMD\", \"mysqladmin\", \"ping\", \"-h\", \"localhost\"]",
    mode_key: "MYSQL_REPLICATION_MODE",
    repl_user_key: "MYSQL_REPLICATION_USER",
    repl_pass_key: "MYSQL_REPLICATION_PASSWORD",
    master_host_key: "MYSQL_MASTER_HOST",
    master_port_key: "MYSQL_MASTER_PORT_NUMBER",
    primary_env: mysql_primary_env,
    replica_auth_env: mysql_replica_auth,
};
fn mysql_primary_env(pw: &str, db: &str) -> String {
    format!("      MYSQL_ROOT_PASSWORD: \"{pw}\"\n      MYSQL_DATABASE: \"{db}\"\n")
}
fn mysql_replica_auth(pw: &str) -> String {
    format!("      MYSQL_MASTER_ROOT_PASSWORD: \"{pw}\"\n")
}

const MARIADB_CFG: ReplCfg = ReplCfg {
    catalog: "mariadb",
    image: "bitnami/mariadb",
    default_version: "11.8",
    default_name: "mariadb",
    internal_port: 3306,
    data_path: "/bitnami/mariadb/data",
    primary_svc: "mariadb-primary",
    db_var: "MARIADB_DATABASE",
    default_db: "mydb",
    healthcheck: "[\"CMD\", \"healthcheck.sh\", \"--connect\", \"--innodb_initialized\"]",
    mode_key: "MARIADB_REPLICATION_MODE",
    repl_user_key: "MARIADB_REPLICATION_USER",
    repl_pass_key: "MARIADB_REPLICATION_PASSWORD",
    master_host_key: "MARIADB_MASTER_HOST",
    master_port_key: "MARIADB_MASTER_PORT_NUMBER",
    primary_env: mariadb_primary_env,
    replica_auth_env: mariadb_replica_auth,
};
fn mariadb_primary_env(pw: &str, db: &str) -> String {
    format!("      MARIADB_ROOT_PASSWORD: \"{pw}\"\n      MARIADB_DATABASE: \"{db}\"\n")
}
fn mariadb_replica_auth(pw: &str) -> String {
    format!("      MARIADB_MASTER_ROOT_PASSWORD: \"{pw}\"\n")
}

/// Generic per-server builder for the Bitnami master/slave trio.
fn build_repl_distributed(
    target: &str,
    nodes: &[ClusterNodePlan],
    vars: &HashMap<String, String>,
    cfg: &ReplCfg,
) -> Result<String> {
    let name = vars
        .get("name")
        .map(|s| s.as_str())
        .unwrap_or(cfg.default_name);
    let version = vars
        .get("version")
        .map(|s| s.as_str())
        .unwrap_or(cfg.default_version);
    let password = vars
        .get("password")
        .map(|s| s.as_str())
        .unwrap_or("changeme");
    let db_name = vars
        .get(cfg.db_var)
        .map(|s| s.as_str())
        .unwrap_or(cfg.default_db);
    let repl_password = vars
        .get("repl_password")
        .map(|s| s.as_str())
        .unwrap_or(password);
    let primary = nodes
        .iter()
        .find(|n| n.role == "primary")
        .ok_or_else(|| anyhow::anyhow!("cluster {} has no primary node", cfg.catalog))?;

    let image = &cfg.image;
    let iport = cfg.internal_port;
    let mut yaml = String::from("services:\n");
    let mut vols: Vec<String> = Vec::new();

    for n in nodes.iter().filter(|n| n.server_id == target) {
        let hp = n.host_port;
        if n.role == "primary" {
            yaml.push_str(&format!(
                "  {svc}:\n    image: {image}:{version}\n    container_name: pier-{name}-primary\n    ports:\n      - \"0.0.0.0:{hp}:{iport}\"\n    environment:\n      {mode}: master\n      {ruser}: repl_user\n      {rpass}: \"{repl_password}\"\n{penv}    volumes:\n      - primary_data:{data}\n    healthcheck:\n      test: {hc}\n      interval: 10s\n      timeout: 5s\n      retries: 5\n      start_period: 30s\n    restart: unless-stopped\n    labels:\n      pier.cluster.role: primary\n",
                svc = cfg.primary_svc,
                mode = cfg.mode_key,
                ruser = cfg.repl_user_key,
                rpass = cfg.repl_pass_key,
                penv = (cfg.primary_env)(password, db_name),
                data = cfg.data_path,
                hc = cfg.healthcheck,
            ));
            vols.push("primary_data".to_string());
        } else {
            let i = n.index;
            let co_located = primary.server_id == n.server_id;
            let (mhost, mport) = if co_located {
                (cfg.primary_svc.to_string(), iport)
            } else {
                (primary.mesh_addr.clone(), primary.host_port)
            };
            // depends_on only works within the same compose (same server).
            let depends = if co_located {
                format!(
                    "    depends_on:\n      {}:\n        condition: service_healthy\n",
                    cfg.primary_svc
                )
            } else {
                String::new()
            };
            yaml.push_str(&format!(
                "  {svc}-replica-{i}:\n    image: {image}:{version}\n    container_name: pier-{name}-replica-{i}\n    ports:\n      - \"0.0.0.0:{hp}:{iport}\"\n    environment:\n      {mode}: slave\n      {ruser}: repl_user\n      {rpass}: \"{repl_password}\"\n      {mhost_k}: \"{mhost}\"\n      {mport_k}: {mport}\n{rauth}    volumes:\n      - replica{i}_data:{data}\n{depends}    restart: unless-stopped\n    labels:\n      pier.cluster.role: replica\n",
                svc = cfg.catalog,
                mode = cfg.mode_key,
                ruser = cfg.repl_user_key,
                rpass = cfg.repl_pass_key,
                mhost_k = cfg.master_host_key,
                mport_k = cfg.master_port_key,
                rauth = (cfg.replica_auth_env)(password),
                data = cfg.data_path,
            ));
            vols.push(format!("replica{i}_data"));
        }
    }

    if !vols.is_empty() {
        yaml.push_str("volumes:\n");
        for v in &vols {
            yaml.push_str(&format!("  {v}:\n"));
        }
    }
    Ok(yaml)
}

/// Get the decommission command for a given database type and node.
#[allow(dead_code)]
pub fn decommission_command(catalog_id: &str, _node_role: &str) -> Option<Vec<String>> {
    match catalog_id {
        "cassandra" | "scylladb" => Some(vec!["nodetool".to_string(), "decommission".to_string()]),
        "mongodb" => {
            // MongoDB removal is done via rs.remove() on the primary, not on the node itself
            None
        }
        // PostgreSQL, MySQL, MariaDB, Redis: just stop the replica, no decommission needed
        _ => None,
    }
}
