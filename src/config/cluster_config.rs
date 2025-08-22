use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeployStatus {
    Pending,
    Finished,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterMeta {
    pub cluster_name: String,
    pub config: ClusterConfig,
    pub deploy_status: DeployStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    pub cluster_name: String,
    pub task_spec: TaskSpec,
    pub nodes: Vec<NodeConfig>,
    pub nokube_config: NokubeSpecificConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub version: String,
    pub monitoring: MonitoringConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub ssh_url: String,
    pub name: String,
    pub role: NodeRole,
    pub storage: StorageConfig,
    pub users: Vec<UserConfig>,
    pub proxy: Option<ProxyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub r#type: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserConfig {
    pub userid: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    Head,
    Worker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    pub image: String,
    pub ports: Vec<u16>,
    pub environment: HashMap<String, String>,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringConfig {
    pub grafana: GrafanaConfig,
    pub greptimedb: GreptimeDbConfig,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrafanaConfig {
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GreptimeDbConfig {
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NokubeSpecificConfig {
    pub log_level: Option<String>,
    pub metrics_interval: Option<u64>,
    pub config_poll_interval: Option<u64>,
}

impl NodeConfig {
    /// 从 SSH URL 中提取节点的 IP 地址
    /// 例如: "192.168.1.100:22" -> "192.168.1.100"
    pub fn get_ip(&self) -> anyhow::Result<&str> {
        self.ssh_url
            .split(':')
            .next()
            .ok_or_else(|| anyhow::anyhow!("Invalid SSH URL format: {}", self.ssh_url))
    }
}

impl ClusterConfig {
    pub fn new(cluster_name: &str) -> Self {
        Self {
            cluster_name: cluster_name.to_string(),
            task_spec: TaskSpec {
                version: "1.0".to_string(),
                monitoring: MonitoringConfig {
                    grafana: GrafanaConfig { port: 3000 },
                    greptimedb: GreptimeDbConfig { port: 4000 },
                    enabled: true,
                },
            },
            nodes: Vec::new(),
            nokube_config: NokubeSpecificConfig {
                log_level: Some("INFO".to_string()),
                metrics_interval: Some(30),
                config_poll_interval: Some(10),
            },
        }
    }

    pub fn add_node(&mut self, node: NodeConfig) {
        self.nodes.push(node);
    }

    pub fn add_service(&mut self, service: ServiceConfig) {
        // services will be handled separately as they're not part of the cluster config
    }
}
