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
    // Optional: explicit storage path; if omitted, use workspace as storage root
    pub storage: Option<StorageConfig>,
    pub users: Vec<UserConfig>,
    pub proxy: Option<ProxyConfig>,
    pub workspace: Option<String>,
    /// 可选：为该节点配置APT源
    pub apt_sources: Option<AptSourcesConfig>,
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
pub struct AptSourcesConfig {
    /// 完整替换 /etc/apt/sources.list 的内容
    pub sources_list: Option<String>,
    /// 在 /etc/apt/sources.list.d/ 下新增的源文件集合（文件名 -> 内容）
    pub sources_list_d: Option<HashMap<String, String>>,
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
    pub httpserver: HttpServerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrafanaConfig {
    pub port: u16,
    pub admin_user: Option<String>,
    pub admin_password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GreptimeDbConfig {
    pub port: u16,
    pub mysql_user: Option<String>,
    pub mysql_password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpServerConfig {
    pub port: u16,
}

/// 固定的 HTTP 文件服务挂载子路径（相对节点 workspace）
pub const HTTP_SERVER_MOUNT_SUBPATH: &str = "public_file_server";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NokubeSpecificConfig {
    pub log_level: Option<String>,
    pub metrics_interval: Option<u64>,
    pub config_poll_interval: Option<u64>,
}

impl NodeConfig {
    /// 从 SSH URL 中提取节点的 IP 地址
    /// 例如: "pa@192.168.1.100:22" -> "192.168.1.100"
    /// 或者: "192.168.1.100:22" -> "192.168.1.100"
    pub fn get_ip(&self) -> anyhow::Result<&str> {
        // 首先检查是否包含@符号
        let host_part = if self.ssh_url.contains('@') {
            self.ssh_url.split('@').nth(1).ok_or_else(|| {
                anyhow::anyhow!("Invalid SSH URL format after @: {}", self.ssh_url)
            })?
        } else {
            &self.ssh_url
        };

        // 然后从主机部分提取IP（去掉端口）
        let ip = host_part
            .split(':')
            .next()
            .ok_or_else(|| anyhow::anyhow!("Invalid host format in SSH URL: {}", self.ssh_url))?;

        Ok(ip)
    }

    /// 获取节点的工作空间路径，如果未指定则报错
    pub fn get_workspace(&self) -> Result<&str, anyhow::Error> {
        self.workspace.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "Missing required workspace configuration for node: {}",
                self.name
            )
        })
    }

    /// 获取节点的存储路径：
    /// - 如果配置了 storage.path，则将其视为相对于 workspace 的路径（或绝对路径）
    /// - 如果未配置 storage，则直接返回 workspace 作为存储根目录
    pub fn get_storage_path(&self) -> Result<String, anyhow::Error> {
        let workspace = self.get_workspace()?;
        match &self.storage {
            Some(storage) => {
                if storage.path.starts_with('/') {
                    // 如果 storage.path 是绝对路径，检查是否已经在 workspace 下
                    if storage.path.starts_with(workspace) {
                        Ok(storage.path.clone())
                    } else {
                        // 否则将其作为相对路径添加到 workspace 下
                        Ok(format!("{}{}", workspace, storage.path))
                    }
                } else {
                    // 如果是相对路径，直接添加到 workspace 下
                    Ok(format!("{}/{}", workspace, storage.path))
                }
            }
            None => {
                // 未配置 storage，使用 workspace 作为存储根目录
                Ok(workspace.to_string())
            }
        }
    }
}

impl ClusterConfig {
    pub fn new(cluster_name: &str) -> Self {
        Self {
            cluster_name: cluster_name.to_string(),
            task_spec: TaskSpec {
                version: "1.0".to_string(),
                monitoring: MonitoringConfig {
                    grafana: GrafanaConfig {
                        port: 3000,
                        admin_user: None,
                        admin_password: None,
                    },
                    greptimedb: GreptimeDbConfig {
                        port: 4000,
                        mysql_user: None,
                        mysql_password: None,
                    },
                    enabled: true,
                    httpserver: HttpServerConfig { port: 8088 },
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

    pub fn add_service(&mut self, _service: ServiceConfig) {
        // services will be handled separately as they're not part of the cluster config
    }

    /// 获取 Head 节点的 IP（严格要求存在，失败向上返回错误）
    pub fn head_node_ip(&self) -> anyhow::Result<String> {
        let head = self
            .nodes
            .iter()
            .find(|n| matches!(n.role, NodeRole::Head))
            .ok_or_else(|| {
                anyhow::anyhow!("Head node not found in cluster '{}'", self.cluster_name)
            })?;
        Ok(head.get_ip()?.to_string())
    }

    /// 生成 OTLP 日志完整 Endpoint（约定优于配置）
    pub fn otlp_logs_endpoint(&self) -> anyhow::Result<String> {
        let ip = self.head_node_ip()?;
        let port = self.task_spec.monitoring.greptimedb.port;
        Ok(format!("http://{}:{}/v1/otlp/v1/logs", ip, port))
    }
}
