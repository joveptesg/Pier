use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub email: String,
    #[serde(skip_serializing)]
    pub password: String,
    pub role: String,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub expires_at: String,
    pub created_at: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub description: String,
    pub port_range_start: Option<i64>,
    pub port_range_end: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub id: String,
    pub project_id: Option<String>,
    pub name: String,
    pub service_type: String,
    pub container_id: Option<String>,
    pub compose_path: Option<String>,
    pub compose_content: Option<String>,
    pub status: String,
    pub port: Option<i64>,
    pub domain: Option<String>,
    pub image: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    // Migration 2 fields
    pub catalog_id: Option<String>,
    pub category: Option<String>,
    pub env_json: Option<String>,
    pub volumes_json: Option<String>,
    // Migration 28 fields
    pub replicas: i64,
    pub lb_strategy: String,
    pub lb_sticky_cookie: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceReplica {
    pub id: String,
    pub service_id: String,
    pub server_id: Option<String>,
    pub replica_idx: i64,
    pub host_port: i64,
    pub container_id: Option<String>,
    pub weight: i64,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortAllocation {
    pub id: String,
    pub service_id: String,
    pub port_name: String,
    pub host_port: i64,
    pub container_port: i64,
    pub protocol: String,
    pub is_public: bool,
    pub public_port: Option<i64>,
    pub created_at: String,
    pub compose_service: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentLog {
    pub id: String,
    pub service_id: Option<String>,
    pub action: String,
    pub status: String,
    pub output: String,
    pub triggered_by: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Setting {
    pub key: String,
    pub value: String,
    pub updated_at: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryCredential {
    pub id: String,
    pub project_id: Option<String>,
    pub registry: String,
    pub username: String,
    #[serde(skip_serializing)]
    pub password_enc: String,
    pub label: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
