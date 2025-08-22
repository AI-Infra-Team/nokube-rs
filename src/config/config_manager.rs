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

pub struct ConfigManager {
    etcd_manager: EtcdManager,
}

impl ConfigManager {
    pub async fn get_cluster_config(&self, cluster_name: &str) -> Result<Option<ClusterConfig>> {
        self.etcd_manager.get_cluster_config(cluster_name).await
    }
    pub async fn new() -> Result<Self> {
        let config = Self::load_nokube_config()?;
        let etcd_manager = EtcdManager::new(config.etcd_endpoints).await?;
        Ok(Self { etcd_manager })
    }

    fn load_nokube_config() -> Result<NokubeConfig> {
        let mut configs_found = Vec::new();
        
        // Try global config first
        let global_config_path = PathBuf::from("/etc/.nokube/config.yaml");
        if global_config_path.exists() {
            configs_found.push(("Global", global_config_path.clone()));
        }

        // Try user-level config
        let user_config_path = if let Ok(home_dir) = std::env::var("HOME") {
            let path = PathBuf::from(format!("{}/.nokube/config.yaml", home_dir));
            if path.exists() {
                configs_found.push(("User", path.clone()));
                Some(path)
            } else {
                None
            }
        } else {
            None
        };

        // Log discovered configs
        tracing::info!("Nokube config discovery: found {} config(s)", configs_found.len());
        for (config_type, path) in &configs_found {
            tracing::info!("  {} config: {}", config_type, path.display());
        }

        // Use user config if available (higher priority), otherwise global
        let config_to_use = if let Some(user_path) = user_config_path {
            tracing::info!("Using user config (higher priority): {}", user_path.display());
            user_path
        } else if global_config_path.exists() {
            tracing::info!("Using global config: {}", global_config_path.display());
            global_config_path
        } else {
                        anyhow::bail!(
                                "Nokube configuration not found. Please create config at:\n\
                                 - Global: /etc/.nokube/config.yaml\n\
                                 - User: ~/.nokube/config.yaml\n\
                                 \n\
                                 Example config:\n\
                                 etcd_endpoints:\n\
                                     - '127.0.0.1:2379'"
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
}