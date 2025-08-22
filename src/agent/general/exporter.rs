use serde::{Deserialize, Serialize};
use tokio::time::Duration;
use anyhow::Result;
use reqwest::Client;
use crate::config::{etcd_manager::EtcdManager, cluster_config::ClusterConfig};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub timestamp: u64,
    pub cpu_usage: f64,
    pub memory_usage: f64,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
    pub node_id: String,
}

pub struct Exporter {
    client: Client,
    node_id: String,
    cluster_name: String,
    etcd_manager: Arc<EtcdManager>,
    current_config: Arc<RwLock<Option<ClusterConfig>>>,
}

impl Exporter {
    pub fn new(node_id: String, cluster_name: String, etcd_manager: Arc<EtcdManager>) -> Self {
        Self {
            client: Client::new(),
            node_id,
            cluster_name,
            etcd_manager,
            current_config: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn start_with_etcd_polling(&self) -> Result<()> {
        let config_poller = self.start_config_polling().await?;
        let metrics_collector = self.start_metrics_collection().await?;
        
        tokio::try_join!(config_poller, metrics_collector)?;
        Ok(())
    }

    async fn start_config_polling(&self) -> Result<tokio::task::JoinHandle<Result<()>>> {
        let etcd_manager = Arc::clone(&self.etcd_manager);
        let cluster_name = self.cluster_name.clone();
        let current_config = Arc::clone(&self.current_config);
        
        let handle = tokio::spawn(async move {
            loop {
                match etcd_manager.get_cluster_config(&cluster_name).await {
                    Ok(Some(config)) => {
                        let mut config_guard = current_config.write().await;
                        *config_guard = Some(config.clone());
                        
                        let poll_interval = config.nokube_config.config_poll_interval.unwrap_or(10);
                        drop(config_guard);
                        
                        tokio::time::sleep(Duration::from_secs(poll_interval)).await;
                    }
                    Ok(None) => {
                        tracing::warn!("No config found for cluster: {}", cluster_name);
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                    Err(e) => {
                        tracing::error!("Failed to get cluster config: {}", e);
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                }
            }
        });
        
        Ok(handle)
    }

    async fn start_metrics_collection(&self) -> Result<tokio::task::JoinHandle<Result<()>>> {
        let current_config = Arc::clone(&self.current_config);
        let client = self.client.clone();
        let node_id = self.node_id.clone();
        
        let handle = tokio::spawn(async move {
            loop {
                let config_guard = current_config.read().await;
                if let Some(config) = config_guard.as_ref() {
                    if config.task_spec.monitoring.enabled {
                        let interval_seconds = config.nokube_config.metrics_interval.unwrap_or(30);
                        // 直接使用节点列表中的地址进行推送，而不需要额外的配置
                        
                        // 查找启用了greptimedb的节点作为推送目标
                        if let Some(head_node) = config.nodes.iter().find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head)) {
                            let greptimedb_url = format!("http://{}:{}", 
                                head_node.ssh_url.split(':').next().unwrap_or("localhost"),
                                config.task_spec.monitoring.greptimedb.port
                            );
                            
                            match Self::collect_system_metrics(&node_id).await {
                                Ok(metrics) => {
                                    if let Err(e) = Self::push_to_greptimedb(&client, &greptimedb_url, &metrics).await {
                                        tracing::error!("Failed to push metrics to GreptimeDB: {}", e);
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Failed to collect metrics: {}", e);
                                }
                            }
                            
                            tokio::time::sleep(Duration::from_secs(interval_seconds)).await;
                        } else {
                            tracing::warn!("No head node found for metrics collection");
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                    } else {
                        drop(config_guard);
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                } else {
                    drop(config_guard);
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        });
        
        Ok(handle)
    }

    async fn collect_system_metrics(node_id: &str) -> Result<SystemMetrics> {
        let cpu_usage = Self::get_cpu_usage().await?;
        let memory_usage = Self::get_memory_usage().await?;
        let (network_rx_bytes, network_tx_bytes) = Self::get_network_stats().await?;
        
        Ok(SystemMetrics {
            timestamp: chrono::Utc::now().timestamp() as u64,
            cpu_usage,
            memory_usage,
            network_rx_bytes,
            network_tx_bytes,
            node_id: node_id.to_string(),
        })
    }

    async fn get_cpu_usage() -> Result<f64> {
        Ok(50.0) // Placeholder
    }

    async fn get_memory_usage() -> Result<f64> {
        Ok(60.0) // Placeholder
    }

    async fn get_network_stats() -> Result<(u64, u64)> {
        Ok((1024000, 512000)) // Placeholder
    }

    async fn push_to_greptimedb(client: &Client, greptimedb_url: &str, metrics: &SystemMetrics) -> Result<()> {
        let job_name = "nokube_node_metrics";
        let instance = &metrics.node_id;
        
        let prometheus_metrics = format!(
            "# HELP nokube_cpu_usage CPU usage percentage\n\
             # TYPE nokube_cpu_usage gauge\n\
             nokube_cpu_usage{{instance=\"{}\"}} {}\n\
             # HELP nokube_memory_usage Memory usage percentage\n\
             # TYPE nokube_memory_usage gauge\n\
             nokube_memory_usage{{instance=\"{}\"}} {}\n\
             # HELP nokube_network_rx_bytes Network received bytes\n\
             # TYPE nokube_network_rx_bytes counter\n\
             nokube_network_rx_bytes{{instance=\"{}\"}} {}\n\
             # HELP nokube_network_tx_bytes Network transmitted bytes\n\
             # TYPE nokube_network_tx_bytes counter\n\
             nokube_network_tx_bytes{{instance=\"{}\"}} {}\n",
            instance, metrics.cpu_usage,
            instance, metrics.memory_usage,
            instance, metrics.network_rx_bytes,
            instance, metrics.network_tx_bytes
        );

        let url = format!("{}/v1/prometheus/write", greptimedb_url);
        
        let response = client
            .post(&url)
            .header("Content-Type", "text/plain; version=0.0.4")
            .body(prometheus_metrics)
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to push metrics to GreptimeDB: {}", response.status());
        }

        tracing::debug!("Successfully pushed metrics to GreptimeDB for node: {}", instance);
        Ok(())
    }
}