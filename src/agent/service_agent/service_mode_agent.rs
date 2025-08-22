use crate::agent::general::process_manager::ProcessManager;
use crate::agent::general::Exporter;
use crate::config::{etcd_manager::EtcdManager, cluster_config::ClusterConfig};
use std::sync::Arc;
use anyhow::Result;
use std::process::Command;
use tracing::info;

/// 服务模式Agent：处理持续运行的服务管理
/// 启动监控服务、管理子进程、处理关闭信号
pub struct ServiceModeAgent {
    process_manager: ProcessManager,
    config: ClusterConfig,
    node_id: String,
    cluster_name: String,
    etcd_manager: Option<Arc<EtcdManager>>,
    exporter: Option<Exporter>,
}

impl ServiceModeAgent {
    pub async fn new(node_id: String, cluster_name: String, etcd_endpoints: Vec<String>, config: ClusterConfig) -> Result<Self> {
        let etcd_manager = Arc::new(EtcdManager::new(etcd_endpoints).await?);
        Ok(Self {
            process_manager: ProcessManager::new(),
            config,
            node_id,
            cluster_name,
            etcd_manager: Some(etcd_manager),
            exporter: None,
        })
    }

    async fn initialize_exporter(&mut self) -> anyhow::Result<()> {
        if let Some(etcd_manager) = &self.etcd_manager {
            let exporter = Exporter::new(
                self.node_id.clone(),
                self.cluster_name.clone(),
                Arc::clone(etcd_manager),
            );
            self.exporter = Some(exporter);
        }
        Ok(())
    }

    pub async fn update_cluster_config(&self, config: &ClusterConfig) -> anyhow::Result<()> {
        info!("Updating cluster config for: {}", config.cluster_name);
        if let Some(etcd_manager) = &self.etcd_manager {
            etcd_manager.store_cluster_config(config).await?;
        }
        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        let current_user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        
        info!("=== NoKube Service Agent Starting ===");
        info!("Node Name: {}", self.node_id);
        info!("Cluster: {}", self.cluster_name);
        info!("Operating User: {}", current_user);
        info!("=====================================");
        
        self.initialize_exporter().await?;
        if let Some(exporter) = &self.exporter {
            exporter.start_with_etcd_polling().await?;
        }
        
        // 启动监控和服务子进程
        self.start_services().await?;
        
        // 持续运行，等待关闭信号
        self.process_manager.wait_for_shutdown_signal().await;
        
        // agent关闭时清理所有子进程
        self.process_manager.cleanup_all()?;
        
        info!("Service mode agent shutdown completed");
        Ok(())
    }

    async fn start_services(&mut self) -> Result<()> {
        info!("Starting services in service mode");
        
        // 启动监控相关服务
        if self.config.task_spec.monitoring.enabled {
            self.start_monitoring_services().await?;
        }
        
        // 启动绑定的业务服务
        self.start_bound_services().await?;
        
        info!("All services started");
        Ok(())
    }

    async fn start_monitoring_services(&mut self) -> Result<()> {
        info!("Starting monitoring services");
        
        // 启动 Grafana 容器（作为子进程管理）
        if self.config.task_spec.monitoring.enabled {
            let grafana_args = vec![
                "-p".to_string(), 
                format!("{}:3000", self.config.task_spec.monitoring.grafana.port),
                "-v".to_string(),
                "/tmp/grafana.ini:/etc/grafana/grafana.ini".to_string(),
            ];
            
            self.process_manager.spawn_docker_container(
                "nokube-grafana".to_string(),
                "grafana/grafana:latest".to_string(),
                grafana_args,
            )?;
        }
        
        // 启动指标收集进程
        let mut metrics_command = Command::new("python3");
        metrics_command.args(&["-c", r#"
import time
import psutil
import json
import sys

def collect_metrics():
    while True:
        try:
            cpu_percent = psutil.cpu_percent(interval=1)
            memory = psutil.virtual_memory()
            network = psutil.net_io_counters()
            
            metrics = {
                'timestamp': int(time.time()),
                'cpu_usage': cpu_percent,
                'memory_usage': memory.percent,
                'network_rx_bytes': network.bytes_recv,
                'network_tx_bytes': network.bytes_sent
            }
            
            print(json.dumps(metrics))
            sys.stdout.flush()
            time.sleep(30)
        except Exception as e:
            print(f"Error collecting metrics: {e}", file=sys.stderr)
            time.sleep(5)

if __name__ == "__main__":
    collect_metrics()
"#]);

        self.process_manager.spawn_process("metrics-collector".to_string(), metrics_command)?;
        
        info!("Monitoring services started");
        Ok(())
    }

    async fn start_bound_services(&mut self) -> Result<()> {
        info!("Starting bound services");
        
        // Services are not part of cluster config anymore - they should be managed separately
        // This is a placeholder for future service management implementation
        
        info!("Bound services started");
        Ok(())
    }
}