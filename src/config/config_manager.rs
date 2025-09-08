use crate::config::etcd_manager::EtcdManager;
use crate::config::cluster_config::ClusterConfig;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;



#[derive(Debug, Deserialize, Serialize)]
pub struct NokubeConfig {
    pub etcd_endpoints: Vec<String>,
}

#[derive(Debug)]
pub struct ConfigManager {
    etcd_manager: EtcdManager,
    etcd_endpoints: Vec<String>,
}

impl ConfigManager {
    /// 返回已存在的本地配置文件路径：优先用户级 (~/.nokube/config.yaml)，其次全局级 (/etc/.nokube/config.yaml)。
    /// 若均不存在，返回 None。
    pub fn try_get_existing_local_config_path() -> Option<PathBuf> {
        // 用户级配置
        if let Ok(home_dir) = std::env::var("HOME") {
            let user_path = PathBuf::from(format!("{}/.nokube/config.yaml", home_dir));
            if user_path.exists() {
                return Some(user_path);
            }
        }
        // 全局级配置
        let global_path = PathBuf::from("/etc/.nokube/config.yaml");
        if global_path.exists() {
            return Some(global_path);
        }
        None
    }

    /// 基于当前 ConfigManager 的 etcd_endpoints 生成最小可用配置的 YAML 文本。
    pub fn generate_minimal_config_yaml(&self) -> String {
        let mut yaml = String::from("etcd_endpoints:\n");
        for ep in &self.etcd_endpoints {
            yaml.push_str(&format!("  - '{}'\n", ep));
        }
        yaml
    }
    pub async fn get_cluster_config(&self, cluster_name: &str) -> Result<Option<ClusterConfig>> {
        self.etcd_manager.get_cluster_config(cluster_name).await
    }

    pub fn get_etcd_endpoints(&self) -> Vec<String> {
        self.etcd_endpoints.clone()
    }

    pub fn get_etcd_manager(&self) -> &EtcdManager {
        &self.etcd_manager
    }
    pub async fn new() -> Result<Self> {
        tracing::info!("Loading nokube configuration...");
        let config = Self::load_nokube_config()?;
        
        tracing::info!("Creating EtcdManager with endpoints: {:?}", config.etcd_endpoints);
        let etcd_manager = EtcdManager::new(config.etcd_endpoints.clone()).await?;
        
        tracing::info!("ConfigManager initialized successfully");
        Ok(Self { 
            etcd_manager,
            etcd_endpoints: config.etcd_endpoints,
        })
    }

    fn load_nokube_config() -> Result<NokubeConfig> {
        // 优先使用用户级配置，其次全局级配置
        let config_to_use = if let Some(path) = Self::try_get_existing_local_config_path() {
            tracing::info!("Using discovered config: {}", path.display());
            path
        } else {
            anyhow::bail!(
                "Nokube configuration not found. Please create config at:\n\
                 - Global: /etc/.nokube/config.yaml\n\
                 - User: ~/.nokube/config.yaml\n\
                 \n\
                 Example config:\n\
                 etcd_endpoints:\n\
                     - 'http://127.0.0.1:2379'"
            );
        };

        let content = fs::read_to_string(&config_to_use)
                .map_err(|e| anyhow::anyhow!("Failed to read config {}: {}", config_to_use.display(), e))?;
            
            serde_yaml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse config {}: {}", config_to_use.display(), e))
    }

    pub async fn init_cluster(&self, cluster_name: &str) -> Result<()> {
        let cluster_config = ClusterConfig::new(cluster_name);
        let meta = crate::config::cluster_config::ClusterMeta {
            cluster_name: cluster_name.to_string(),
            config: cluster_config,
            deploy_status: crate::config::cluster_config::DeployStatus::Pending,
        };
        self.etcd_manager.store_cluster_meta(&meta).await?;
        Ok(())
    }

    pub async fn get_cluster_meta(&self, cluster_name: &str) -> Result<Option<crate::config::cluster_config::ClusterMeta>> {
        self.etcd_manager.get_cluster_meta(cluster_name).await
    }
    pub async fn list_clusters(&self) -> Result<Vec<crate::config::cluster_config::ClusterMeta>> {
        self.etcd_manager.list_cluster_metas().await
    }

    pub async fn update_cluster_config(&self, config: &ClusterConfig) -> Result<()> {
        self.etcd_manager.store_cluster_config(config).await
    }

    // K8s对象存储方法
    pub async fn store_deployment(&self, cluster_name: &str, name: &str, yaml_content: &str) -> Result<()> {
        let key = format!("/nokube/{}/deployments/{}", cluster_name, name);
        self.etcd_manager.put(key, yaml_content.to_string()).await
    }

    pub async fn store_daemonset(&self, cluster_name: &str, name: &str, yaml_content: &str) -> Result<()> {
        let key = format!("/nokube/{}/daemonsets/{}", cluster_name, name);
        self.etcd_manager.put(key, yaml_content.to_string()).await
    }

    pub async fn store_configmap(&self, cluster_name: &str, name: &str, yaml_content: &str) -> Result<()> {
        let key = format!("/nokube/{}/configmaps/{}", cluster_name, name);
        self.etcd_manager.put(key, yaml_content.to_string()).await
    }

    pub async fn store_secret(&self, cluster_name: &str, name: &str, yaml_content: &str) -> Result<()> {
        let key = format!("/nokube/{}/secrets/{}", cluster_name, name);
        self.etcd_manager.put(key, yaml_content.to_string()).await
    }
}
