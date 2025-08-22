use serde::{Deserialize, Serialize};
use tokio::time::Duration;
use anyhow::Result;
use reqwest::Client;
use crate::config::{etcd_manager::EtcdManager, cluster_config::ClusterConfig};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

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
                                head_node.get_ip().map_err(|e| anyhow::anyhow!("Failed to get node IP: {}", e))?,
                                config.task_spec.monitoring.greptimedb.port
                            );
                            
                            match Self::collect_system_metrics(&node_id).await {
                                Ok(metrics) => {
                                    info!("📊 Metrics collected for node '{}': CPU: {:.1}%, Memory: {:.1}%, RX: {} bytes, TX: {} bytes", 
                                          node_id, metrics.cpu_usage, metrics.memory_usage, metrics.network_rx_bytes, metrics.network_tx_bytes);
                                    
                                    if let Err(e) = Self::push_to_greptimedb(&client, &greptimedb_url, &metrics).await {
                                        tracing::error!("Failed to push metrics to GreptimeDB URL {}: {}", greptimedb_url, e);
                                    } else {
                                        info!("✅ Successfully pushed metrics to GreptimeDB for node '{}'", node_id);
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
        // 读取 /proc/stat 获取CPU使用率
        let stat_content = tokio::fs::read_to_string("/proc/stat").await?;
        let line = stat_content.lines().next().unwrap_or("");
        
        // 解析第一行: cpu user nice system idle iowait irq softirq steal guest guest_nice
        let values: Vec<u64> = line.split_whitespace()
            .skip(1) // 跳过 "cpu" 标签
            .take(10)
            .filter_map(|s| s.parse().ok())
            .collect();
            
        if values.len() >= 4 {
            let idle = values[3];
            let total: u64 = values.iter().sum();
            if total > 0 {
                let cpu_usage = 100.0 - (idle as f64 / total as f64 * 100.0);
                return Ok(cpu_usage.max(0.0).min(100.0));
            }
        }
        
        // 如果解析失败，返回默认值
        Ok(0.0)
    }

    async fn get_memory_usage() -> Result<f64> {
        // 读取 /proc/meminfo 获取内存使用率
        let meminfo_content = tokio::fs::read_to_string("/proc/meminfo").await?;
        
        let mut mem_total = 0u64;
        let mut mem_available = 0u64;
        
        for line in meminfo_content.lines() {
            if line.starts_with("MemTotal:") {
                if let Some(value) = line.split_whitespace().nth(1) {
                    mem_total = value.parse().unwrap_or(0);
                }
            } else if line.starts_with("MemAvailable:") {
                if let Some(value) = line.split_whitespace().nth(1) {
                    mem_available = value.parse().unwrap_or(0);
                }
            }
        }
        
        if mem_total > 0 {
            let mem_used = mem_total.saturating_sub(mem_available);
            let memory_usage = (mem_used as f64 / mem_total as f64) * 100.0;
            return Ok(memory_usage.max(0.0).min(100.0));
        }
        
        Ok(0.0)
    }

    async fn get_network_stats() -> Result<(u64, u64)> {
        // 读取 /proc/net/dev 获取网络统计信息
        let netdev_content = tokio::fs::read_to_string("/proc/net/dev").await?;
        
        let mut total_rx_bytes = 0u64;
        let mut total_tx_bytes = 0u64;
        
        for line in netdev_content.lines().skip(2) { // 跳过头部两行
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 10 {
                let interface = parts[0].trim_end_matches(':');
                // 忽略 loopback 接口
                if interface != "lo" {
                    if let (Ok(rx), Ok(tx)) = (parts[1].parse::<u64>(), parts[9].parse::<u64>()) {
                        total_rx_bytes += rx;
                        total_tx_bytes += tx;
                    }
                }
            }
        }
        
        Ok((total_rx_bytes, total_tx_bytes))
    }

    async fn push_to_greptimedb(client: &Client, greptimedb_url: &str, metrics: &SystemMetrics) -> Result<()> {
        let instance = &metrics.node_id;
        
        // 使用 InfluxDB 行协议格式，这是 GreptimeDB 推荐的写入方式
        let influxdb_metrics = format!(
            "nokube_cpu_usage,instance={} value={} {}\n\
             nokube_memory_usage,instance={} value={} {}\n\
             nokube_network_rx_bytes,instance={} value={} {}\n\
             nokube_network_tx_bytes,instance={} value={} {}",
            instance, metrics.cpu_usage, metrics.timestamp * 1_000_000_000,
            instance, metrics.memory_usage, metrics.timestamp * 1_000_000_000,
            instance, metrics.network_rx_bytes, metrics.timestamp * 1_000_000_000,
            instance, metrics.network_tx_bytes, metrics.timestamp * 1_000_000_000
        );

        // 使用 InfluxDB 写入端点而不是 Prometheus remote write
        let url = format!("{}/v1/influxdb/write", greptimedb_url);
        
        tracing::debug!("Pushing metrics to GreptimeDB URL: {}", url);
        tracing::debug!("Request body (InfluxDB format):\n{}", influxdb_metrics);
        
        let response = client
            .post(&url)
            .header("Content-Type", "text/plain")
            .body(influxdb_metrics.clone())
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_else(|_| "Failed to read error response".to_string());
            anyhow::bail!(
                "Failed to push metrics to GreptimeDB URL {}: {} {}\nRequest body (InfluxDB format):\n{}\nResponse body:\n{}", 
                url, 
                status.as_u16(), 
                status.canonical_reason().unwrap_or("Unknown"),
                influxdb_metrics,
                error_body
            );
        }

        tracing::debug!("Successfully pushed metrics to GreptimeDB for node: {}", instance);
        Ok(())
    }
}