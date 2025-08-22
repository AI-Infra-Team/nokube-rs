use crate::agent::general::RemoteExecutor;
use crate::config::{cluster_config::ClusterConfig, etcd_manager::EtcdManager};
use anyhow::Result;
use std::sync::Arc;
use tracing::info;

pub struct MasterAgent {
    cluster_config: ClusterConfig,
    remote_executor: RemoteExecutor,
    etcd_manager: Arc<EtcdManager>,
}

impl MasterAgent {
    pub async fn new(cluster_config: ClusterConfig, etcd_endpoints: Vec<String>) -> Result<Self> {
        let etcd_manager = Arc::new(EtcdManager::new(etcd_endpoints).await?);
        
        let remote_executor = if let Some(node) = cluster_config.nodes.first() {
            if let Some(proxy) = &node.proxy {
                RemoteExecutor::with_proxy(proxy.clone())
            } else {
                RemoteExecutor::new()
            }
        } else {
            RemoteExecutor::new()
        };
        
        Ok(Self {
            cluster_config,
            remote_executor,
            etcd_manager,
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        info!("Starting master agent for cluster: {}", self.cluster_config.cluster_name);
        
        self.store_cluster_config().await?;
        
        info!("Master agent started successfully - only managing cluster config");
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        info!("Stopping master agent");
        info!("Master agent stopped");
        Ok(())
    }

    async fn store_cluster_config(&self) -> Result<()> {
        info!("Storing cluster configuration to etcd");
        self.etcd_manager.store_cluster_config(&self.cluster_config).await?;
        Ok(())
    }

    pub async fn update_cluster_config(&mut self, new_config: ClusterConfig) -> Result<()> {
        info!("Updating cluster configuration");
        self.cluster_config = new_config;
        self.store_cluster_config().await?;
        Ok(())
    }

    pub async fn execute_cluster_command(&self, command: &str) -> Result<()> {
        info!("Executing cluster-wide command: {}", command);
        
        for node in &self.cluster_config.nodes {
            match self.remote_executor.execute_command(command).await {
                Ok(result) => {
                    info!("Node {} command result: {}", node.ssh_url, result.stdout);
                }
                Err(e) => {
                    tracing::error!("Failed to execute command on node {}: {}", node.ssh_url, e);
                }
            }
        }
        
        Ok(())
    }
}