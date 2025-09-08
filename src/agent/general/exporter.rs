use serde::{Deserialize, Serialize};
use tokio::time::Duration;
use anyhow::Result;
use reqwest::Client;
use crate::config::{etcd_manager::EtcdManager, cluster_config::ClusterConfig};
use super::docker_runner::DockerRunner;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;
use std::process::Command as StdCommand;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub timestamp: u64,
    pub cpu_usage: f64,
    pub cpu_cores: u64,
    pub memory_usage: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
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
                                    
                                    // 收集 actor（容器）级别指标
                                    let containers = Self::list_actor_containers();
                                    let container_metrics = containers.into_iter()
                                        .filter_map(|name| Self::collect_container_metrics(&name).ok().map(|m| (name, m)))
                                        .collect::<Vec<_>>();

                                    if let Err(e) = Self::push_to_greptimedb_all(&client, &greptimedb_url, &metrics, &node_id, &container_metrics, &config.cluster_name).await {
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

    /// 列出由 NoKube 创建的 actor 容器名称
    fn list_actor_containers() -> Vec<String> {
        let docker_path = DockerRunner::get_runtime_path().unwrap_or_else(|_| "docker".to_string());
        let output = StdCommand::new(docker_path)
            .args(["ps", "--format", "{{.Names}}"])
            .output();
        if let Ok(out) = output {
            let text = String::from_utf8_lossy(&out.stdout);
            return text
                .lines()
                .filter(|n| n.starts_with("nokube-pod-"))
                .map(|s| s.to_string())
                .collect();
        }
        Vec::new()
    }

    /// 收集单个容器的 CPU/内存 指标
    fn collect_container_metrics(container: &str) -> Result<(f64, u64, f64)> {
        let docker_path = DockerRunner::get_runtime_path().unwrap_or_else(|_| "docker".to_string());
        let out = StdCommand::new(docker_path)
            .args(["stats", "--no-stream", "--format", "{{.CPUPerc}}\t{{.MemUsage}}\t{{.MemPerc}}", container])
            .output()?;
        if !out.status.success() {
            anyhow::bail!("docker stats failed for {}", container);
        }
        let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if line.is_empty() { anyhow::bail!("empty stats for {}", container); }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 { anyhow::bail!("unexpected stats format: {}", line); }
        let cpu = parts[0].trim().trim_end_matches('%').replace(',', ".").parse::<f64>().unwrap_or(0.0);
        let mem_usage_str = parts[1].trim().split('/').next().unwrap_or(parts[1].trim()).trim();
        let mem_bytes = Self::parse_size_to_bytes(mem_usage_str);
        let mem_pct = parts[2].trim().trim_end_matches('%').replace(',', ".").parse::<f64>().unwrap_or(0.0);
        Ok((cpu, mem_bytes, mem_pct))
    }

    /// 从容器名推断 pod 名、顶层 actor 名和所有者类型（deployment/daemonset）
    pub fn derive_actor_core(container: &str, node: &str, cluster: &str) -> (String, String, String) {
        // 容器名约定: nokube-pod-<pod_name>
        let pod = container.strip_prefix("nokube-pod-").unwrap_or(container).to_string();
        let mut core = pod.clone();
        // DaemonSet: <daemonset>-<node>
        let ds_suffix = format!("-{}", node);
        if core.ends_with(&ds_suffix) {
            core = core.trim_end_matches(&ds_suffix).trim_end_matches('-').to_string();
            // 若末尾还带 -<cluster>，一起去掉，得到 core 名
            let cluster_suffix = format!("-{}", cluster);
            if core.ends_with(&cluster_suffix) {
                core = core.trim_end_matches(&cluster_suffix).trim_end_matches('-').to_string();
            }
            return (pod, core, "daemonset".to_string());
        }
        // 统一去掉 -<cluster>
        let cluster_suffix = format!("-{}", cluster);
        if core.ends_with(&cluster_suffix) {
            core = core.trim_end_matches(&cluster_suffix).trim_end_matches('-').to_string();
        }
        // Deployment: 若末段为8hex，则去掉
        if let Some((b, last)) = core.rsplit_once('-') {
            if last.len() == 8 && last.chars().all(|c| c.is_ascii_hexdigit()) {
                return (pod, b.to_string(), "deployment".to_string());
            }
        }
        (pod, core, "deployment".to_string())
    }

    /// 转义 Influx 标签值（空格、逗号）
    fn esc_tag(v: &str) -> String {
        v.replace(' ', "\\ ").replace(',', "\\,")
    }

    fn parse_size_to_bytes(s: &str) -> u64 {
        // Supports: B, KiB, MiB, GiB, KB, MB, GB
        let s = s.trim();
        let (num, unit) = s.split_at(s.trim_end_matches(|c: char| c.is_alphabetic()).len());
        let val = num.trim().replace(',', ".").parse::<f64>().unwrap_or(0.0);
        let unit = unit.trim().to_ascii_lowercase();
        let mult = match unit.as_str() {
            "b" => 1.0,
            "kib" => 1024.0,
            "mib" => 1024.0 * 1024.0,
            "gib" => 1024.0 * 1024.0 * 1024.0,
            "kb" => 1000.0,
            "mb" => 1000.0 * 1000.0,
            "gb" => 1000.0 * 1000.0 * 1000.0,
            _ => 1.0,
        };
        (val * mult) as u64
    }

    async fn collect_system_metrics(node_id: &str) -> Result<SystemMetrics> {
        let cpu_usage = Self::get_cpu_usage().await?;
        let cpu_cores = Self::get_cpu_cores().await?;
        let (memory_usage, memory_used_bytes, memory_total_bytes) = Self::get_memory_stats().await?;
        let (network_rx_bytes, network_tx_bytes) = Self::get_network_stats().await?;
        
        Ok(SystemMetrics {
            timestamp: chrono::Utc::now().timestamp() as u64,
            cpu_usage,
            cpu_cores,
            memory_usage,
            memory_used_bytes,
            memory_total_bytes,
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

    async fn get_memory_stats() -> Result<(f64, u64, u64)> {
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
            let mem_used_kb = mem_total.saturating_sub(mem_available);
            let memory_usage = (mem_used_kb as f64 / mem_total as f64) * 100.0;
            let mem_used_bytes = mem_used_kb.saturating_mul(1024);
            let mem_total_bytes = mem_total.saturating_mul(1024);
            return Ok((memory_usage.max(0.0).min(100.0), mem_used_bytes, mem_total_bytes));
        }
        
        Ok((0.0, 0, 0))
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

    async fn get_cpu_cores() -> Result<u64> {
        // Prefer /proc/cpuinfo logical processor count
        let content = tokio::fs::read_to_string("/proc/cpuinfo").await.unwrap_or_default();
        let mut count = 0u64;
        for line in content.lines() {
            // Each logical processor in linux shows a 'processor\t: N' entry
            if line.to_ascii_lowercase().starts_with("processor") && line.contains(':') {
                count += 1;
            }
        }
        if count == 0 {
            // Fallback: try /sys/devices/system/cpu/present like 0-3 -> 4 cores
            if let Ok(present) = tokio::fs::read_to_string("/sys/devices/system/cpu/present").await {
                let s = present.trim();
                if let Some((start, end)) = s.split_once('-') {
                    if let (Ok(a), Ok(b)) = (start.parse::<u64>(), end.parse::<u64>()) {
                        count = b.saturating_sub(a) + 1;
                    }
                }
            }
        }
        if count == 0 { count = 1; } // sensible fallback
        Ok(count)
    }

    async fn push_to_greptimedb_all(
        client: &Client,
        greptimedb_url: &str,
        metrics: &SystemMetrics,
        node_id: &str,
        container_metrics: &[(String, (f64, u64, f64))],
        cluster_name: &str,
    ) -> Result<()> {
        let instance = &metrics.node_id;
        // 为了方便格式化，准备一个闭包构建公共标签（系统级）
        let node_tag = Self::esc_tag(instance);

        // 使用 InfluxDB 行协议格式，这是 GreptimeDB 推荐的写入方式
        let mut influxdb_metrics = String::new();
        let ts = metrics.timestamp * 1_000_000_000;
        // 系统级指标附带 cluster_name 与 node 标签
        let sys_tags = format!(
            ",cluster_name={},node={},instance={}",
            Self::esc_tag(cluster_name),
            node_tag,
            node_tag
        );
        influxdb_metrics.push_str(&format!("nokube_cpu_usage{} value={} {}\n", sys_tags, metrics.cpu_usage, ts));
        influxdb_metrics.push_str(&format!("nokube_cpu_cores{} value={} {}\n", sys_tags, metrics.cpu_cores, ts));
        influxdb_metrics.push_str(&format!("nokube_memory_usage{} value={} {}\n", sys_tags, metrics.memory_usage, ts));
        influxdb_metrics.push_str(&format!("nokube_memory_used_bytes{} value={} {}\n", sys_tags, metrics.memory_used_bytes, ts));
        influxdb_metrics.push_str(&format!("nokube_memory_total_bytes{} value={} {}\n", sys_tags, metrics.memory_total_bytes, ts));
        influxdb_metrics.push_str(&format!("nokube_network_rx_bytes{} value={} {}\n", sys_tags, metrics.network_rx_bytes, ts));
        influxdb_metrics.push_str(&format!("nokube_network_tx_bytes{} value={} {}\n", sys_tags, metrics.network_tx_bytes, ts));

        // 追加容器级别指标
        for (name, (cpu_pct, mem_bytes, mem_pct)) in container_metrics {
            let container_tag = Self::esc_tag(name);
            let (pod, core_actor, owner_type) = Self::derive_actor_core(name, instance, cluster_name);
            // 规范化顶层名：始终为 <core>-<cluster>
            let canonical_upper = format!("{}-{}", core_actor, cluster_name);
            let pod_tag = Self::esc_tag(&pod);
            let upper_tag = Self::esc_tag(&canonical_upper);
            let actor_tag = upper_tag.clone();
            let owner_tag = Self::esc_tag(&owner_type);
            let parent_tag = pod_tag.clone();
            // absolute cpu in cores
            let cpu_cores_val = (cpu_pct / 100.0) * (metrics.cpu_cores as f64);
            // richer label set for easier Grafana queries
            let container_path = format!("{}/{}/{}/{}", cluster_name, core_actor, pod, name);
            let container_path_tag = Self::esc_tag(&container_path);
            let tags = format!(
                ",cluster_name={},node={},instance={},container={},container_name={},pod={},pod_name={},parent_actor={},root_actor={},top_actor={},upper_actor={},owner_type={},container_path={},canonical=1",
                Self::esc_tag(cluster_name),
                node_tag,
                node_tag,
                container_tag,
                container_tag,
                pod_tag,
                pod_tag,
                parent_tag,
                upper_tag,
                actor_tag,
                upper_tag,
                owner_tag,
                container_path_tag
            );
            influxdb_metrics.push_str(&format!("nokube_container_cpu{} value={} {}\n", tags, cpu_pct, ts));
            influxdb_metrics.push_str(&format!("nokube_container_cpu_cores{} value={} {}\n", tags, cpu_cores_val, ts));
            influxdb_metrics.push_str(&format!("nokube_container_mem_bytes{} value={} {}\n", tags, mem_bytes, ts));
            influxdb_metrics.push_str(&format!("nokube_container_mem_percent{} value={} {}\n", tags, mem_pct, ts));
        }

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

        tracing::debug!("Successfully pushed metrics (node+containers) to GreptimeDB for node: {}", instance);
        Ok(())
    }
}
