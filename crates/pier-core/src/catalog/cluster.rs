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
