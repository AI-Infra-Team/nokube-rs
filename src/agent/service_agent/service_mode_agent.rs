use crate::agent::general::process_manager::ProcessManager;
use crate::agent::general::{
    DockerRunConfig, DockerRunner, Exporter, LogCollector, LogCollectorConfig,
};
use crate::config::{
    cluster_config::ClusterConfig, config_manager::ConfigManager, etcd_manager::EtcdManager,
};
use crate::k8s::controllers::KubeController;
use crate::k8s::objects::{ContainerSpec, DaemonSetObject, DeploymentObject, NodeAffinity};
use crate::k8s::the_proxy::TheProxy;
use crate::k8s::GlobalAttributionPath;
use anyhow::Result;
use base64::Engine;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use tracing::{error, info, warn};

/// 服务模式Agent：处理持续运行的服务管理
/// 启动监控服务、管理子进程、处理关闭信号
pub struct ServiceModeAgent {
    process_manager: ProcessManager,
    config: ClusterConfig,
    node_id: String,
    cluster_name: String,
    etcd_manager: Option<Arc<EtcdManager>>,
    exporter: Option<Exporter>,
    kube_controller: Option<KubeController>,
    log_collector: Option<LogCollector>,
    the_proxy: Option<TheProxy>,
}

impl ServiceModeAgent {
    pub async fn new(
        node_id: String,
        cluster_name: String,
        etcd_endpoints: Vec<String>,
        config: ClusterConfig,
    ) -> Result<Self> {
        let etcd_manager = Arc::new(EtcdManager::new(etcd_endpoints).await?);

        // 创建 ConfigManager
        let config_manager = Arc::new(ConfigManager::new().await?);

        // 初始TheProxy
        let the_proxy = TheProxy::new(
            node_id.clone(),
            cluster_name.clone(),
            Arc::clone(&config_manager),
            5, // 按设计以 5s 间隔刷新保活
        );

        // 初始化KubeController时传入TheProxy的发送端
        // 使用集群配置中的当前节点 workspace，避免硬编码路径导致挂载校验失败
        let workspace = config
            .nodes
            .iter()
            .find(|n| n.name == node_id)
            .and_then(|n| n.workspace.clone())
            .unwrap_or_else(|| "/opt/devcon/pa/nokube-workspace".to_string());
        let _ = std::fs::create_dir_all(&workspace);
        let mut kube_controller = KubeController::new(workspace);
        kube_controller.proxy_tx = the_proxy.get_alive_sender();

        Ok(Self {
            process_manager: ProcessManager::new(),
            config,
            node_id,
            cluster_name,
            etcd_manager: Some(etcd_manager),
            exporter: None,
            kube_controller: Some(kube_controller),
            log_collector: None,
            the_proxy: Some(the_proxy),
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

    async fn initialize_log_collector(&mut self) -> anyhow::Result<()> {
        // 获取 GreptimeDB URL（从head节点配置中）
        // 解析 OTLP Logs 端点（严格要求 head 节点存在）
        let otlp_logs_endpoint = self.config.otlp_logs_endpoint()?;

        tracing::info!(
            "Init LogCollector: endpoint={}, auth_user={}, auth_password_set={}",
            otlp_logs_endpoint,
            self.config
                .task_spec
                .monitoring
                .greptimedb
                .mysql_user
                .as_deref()
                .unwrap_or("<none>"),
            self.config
                .task_spec
                .monitoring
                .greptimedb
                .mysql_password
                .as_deref()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        );

        let config = LogCollectorConfig {
            cluster_name: self.cluster_name.clone(),
            node_name: self.node_id.clone(),
            otlp_logs_endpoint,
            batch_size: 10,
            flush_interval_secs: 5,
            // 约定优于配置：固定超时 5 秒
            flush_timeout_secs: 5,
            auth_user: self
                .config
                .task_spec
                .monitoring
                .greptimedb
                .mysql_user
                .clone(),
            auth_password: self
                .config
                .task_spec
                .monitoring
                .greptimedb
                .mysql_password
                .clone(),
        };

        let mut log_collector = LogCollector::new(config)?;
        log_collector.start().await?;

        // 开始收集关键容器的日志
        log_collector.follow_docker_logs("nokube-grafana").await?;
        log_collector
            .follow_docker_logs("nokube-greptimedb")
            .await?;
        let _ = log_collector.follow_docker_logs("nokube-httpserver").await;
        // 追加：跟随已有的 actor 容器日志（前缀 nokube-pod-）
        let runtime_path =
            DockerRunner::get_runtime_path().unwrap_or_else(|_| "docker".to_string());
        if let Ok(output) = std::process::Command::new(runtime_path)
            .args(["ps", "--format", "{{.Names}}"])
            .output()
        {
            let names = String::from_utf8_lossy(&output.stdout);
            for name in names.lines() {
                if name.starts_with("nokube-pod-") {
                    let _ = log_collector.follow_docker_logs(name).await;
                }
            }
        }

        info!("Log collector initialized and started");
        self.log_collector = Some(log_collector);

        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        let current_user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());

        info!("=== NoKube Service Agent Starting ===");
        info!("Node Name: {}", self.node_id);
        info!("Cluster: {}", self.cluster_name);
        info!("Operating User: {}", current_user);
        info!("=====================================");

        // 初始化日志收集器
        if let Err(e) = self.initialize_log_collector().await {
            warn!("Failed to initialize log collector: {}", e);
        }

        // 启动TheProxy
        if let Some(ref mut the_proxy) = self.the_proxy {
            info!("Starting TheProxy...");
            the_proxy.start().await?;
            info!("TheProxy started successfully");
        }

        // 启动KubeController
        if let Some(ref mut kube_controller) = self.kube_controller {
            info!("Starting KubeController...");
            kube_controller.start().await?;
            info!("KubeController started successfully");
        }

        // 启动时先进行一次本地容器与etcd状态的对账：
        // - 清理不在etcd中的 nokube-pod-* 容器
        // - 如容器带有校验标签且与 etcd 不一致，先回收，后续将按新配置重建
        if let Err(e) = self.startup_container_reconcile().await {
            warn!("Startup container reconcile failed: {}", e);
        }

        // 从etcd加载k8s对象并应用到KubeController
        self.load_and_apply_k8s_objects().await?;

        self.initialize_exporter().await?;
        if let Some(exporter) = &self.exporter {
            exporter.start_with_etcd_polling().await?;
        }

        // 启动监控和服务子进程
        self.start_services().await?;

        // 启动k8s对象监控协程
        self.start_k8s_object_monitor().await?;

        // 持续运行，等待关闭信号
        self.process_manager.wait_for_shutdown_signal().await;

        // agent关闭时清理所有子进程
        self.process_manager.cleanup_all()?;

        // 停止KubeController
        if let Some(ref mut kube_controller) = self.kube_controller {
            info!("Stopping KubeController...");
            kube_controller.stop().await?;
            info!("KubeController stopped");
        }

        info!("Service mode agent shutdown completed");
        Ok(())
    }

    /// 启动阶段的本地容器与etcd状态对账（Deployment）
    async fn startup_container_reconcile(&self) -> Result<()> {
        if let Some(etcd_manager) = &self.etcd_manager {
            use std::collections::{HashMap, HashSet};

            // 1) 读取 etcd 中期望的 deployments 以及其 checksum
            let mut desired: HashMap<String, u64> = HashMap::new(); // deployName -> checksum
            let deployment_prefix = format!("/nokube/{}/deployments/", self.cluster_name);
            match etcd_manager.get_prefix(deployment_prefix).await {
                Ok(kvs) => {
                    for kv in kvs {
                        let key = kv.key_str().to_string();
                        if let Some(name) = key.split('/').last() {
                            let val = String::from_utf8_lossy(&kv.value);
                            desired.insert(name.to_string(), Self::calc_hash_u64(&val));
                        }
                    }
                }
                Err(e) => {
                    warn!("Reconcile: failed to load deployments from etcd: {}", e);
                }
            }

            // 2) 列出本机容器
            let runtime = crate::agent::general::DockerRunner::get_runtime_path()
                .unwrap_or_else(|_| "docker".to_string());
            let output = std::process::Command::new(&runtime)
                .args(["ps", "--format", "{{.Names}}"])
                .output();
            match output {
                Ok(out) if out.status.success() => {
                    let names = String::from_utf8_lossy(&out.stdout);
                    let mut to_remove: HashSet<String> = HashSet::new();
                    for name in names.lines() {
                        if let Some(stripped) = name.strip_prefix("nokube-pod-") {
                            // 只处理 deployment 命名；daemonset 在统一逻辑下由后续重建
                            let deploy_name = stripped;
                            if !desired.contains_key(deploy_name) {
                                // 不在etcd中，标记删除
                                to_remove.insert(name.to_string());
                                continue;
                            }
                            // 读取容器标签中的 checksum
                            let fmt = "{{ index .Config.Labels \"nokube.actor.checksum\" }}";
                            let inspect = std::process::Command::new(&runtime)
                                .args(["inspect", "--format", fmt, name])
                                .output();
                            if let Ok(ins) = inspect {
                                if ins.status.success() {
                                    let tag_val =
                                        String::from_utf8_lossy(&ins.stdout).trim().to_string();
                                    if let Ok(cur) = tag_val.parse::<u64>() {
                                        if let Some(exp) = desired.get(deploy_name) {
                                            if &cur != exp {
                                                to_remove.insert(name.to_string());
                                            }
                                        }
                                    } else {
                                        // 无标签或格式异常，保守回收
                                        to_remove.insert(name.to_string());
                                    }
                                } else {
                                    to_remove.insert(name.to_string());
                                }
                            }
                        }
                    }
                    // 执行删除
                    for n in to_remove {
                        info!("Startup reconcile: removing stale container '{}'", n);
                        let _ = crate::agent::general::DockerRunner::stop(&n);
                        let _ = crate::agent::general::DockerRunner::remove_container(&n);
                    }
                }
                Ok(out) => {
                    warn!(
                        "Reconcile: list containers failed (code={:?})",
                        out.status.code()
                    );
                }
                Err(e) => warn!("Reconcile: error listing containers: {}", e),
            }
        }
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
            // 解析 workspace（优先 head 节点 workspace）
            let workspace = if let Some(head_node) = self
                .config
                .nodes
                .iter()
                .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head))
            {
                head_node
                    .workspace
                    .clone()
                    .unwrap_or("/opt/devcon/pa/nokube-workspace".to_string())
            } else {
                "/opt/devcon/pa/nokube-workspace".to_string()
            };
            let config_dir = format!("{}/config", workspace);
            let ds_dir = format!("{}/provisioning/datasources", config_dir);
            let dash_dir = format!("{}/provisioning/dashboards", config_dir);
            let grafana_ini = format!("{}/grafana.ini", config_dir);

            // 确保目录存在（容器重启后可用）
            let _ = std::fs::create_dir_all(&ds_dir);
            let _ = std::fs::create_dir_all(&dash_dir);
            if std::fs::metadata(&grafana_ini).is_err() {
                let default_ini = "[server]\nhttp_port = 3000\n\n[security]\nadmin_user = admin\nadmin_password = admin\n\n[auth.anonymous]\nenabled = true\norg_role = Viewer\n";
                let _ = std::fs::create_dir_all(&config_dir);
                let _ = std::fs::write(&grafana_ini, default_ini);
            }

            // 如缺失，则写入数据源与仪表盘 provisioning
            let ds_yaml_path = format!("{}/nokube-datasource.yaml", ds_dir);
            if std::fs::metadata(&ds_yaml_path).is_err() {
                let head_ip = if let Some(head_node) = self
                    .config
                    .nodes
                    .iter()
                    .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head))
                {
                    head_node.get_ip().unwrap_or("127.0.0.1")
                } else {
                    "127.0.0.1"
                };
                let greptime_port = self.config.task_spec.monitoring.greptimedb.port;
                let mysql_port = greptime_port + 2;
                // 约定优于配置：仅使用集群配置中的凭证，或默认 root/无口令
                let mysql_user = self
                    .config
                    .task_spec
                    .monitoring
                    .greptimedb
                    .mysql_user
                    .clone()
                    .unwrap_or_else(|| "root".to_string());
                let mysql_pass = self
                    .config
                    .task_spec
                    .monitoring
                    .greptimedb
                    .mysql_password
                    .clone();
                let secure_block = match mysql_pass {
                    Some(ref p) if !p.is_empty() => {
                        format!("\n  secureJsonData:\n    password: {}\n", p)
                    }
                    _ => String::new(),
                };
                let ds_yaml = format!(
                    r#"apiVersion: 1
datasources:
  - name: GreptimeDB
    type: prometheus
    access: proxy
    url: http://{head}:{port}/v1/prometheus
    isDefault: true
    editable: true
  - name: greptimeplugin
    type: info8fcc-greptimedb-datasource
    access: proxy
    url: http://{head}:{port}
    isDefault: false
    editable: true
    jsonData:
      server: http://{head}:{port}
      defaultDatabase: public
  - name: greptimemysql
    type: mysql
    access: proxy
    url: {head}:{mysql_port}
    database: public
    user: {mysql_user}
    isDefault: false
    editable: true
    jsonData:
      timeInterval: 1s{secure}
"#,
                    head = head_ip,
                    port = greptime_port,
                    mysql_port = mysql_port,
                    mysql_user = mysql_user,
                    secure = secure_block
                );
                let _ = std::fs::write(&ds_yaml_path, ds_yaml);
            }

            let provider_yaml_path = format!("{}/nokube-provider.yaml", dash_dir);
            if std::fs::metadata(&provider_yaml_path).is_err() {
                let provider_yaml = r#"apiVersion: 1
providers:
  - name: 'nokube'
    orgId: 1
    type: file
    disableDeletion: false
    updateIntervalSeconds: 10
    allowUiUpdates: true
    options:
      path: /etc/grafana/provisioning/dashboards/nokube
      foldersFromFilesStructure: true
"#;
                let _ = std::fs::write(&provider_yaml_path, provider_yaml);
            }
            let dash_nokube_dir = format!("{}/nokube", dash_dir);
            let _ = std::fs::create_dir_all(&dash_nokube_dir);
            // Write a simple Home dashboard with useful links
            let head_ip = if let Some(head_node) = self
                .config
                .nodes
                .iter()
                .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head))
            {
                head_node.get_ip().unwrap_or("127.0.0.1")
            } else {
                "127.0.0.1"
            };
            let grafana_port = self.config.task_spec.monitoring.grafana.port;
            let greptime_port = self.config.task_spec.monitoring.greptimedb.port;
            let http_link = format!(
                "http://{}:{}",
                head_ip, self.config.task_spec.monitoring.httpserver.port
            );
            let home_markdown = format!(
                "# NoKube Links\\n\\n- GreptimeDB HTTP: [http://{head}:{gport}](http://{head}:{gport})\\n- Grafana: [http://{head}:{gfport}](http://{head}:{gfport})\\n- HTTP Server: [{http}]({http})",
                head=head_ip, gfport=grafana_port, gport=greptime_port, http=http_link
            );
            let home_dash_json_path = format!("{}/nokube-home.json", dash_nokube_dir);
            let home_dash = serde_json::json!({
                "id": null,
                "uid": "nokube-home",
                "title": "NoKube Home",
                "tags": ["nokube", "home", "links"],
                "timezone": "browser",
                "panels": [
                    {"id": 1, "title": "Links", "type": "text",
                     "gridPos": {"h": 6, "w": 24, "x": 0, "y": 0},
                     "options": {"mode": "markdown", "content": home_markdown}
                    }
                ],
                "time": {"from": "now-1h", "to": "now"},
                "refresh": "",
                "schemaVersion": 16,
                "version": 0
            });
            let _ = std::fs::write(
                &home_dash_json_path,
                serde_json::to_string_pretty(&home_dash).unwrap_or_else(|_| String::new()),
            );
            let mysql_dash_json = format!("{}/nokube-logs-mysql.json", dash_nokube_dir);
            // Always write dashboard JSON to ensure layout and queries are updated
            let dash_json = serde_json::json!({
                    "id": null,
                    "uid": "nokube-logs-mysql",
                    "title": "NoKube Logs (MySQL)",
                    "tags": ["nokube", "logs", "mysql", "greptimedb"],
                    "timezone": "browser",
                    "panels": [
                        {"id": 1, "title": "Log Messages (Latest)", "type": "logs", "datasource": "greptimemysql",
                         "targets": [{"format":"table","rawSql":"SELECT timestamp AS time, body AS message, severity_text AS level FROM opentelemetry_logs WHERE $__timeFilter(timestamp) AND (${container_path:sqlstring} = '' OR scope_name = ${container_path:sqlstring}) ORDER BY timestamp DESC LIMIT 1000"}],
                         "options": {"showTime": true, "showLabels": false, "showCommonLabels": false, "wrapLogMessage": false, "enableLogDetails": false, "messageField": "message"},
                         "gridPos": {"h": 12, "w": 24, "x": 0, "y": 0}},
                        {"id": 2, "title": "Log Level Distribution", "type": "piechart", "datasource": "greptimemysql",
                         "targets": [{"format":"table","rawSql":"SELECT severity_text AS metric, COUNT(*) AS value FROM opentelemetry_logs WHERE $__timeFilter(timestamp) AND (${container_path:sqlstring} = '' OR scope_name = ${container_path:sqlstring}) GROUP BY severity_text"}],
                         "gridPos": {"h": 6, "w": 8, "x": 0, "y": 12}},
                        {"id": 3, "title": "Logs per Minute", "type": "timeseries", "datasource": "greptimemysql",
                         "targets": [{"format":"time_series","rawSql":"SELECT $__timeGroup(timestamp, '1m') AS time, 'All Logs' AS metric, COUNT(*) AS value FROM opentelemetry_logs WHERE $__timeFilter(timestamp) AND (${container_path:sqlstring} = '' OR scope_name = ${container_path:sqlstring}) GROUP BY 1 ORDER BY 1"}],
                         "gridPos": {"h": 6, "w": 16, "x": 8, "y": 12}}
                    ],
                    "templating": {"list": [
                        {"name": "container_path", "type": "query", "datasource": "greptimemysql", "query": "SELECT DISTINCT scope_name AS text FROM opentelemetry_logs WHERE scope_name <> '' ORDER BY text", "refresh": 1, "includeAll": true, "allValue": "", "multi": false, "current": {"text": "", "value": ""}}
                    ]},
                    "time": {"from": "now-6h", "to": "now"},
                    "refresh": "30s",
                    "schemaVersion": 30,
                    "version": 1
            });
            let _ = std::fs::write(
                &mysql_dash_json,
                serde_json::to_string_pretty(&dash_json).unwrap_or_default(),
            );

            let grafana_args = vec![
                "-p".to_string(),
                format!("{}:3000", self.config.task_spec.monitoring.grafana.port),
                "-v".to_string(),
                format!("{}:/etc/grafana/grafana.ini", grafana_ini),
                "-v".to_string(),
                format!("{}:/etc/grafana/provisioning/datasources", ds_dir),
                "-v".to_string(),
                format!("{}:/etc/grafana/provisioning/dashboards", dash_dir),
            ];

            self.process_manager.spawn_docker_container(
                "nokube-grafana".to_string(),
                "greptime/grafana-greptimedb:latest".to_string(),
                grafana_args,
                None, // Grafana容器没有自定义启动命令
            )?;
        }

        // 启动增强的指标收集进程 - 包含k8s对象指标，并推送到GreptimeDB
        // 计算 GreptimeDB 基址（基于 head 节点 IP 与配置端口）
        let (greptime_host, greptime_http_port) = if let Some(head_node) = self
            .config
            .nodes
            .iter()
            .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head))
        {
            let host_part = head_node.ssh_url.split('@').last().unwrap_or("localhost");
            let host = if host_part.contains(':') {
                host_part.split(':').next().unwrap_or("localhost")
            } else {
                host_part
            };
            (
                host.to_string(),
                self.config.task_spec.monitoring.greptimedb.port,
            )
        } else {
            ("localhost".to_string(), 4000)
        };

        let mut metrics_command = Command::new("python3");
        metrics_command.args(&["-c", &format!(r#"
import time
import psutil
import json
import sys
import subprocess
import os
import socket
from urllib import request
from urllib.error import URLError, HTTPError

class NoKubeMetricsCollector:
    def __init__(self, node_id, cluster_name, greptime_url):
        self.node_id = node_id
        self.cluster_name = cluster_name
        self.greptime_url = greptime_url.rstrip('/')
        self.k8s_objects = {{}}
        
    def collect_system_metrics(self):
        """收集系统指标"""
        cpu_percent = psutil.cpu_percent(interval=1)
        memory = psutil.virtual_memory()
        network = psutil.net_io_counters()
        
        return {{
            'timestamp': int(time.time()),
            'cpu_usage': cpu_percent,
            'memory_usage': memory.percent,
            'network_rx_bytes': network.bytes_recv,
            'network_tx_bytes': network.bytes_sent,
            'node_id': self.node_id,
            'cluster_name': self.cluster_name
        }}
    
    def collect_k8s_object_metrics(self):
        """收集k8s对象指标（仅依据实际运行中的容器）"""
        metrics = []
        timestamp = int(time.time())

        # 读取实际运行的 nokube actor 容器
        running = []  # (name, status)
        try:
            result = subprocess.run(['docker', 'ps', '--format', 'table {{{{.Names}}}}\\t{{{{.Status}}}}'],
                                    capture_output=True, text=True, timeout=5)
            if result.returncode == 0:
                for line in result.stdout.split('\\n')[1:]:  # 跳过标题行
                    if not line.strip():
                        continue
                    parts = line.split('\\t')
                    if len(parts) < 2:
                        continue
                    name, status = parts[0], parts[1]
                    if name.startswith('nokube-pod-'):
                        running.append((name, status))
        except Exception:
            pass

        # 为每个容器推送对象信息和Pod状态
        for name, status in running:
            # 判断对象类型（deployment 或 daemonset）
            actor_name = name[len('nokube-pod-'):]
            is_daemonset = actor_name.endswith('-' + self.node_id)
            parent_type = 'daemonset' if is_daemonset else 'deployment'
            parent_name = actor_name[:-(len(self.node_id) + 1)] if is_daemonset else actor_name
            obj_status = 'Running' if 'Up' in status else 'Unknown'

            # k8s 对象信息（按pod记录）
            metrics.append({{
                'metric_name': 'nokube_k8s_object_info',
                'timestamp': timestamp,
                'value': 1,
                'labels': {{
                    'namespace': 'default',
                    'object_type': 'pod',
                    'object_name': actor_name,
                    'status': obj_status,
                    'parent_object': parent_type + '/' + parent_name,
                    'node_id': self.node_id,
                    'cluster_name': self.cluster_name,
                }}
            }})

            # Pod状态（1=Running, 0=非Running）
            metrics.append({{
                'metric_name': 'nokube_k8s_pod_status',
                'timestamp': timestamp,
                'value': 1 if 'Up' in status else 0,
                'labels': {{
                    'namespace': 'default',
                    'pod_name': actor_name,
                    'parent_daemonset': (parent_type + '/' + parent_name) if parent_type == 'daemonset' else '',
                    'parent_deployment': (parent_type + '/' + parent_name) if parent_type == 'deployment' else '',
                    'status': obj_status,
                    'node_id': self.node_id,
                    'cluster_name': self.cluster_name,
                }}
            }})

        # 实际容器资源指标由 collect_actor_metrics 收集（docker stats）
        return metrics
    
    def collect_actor_metrics(self):
        """收集actor与容器资源指标（基于 docker stats 与容器命名约定）"""
        metrics = []
        timestamp = int(time.time())

        # 获取所有容器的实时资源（一次性读取避免多次开销）
        stats = {{}}
        try:
            out = subprocess.run(
                ['docker', 'stats', '--no-stream', '--format', '{{{{.Name}}}}\t{{{{.CPUPerc}}}}\t{{{{.MemUsage}}}}\t{{{{.MemPerc}}}}'],
                capture_output=True, text=True, timeout=8)
            if out.returncode == 0:
                for line in out.stdout.split('\n'):
                    if not line.strip():
                        continue
                    parts = line.split('\t')
                    if len(parts) < 4:
                        continue
                    stats[parts[0]] = {{
                        'cpu': parts[1],  # e.g. '0.15%'
                        'mem_usage': parts[2],  # e.g. '68.8MiB / 7.68GiB'
                        'mem_pct': parts[3],  # e.g. '0.88%'
                    }}
        except Exception:
            pass

        def parse_bytes(s):
            try:
                val = s.strip().split(' ')[0]  # '68.8MiB'
                num = float(''.join([c for c in val if c.isdigit() or c=='.']))
                unit = ''.join([c for c in val if c.isalpha()])
                mul = 1.0
                if unit.lower() in ['b']:
                    mul = 1
                elif unit.lower() in ['kb', 'kib']:
                    mul = 1024
                elif unit.lower() in ['mb', 'mib']:
                    mul = 1024**2
                elif unit.lower() in ['gb', 'gib']:
                    mul = 1024**3
                elif unit.lower() in ['tb', 'tib']:
                    mul = 1024**4
                return int(num * mul)
            except Exception:
                return 0

        # 遍历当前运行中的 actor 容器
        try:
            ps = subprocess.run(['docker', 'ps', '--format', '{{{{.Names}}}}\t{{{{.Status}}}}'],
                                 capture_output=True, text=True, timeout=5)
            if ps.returncode == 0:
                for line in ps.stdout.split('\n'):
                    if not line.strip():
                        continue
                    name_status = line.split('\t')
                    if len(name_status) < 2:
                        continue
                    name, status = name_status[0], name_status[1]
                    if not name.startswith('nokube-pod-'):
                        continue
                    actor_name = name[len('nokube-pod-'):]
                    is_daemonset = actor_name.endswith('-' + self.node_id)
                    actor_type = 'daemonset' if is_daemonset else 'deployment'
                    # Actor状态（容器Up即Running）
                    metrics.append({{
                        'metric_name': 'nokube_actor_status',
                        'timestamp': timestamp,
                        'value': 1 if 'Up' in status else 0,
                        'labels': {{
                            'actor_type': actor_type,
                            'actor_name': actor_name if not is_daemonset else actor_name[:-(len(self.node_id)+1)],
                            'namespace': 'default',
                            'status': 'Running' if 'Up' in status else 'Unknown',
                            'cluster_name': self.cluster_name,
                            'node_id': self.node_id,
                        }}
                    }})

                    # 容器资源（CPU 百分比；内存字节数）
                    st = stats.get(name)
                    if st:
                        try:
                            cpu = float(st['cpu'].strip().rstrip('%'))
                        except Exception:
                            cpu = 0.0
                        mem_bytes = 0
                        try:
                            mem_usage = st['mem_usage'].split('/')[0]
                            mem_bytes = parse_bytes(mem_usage)
                        except Exception:
                            pass

                        metrics.append({{
                            'metric_name': 'nokube_container_cpu_usage',
                            'timestamp': timestamp,
                            'value': cpu,
                            'labels': {{
                                'container_name': name,
                                'namespace': 'default',
                                'object_type': 'container',
                                'node_id': self.node_id,
                                'cluster_name': self.cluster_name,
                            }}
                        }})
                        metrics.append({{
                            'metric_name': 'nokube_container_memory_usage',
                            'timestamp': timestamp,
                            'value': mem_bytes,
                            'labels': {{
                                'container_name': name,
                                'namespace': 'default',
                                'object_type': 'container',
                                'node_id': self.node_id,
                                'cluster_name': self.cluster_name,
                            }}
                        }})
                        # 同时上报内存百分比（用于百分比面板）
                        try:
                            mem_pct = float(st.get('mem_pct','0').strip().rstrip('%'))
                        except Exception:
                            mem_pct = 0.0
                        metrics.append({{
                            'metric_name': 'nokube_container_mem_percent',
                            'timestamp': timestamp,
                            'value': mem_pct,
                            'labels': {{
                                'container_name': name,
                                'namespace': 'default',
                                'object_type': 'container',
                                'node_id': self.node_id,
                                'cluster_name': self.cluster_name,
                            }}
                        }})
        except Exception:
            pass

        return metrics
    
    def to_influx_lines(self, metrics):
        lines = []
        ts = int(time.time()) * 1_000_000_000
        for m in metrics:
            name = m.get('metric_name', 'nokube_metric')
            value = m.get('value', 0)
            labels = m.get('labels', {{}}).copy()
            # normalize tag values (escape commas/spaces)
            def esc(v):
                return str(v).replace(' ', '\\ ').replace(',', '\\,')
            tag_parts = ['cluster_name=' + esc(self.cluster_name), 'node_id=' + esc(self.node_id)]
            for k, v in labels.items():
                if v is None or v == '':
                    continue
                tag_parts.append(str(k) + '=' + esc(v))
            tags = ','.join(tag_parts)
            line = str(name) + ',' + tags + ' value=' + str(value) + ' ' + str(ts)
            lines.append(line)
            # Emit compatible container metrics if applicable
            if name == 'nokube_container_cpu_usage':
                # also emit new name used elsewhere
                line2 = 'nokube_container_cpu,' + tags + ' value=' + str(value) + ' ' + str(ts)
                lines.append(line2)
            if name == 'nokube_container_memory_usage':
                # also emit bytes-aliased series for clarity
                line2 = 'nokube_container_mem_bytes,' + tags + ' value=' + str(value) + ' ' + str(ts)
                lines.append(line2)
        return "\n".join(lines)
    
    def push_influx(self, metrics):
        try:
            body = self.to_influx_lines(metrics).encode('utf-8')
            url = self.greptime_url + "/v1/influxdb/write"
            req = request.Request(url, data=body, headers={{'Content-Type': 'text/plain'}}, method='POST')
            with request.urlopen(req, timeout=5) as resp:
                if resp.status >= 300:
                    sys.stderr.write("Greptime write failed: HTTP " + str(resp.status) + "\n")
        except (HTTPError, URLError) as e:
            sys.stderr.write("Greptime write error: " + str(e) + "\n")

    def collect_and_export_metrics(self):
        """收集并导出所有指标"""
        while True:
            try:
                # 收集系统指标
                system_metrics = self.collect_system_metrics()
                print("=== System Info ===")
                print(json.dumps(system_metrics))
                
                # 收集k8s对象指标
                k8s_metrics = self.collect_k8s_object_metrics()
                print("=== K8s Objects ===")
                for metric in k8s_metrics:
                    print(json.dumps(metric))
                
                # 收集actor指标
                actor_metrics = self.collect_actor_metrics()
                print("=== Actor Metrics ===")
                for metric in actor_metrics:
                    print(json.dumps(metric))
                
                # 推送到 GreptimeDB (InfluxDB line protocol)
                all_metrics = []
                all_metrics.extend(k8s_metrics)
                all_metrics.extend(actor_metrics)
                if all_metrics:
                    self.push_influx(all_metrics)
                
                sys.stdout.flush()
                time.sleep(15)
                
            except Exception as e:
                print(f"Error collecting metrics: {{{{e}}}}", file=sys.stderr)
                time.sleep(5)

def main():
    node_id = "{}"
    cluster_name = "{}"
    greptime_url = "{}"
    collector = NoKubeMetricsCollector(node_id, cluster_name, greptime_url)
    collector.collect_and_export_metrics()

if __name__ == "__main__":
    main()
"#, self.node_id, self.cluster_name, format!("http://{}:{}", greptime_host, greptime_http_port))]);

        self.process_manager
            .spawn_process("metrics-collector".to_string(), metrics_command)?;

        info!("Enhanced monitoring services started with k8s metrics collection");
        Ok(())
    }

    async fn start_bound_services(&mut self) -> Result<()> {
        info!("Starting bound services");
        // HTTP file server (head node only, always enabled)
        let http_port = self.config.task_spec.monitoring.httpserver.port;
        if let Some(head) = self
            .config
            .nodes
            .iter()
            .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head))
        {
            let head_name = &head.name;
            if &self.node_id == head_name {
                // Determine workspace and fixed mount path
                let workspace = head
                    .workspace
                    .clone()
                    .unwrap_or_else(|| "/opt/devcon/pa/nokube-workspace".to_string());
                let host_dir = format!(
                    "{}/{}",
                    workspace,
                    crate::config::cluster_config::HTTP_SERVER_MOUNT_SUBPATH
                );
                // Ensure host dir exists
                let _ = std::fs::create_dir_all(&host_dir);

                let container_name = "nokube-httpserver";
                let image = "python:3.10-slim".to_string();
                let run = DockerRunConfig::new(container_name.to_string(), image)
                    .add_volume(host_dir.clone(), "/srv/http".to_string(), true)
                    .add_port(http_port, 8080)
                    .command(vec![
                        "python".to_string(),
                        "-m".to_string(),
                        "http.server".to_string(),
                        "8080".to_string(),
                        "--directory".to_string(),
                        "/srv/http".to_string(),
                    ]);
                // Try to run
                match DockerRunner::run(&run) {
                    Ok(id) => {
                        info!("Started HTTP server '{}' (container id: {}), serving {} on /srv/http, port {}->8080", container_name, id, host_dir, http_port);
                        info!("HTTP artifacts path (host): {}", host_dir);
                        info!(
                            "HTTP server URL: http://{}:{}",
                            head.get_ip().unwrap_or("127.0.0.1"),
                            http_port
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Failed to start HTTP server container '{}': {}",
                            container_name, e
                        );
                    }
                }
            } else {
                info!(
                    "HTTP server runs only on head node: {} (current={})",
                    head_name, self.node_id
                );
            }
        }

        info!("Bound services started");
        Ok(())
    }

    /// 从etcd加载k8s对象并应用到KubeController
    async fn load_and_apply_k8s_objects(&mut self) -> Result<()> {
        info!("Loading k8s objects from etcd...");

        // 首先收集所有需要创建的对象
        let mut deployments_to_create = Vec::new();
        let mut daemonsets_to_create = Vec::new();

        if let Some(etcd_manager) = &self.etcd_manager {
            // 查找所有deployments
            let deployment_prefix = format!("/nokube/{}/deployments/", self.cluster_name);
            match etcd_manager.get_prefix(deployment_prefix).await {
                Ok(deployment_kvs) => {
                    info!("Found {} deployments in etcd", deployment_kvs.len());

                    for kv in deployment_kvs {
                        let key_str = String::from_utf8_lossy(&kv.key);
                        let deployment_name = key_str.split('/').last().unwrap_or("unknown");

                        // 解析deployment YAML配置
                        let value_str = String::from_utf8_lossy(&kv.value);
                        match serde_yaml::from_str::<serde_yaml::Value>(&value_str) {
                            Ok(deployment_yaml) => {
                                deployments_to_create
                                    .push((deployment_yaml, deployment_name.to_string()));
                            }
                            Err(e) => {
                                error!(
                                    "Failed to parse deployment YAML for {}: {}",
                                    deployment_name, e
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to get deployments from etcd: {}", e);
                }
            }

            // 查找所有daemonsets
            let daemonset_prefix = format!("/nokube/{}/daemonsets/", self.cluster_name);
            match etcd_manager.get_prefix(daemonset_prefix).await {
                Ok(daemonset_kvs) => {
                    info!("Found {} daemonsets in etcd", daemonset_kvs.len());

                    for kv in daemonset_kvs {
                        let key_str = String::from_utf8_lossy(&kv.key);
                        let daemonset_name = key_str.split('/').last().unwrap_or("unknown");

                        // 解析daemonset YAML配置
                        let value_str = String::from_utf8_lossy(&kv.value);
                        match serde_yaml::from_str::<serde_yaml::Value>(&value_str) {
                            Ok(daemonset_yaml) => {
                                daemonsets_to_create
                                    .push((daemonset_yaml, daemonset_name.to_string()));
                            }
                            Err(e) => {
                                error!(
                                    "Failed to parse daemonset YAML for {}: {}",
                                    daemonset_name, e
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to get daemonsets from etcd: {}", e);
                }
            }
        }

        // 现在创建所有对象
        for (deployment_yaml, deployment_name) in deployments_to_create {
            info!("Processing deployment: {}", deployment_name);
            if let Err(e) = self
                .create_deployment_from_yaml(deployment_yaml, &deployment_name)
                .await
            {
                error!("Failed to create deployment {}: {}", deployment_name, e);
            }
        }

        for (daemonset_yaml, daemonset_name) in daemonsets_to_create {
            info!("Processing daemonset: {}", daemonset_name);
            if let Err(e) = self
                .create_daemonset_from_yaml(daemonset_yaml, &daemonset_name)
                .await
            {
                error!("Failed to create daemonset {}: {}", daemonset_name, e);
            }
        }

        info!("Completed loading k8s objects from etcd");
        Ok(())
    }

    /// 创建Deployment对象 - 使用统一的容器创建方法
    async fn create_deployment_from_yaml(
        &mut self,
        deployment_yaml: serde_yaml::Value,
        deployment_name: &str,
    ) -> Result<()> {
        info!(
            "Creating deployment (using unified method): {}",
            deployment_name
        );

        // 直接使用统一的容器创建方法
        let workspace = self
            .config
            .nodes
            .iter()
            .find(|n| n.name == self.node_id)
            .and_then(|n| n.workspace.clone())
            .unwrap_or_else(|| "/opt/devcon/pa/nokube-workspace".to_string());
        let _ = std::fs::create_dir_all(&workspace);

        match Self::create_deployment_container_unified(
            &deployment_yaml,
            deployment_name,
            &self.cluster_name,
            self.etcd_manager.as_ref().unwrap(),
            &workspace,
        )
        .await
        {
            Ok(_) => {
                info!(
                    "Successfully created deployment container: {}",
                    deployment_name
                );
                // 跟随该部署容器的日志到 GreptimeDB（actor 面板）
                if let Some(ref log_collector) = self.log_collector {
                    let container_name = format!("nokube-pod-{}", deployment_name);
                    if let Err(e) = log_collector.follow_docker_logs(&container_name).await {
                        warn!(
                            "Failed to start following logs for {}: {}",
                            container_name, e
                        );
                    } else {
                        info!(
                            "Started log following for actor container: {}",
                            container_name
                        );
                    }
                }

                // TODO: 如果需要K8s对象管理，可以在这里添加
                // 目前重点是确保容器能正确启动并挂载ConfigMap
            }
            Err(e) => {
                error!("Failed to create deployment {}: {}", deployment_name, e);
            }
        }

        Ok(())
    }

    /// 创建DaemonSet对象
    async fn create_daemonset_from_yaml(
        &mut self,
        daemonset_yaml: serde_yaml::Value,
        daemonset_name: &str,
    ) -> Result<()> {
        info!("Creating daemonset: {}", daemonset_name);

        // 解析daemonset配置（类似于deployment但每个节点只有一个实例）
        let spec = daemonset_yaml
            .get("spec")
            .ok_or_else(|| anyhow::anyhow!("Missing spec in daemonset"))?;
        let template = spec
            .get("template")
            .ok_or_else(|| anyhow::anyhow!("Missing template in daemonset spec"))?;
        let template_spec = template
            .get("spec")
            .ok_or_else(|| anyhow::anyhow!("Missing spec in template"))?;
        let containers = template_spec
            .get("containers")
            .ok_or_else(|| anyhow::anyhow!("Missing containers in template spec"))?;

        if let Some(container_array) = containers.as_sequence() {
            if let Some(container) = container_array.first() {
                let container_name = container
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(daemonset_name);
                let image = container
                    .get("image")
                    .and_then(|v| v.as_str())
                    .unwrap_or("python:3.10-slim");

                let command = container
                    .get("command")
                    .and_then(|v| v.as_sequence())
                    .map(|seq| {
                        seq.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    });
                let args = container
                    .get("args")
                    .and_then(|v| v.as_sequence())
                    .map(|seq| {
                        seq.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    });

                let mut env = HashMap::new();
                if let Some(env_obj) = container.get("env") {
                    if let Some(env_map) = env_obj.as_mapping() {
                        for (k, v) in env_map {
                            if let (Some(key), Some(value)) = (k.as_str(), v.as_str()) {
                                env.insert(key.to_string(), value.to_string());
                            }
                        }
                    }
                }

                let container_spec = ContainerSpec {
                    name: container_name.to_string(),
                    image: image.to_string(),
                    command,
                    args,
                    env: if env.is_empty() { None } else { Some(env) },
                    volume_mounts: None,
                };

                let node_affinity = NodeAffinity {
                    required: vec![],
                    preferred: vec![],
                };

                let attribution_path =
                    GlobalAttributionPath::new(format!("daemonset/{}", daemonset_name));
                let workspace = self
                    .config
                    .nodes
                    .iter()
                    .find(|n| n.name == self.node_id)
                    .and_then(|n| n.workspace.clone())
                    .unwrap_or_else(|| "/opt/devcon/pa/nokube-workspace".to_string());
                let _ = std::fs::create_dir_all(&workspace);

                if let Some(ref kube_controller) = self.kube_controller {
                    let proxy_tx = kube_controller.proxy_tx.clone();
                    // Clone spec for daemonset object; keep local for env injection below
                    let ds_container_spec = container_spec.clone();
                    let daemonset_obj = DaemonSetObject::new(
                        daemonset_name.to_string(),
                        "default".to_string(),
                        attribution_path,
                        node_affinity,
                        ds_container_spec,
                        workspace,
                        self.cluster_name.clone(), // 添加 cluster_name
                        proxy_tx,
                        Arc::new(ConfigManager::new().await?), // 创建临时 ConfigManager
                    );

                    kube_controller.add_daemonset(daemonset_obj).await?;
                    info!("Successfully created daemonset: {}", daemonset_name);
                }

                // 启动本节点的 DaemonSet 容器（容器是最小执行粒度，用于度量与面板）
                // 采用统一的 DockerRunner 来创建一个以 daemonset + node 命名的容器
                // 容器名: nokube-pod-<daemonset_name>-<node>
                let ds_container_name = format!("nokube-pod-{}-{}", daemonset_name, self.node_id);
                let mut run_cfg =
                    DockerRunConfig::new(ds_container_name.clone(), image.to_string())
                        .restart_policy("unless-stopped".to_string());

                // 注入环境变量
                if let Some(env_map) = &container_spec.env {
                    for (k, v) in env_map {
                        run_cfg = run_cfg.add_env(k.clone(), v.clone());
                    }
                }

                // 传递代理相关环境（若外界已设置）
                for key in [
                    "http_proxy",
                    "https_proxy",
                    "no_proxy",
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "NO_PROXY",
                ] {
                    if let Ok(val) = std::env::var(key) {
                        if !val.is_empty() {
                            run_cfg = run_cfg.add_env(key.to_string(), val);
                        }
                    }
                }

                match self
                    .process_manager
                    .spawn_docker_container_with_config(run_cfg)
                {
                    Ok(cid) => {
                        info!(
                            "DaemonSet '{}' container started on node {}: {}",
                            daemonset_name, self.node_id, cid
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Failed to start DaemonSet '{}' container on node {}: {}",
                            daemonset_name, self.node_id, e
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// 存储pod状态和错误信息到etcd
    async fn store_pod_status(
        &self,
        pod_name: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> anyhow::Result<()> {
        if let Some(etcd_manager) = &self.etcd_manager {
            let pod_key = format!("/nokube/{}/pods/{}", self.cluster_name, pod_name);

            let mut pod_info = serde_json::json!({
                "name": pod_name,
                "namespace": "default",
                "node": self.node_id,
                "image": "python:3.10-slim", // 从container_spec获取
                "container_id": serde_json::Value::Null,
                "status": status,
                "ready": status == "Running",
                "restart_count": 0,
                "start_time": chrono::Utc::now().to_rfc3339(),
                "pod_ip": if status == "Running" { serde_json::Value::String("172.17.0.5".to_string()) } else { serde_json::Value::Null },
                "labels": {
                    "app": pod_name,
                    "component": "pod"
                },
                "ports": ["8080/TCP"],
                "priority": 0
            });

            // 如果有错误信息，添加到pod信息中
            if let Some(error) = error_message {
                pod_info["error_message"] = serde_json::Value::String(error.to_string());
            }

            etcd_manager.put(pod_key, pod_info.to_string()).await?;

            // 同时存储事件信息
            self.store_pod_events(pod_name, status, error_message)
                .await?;
        }

        Ok(())
    }

    /// 存储pod事件信息到etcd
    async fn store_pod_events(
        &self,
        pod_name: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> anyhow::Result<()> {
        if let Some(etcd_manager) = &self.etcd_manager {
            let events_key = format!("/nokube/{}/events/pod/{}", self.cluster_name, pod_name);

            let events = match status {
                "Running" => vec![
                    serde_json::json!({
                        "type": "Normal",
                        "reason": "Scheduled",
                        "message": format!("Successfully assigned default/{} to {}", pod_name, self.node_id),
                        "age": "2m",
                        "timestamp": chrono::Utc::now().to_rfc3339()
                    }),
                    serde_json::json!({
                        "type": "Normal",
                        "reason": "Pulled",
                        "message": "Successfully pulled image \"python:3.10-slim\"",
                        "age": "1m",
                        "timestamp": chrono::Utc::now().to_rfc3339()
                    }),
                    serde_json::json!({
                        "type": "Normal",
                        "reason": "Created",
                        "message": format!("Created container {}", pod_name),
                        "age": "1m",
                        "timestamp": chrono::Utc::now().to_rfc3339()
                    }),
                    serde_json::json!({
                        "type": "Normal",
                        "reason": "Started",
                        "message": format!("Started container {}", pod_name),
                        "age": "1m",
                        "timestamp": chrono::Utc::now().to_rfc3339()
                    }),
                ],
                "Failed" => {
                    let mut events = vec![
                        serde_json::json!({
                            "type": "Normal",
                            "reason": "Scheduled",
                            "message": format!("Successfully assigned default/{} to {}", pod_name, self.node_id),
                            "age": "2m",
                            "timestamp": chrono::Utc::now().to_rfc3339()
                        }),
                        serde_json::json!({
                            "type": "Normal",
                            "reason": "Pulling",
                            "message": "Pulling image \"python:3.10-slim\"",
                            "age": "1m",
                            "timestamp": chrono::Utc::now().to_rfc3339()
                        }),
                    ];

                    if let Some(error) = error_message {
                        events.push(serde_json::json!({
                            "type": "Warning",
                            "reason": "Failed",
                            "message": format!("Container creation failed: {}", error),
                            "age": "30s",
                            "timestamp": chrono::Utc::now().to_rfc3339()
                        }));
                    }

                    events
                }
                _ => vec![serde_json::json!({
                    "type": "Normal",
                    "reason": "Scheduled",
                    "message": format!("Successfully assigned default/{} to {}", pod_name, self.node_id),
                    "age": "1m",
                    "timestamp": chrono::Utc::now().to_rfc3339()
                })],
            };

            etcd_manager
                .put(events_key, serde_json::Value::Array(events).to_string())
                .await?;
        }

        Ok(())
    }

    /// 启动k8s对象监控协程
    async fn start_k8s_object_monitor(&mut self) -> Result<()> {
        if let Some(etcd_manager) = &self.etcd_manager {
            use std::collections::HashSet;

            let etcd_manager_clone = Arc::clone(etcd_manager);
            let cluster_name = self.cluster_name.clone();
            let _log_collector_clone = if let Some(ref log_collector) = self.log_collector {
                Some(log_collector.get_log_sender())
            } else {
                None
            };

            tokio::spawn(async move {
                use std::collections::HashMap;
                let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
                let mut seen_deploy_checksums: HashMap<String, u64> = HashMap::new();
                let mut seen_daemon_checksums: HashMap<String, u64> = HashMap::new();

                loop {
                    interval.tick().await;
                    info!(
                        "Checking for new deployments/daemonsets in cluster: {}",
                        cluster_name
                    );

                    // 检查 deployments
                    let deployment_prefix = format!("/nokube/{}/deployments/", cluster_name);
                    match etcd_manager_clone
                        .get_prefix(deployment_prefix.clone())
                        .await
                    {
                        Ok(deployment_kvs) => {
                            info!(
                                "Deployment monitor: total keys={} (seen={})",
                                deployment_kvs.len(),
                                seen_deploy_checksums.len()
                            );
                            // 收集当前存在的 deployment 及其校验和
                            let mut current_map: HashMap<String, u64> = HashMap::new();
                            for kv in &deployment_kvs {
                                let key = String::from_utf8_lossy(&kv.key).to_string();
                                let val = String::from_utf8_lossy(&kv.value);
                                let csum = Self::calc_hash_u64(&val);
                                current_map.insert(key, csum);
                            }

                            // 处理新增的 deployments
                            for kv in deployment_kvs {
                                let key_str = String::from_utf8_lossy(&kv.key).to_string();
                                let value_str = String::from_utf8_lossy(&kv.value);
                                let checksum = Self::calc_hash_u64(&value_str);
                                if !seen_deploy_checksums.contains_key(&key_str) {
                                    let deployment_name =
                                        key_str.split('/').last().unwrap_or("unknown");
                                    match serde_yaml::from_str::<serde_yaml::Value>(&value_str) {
                                        Ok(deployment_yaml) => {
                                            info!(
                                                "Processing new deployment: {} (key={})",
                                                deployment_name, key_str
                                            );
                                            // 计算当前节点的工作目录以挂载（避免校验失败）
                                            let workspace = if let Ok(cfg_mgr) =
                                                ConfigManager::new().await
                                            {
                                                if let Ok(Some(cfg)) =
                                                    cfg_mgr.get_cluster_config(&cluster_name).await
                                                {
                                                    // 优先使用 Head 节点 workspace，其次用第一个节点 workspace，最后回退默认路径
                                                    if let Some(head) = cfg.nodes.iter().find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head)) {
                                                        head.workspace.clone().unwrap_or_else(|| "/opt/devcon/pa/nokube-workspace".to_string())
                                                    } else {
                                                        cfg.nodes
                                                            .first()
                                                            .and_then(|n| n.workspace.clone())
                                                            .unwrap_or_else(|| "/opt/devcon/pa/nokube-workspace".to_string())
                                                    }
                                                } else {
                                                    "/opt/devcon/pa/nokube-workspace".to_string()
                                                }
                                            } else {
                                                "/opt/devcon/pa/nokube-workspace".to_string()
                                            };
                                            let _ = std::fs::create_dir_all(&workspace);

                                            if let Err(e) =
                                                Self::create_deployment_container_unified(
                                                    &deployment_yaml,
                                                    deployment_name,
                                                    &cluster_name,
                                                    &etcd_manager_clone,
                                                    &workspace,
                                                )
                                                .await
                                            {
                                                error!(
                                                    "Failed to create deployment {}: {}",
                                                    deployment_name, e
                                                );
                                            } else {
                                                seen_deploy_checksums.insert(key_str, checksum);
                                            }
                                        }
                                        Err(e) => {
                                            error!(
                                                "Failed to parse deployment YAML for {}: {}",
                                                deployment_name, e
                                            );
                                        }
                                    }
                                } else {
                                    // 已存在：检查校验和是否变化
                                    if let Some(prev) = seen_deploy_checksums.get(&key_str) {
                                        if *prev != checksum {
                                            let deployment_name =
                                                key_str.split('/').last().unwrap_or("unknown");
                                            info!("🔁 Detected deployment update: {} (key={}), restarting", deployment_name, key_str);
                                            let container_name =
                                                format!("nokube-pod-{}", deployment_name);
                                            // 停旧容器
                                            if let Err(e) =
                                                crate::agent::general::DockerRunner::stop(
                                                    &container_name,
                                                )
                                            {
                                                tracing::warn!(
                                                    "Failed to stop container {}: {}",
                                                    container_name,
                                                    e
                                                );
                                            }
                                            if let Err(e) = crate::agent::general::DockerRunner::remove_container(&container_name) {
                                                tracing::warn!("Failed to remove container {}: {}", container_name, e);
                                            }
                                            // 重建
                                            match serde_yaml::from_str::<serde_yaml::Value>(
                                                &value_str,
                                            ) {
                                                Ok(deployment_yaml) => {
                                                    // 计算当前节点的工作目录
                                                    let workspace = if let Ok(cfg_mgr) =
                                                        ConfigManager::new().await
                                                    {
                                                        if let Ok(Some(cfg)) = cfg_mgr
                                                            .get_cluster_config(&cluster_name)
                                                            .await
                                                        {
                                                            if let Some(head) = cfg.nodes.iter().find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head)) {
                                                                head.workspace.clone().unwrap_or_else(|| "/opt/devcon/pa/nokube-workspace".to_string())
                                                            } else {
                                                                cfg.nodes.first().and_then(|n| n.workspace.clone()).unwrap_or_else(|| "/opt/devcon/pa/nokube-workspace".to_string())
                                                            }
                                                        } else {
                                                            "/opt/devcon/pa/nokube-workspace"
                                                                .to_string()
                                                        }
                                                    } else {
                                                        "/opt/devcon/pa/nokube-workspace"
                                                            .to_string()
                                                    };
                                                    let _ = std::fs::create_dir_all(&workspace);
                                                    if let Err(e) =
                                                        Self::create_deployment_container_unified(
                                                            &deployment_yaml,
                                                            deployment_name,
                                                            &cluster_name,
                                                            &etcd_manager_clone,
                                                            &workspace,
                                                        )
                                                        .await
                                                    {
                                                        error!(
                                                            "Failed to recreate deployment {}: {}",
                                                            deployment_name, e
                                                        );
                                                    } else {
                                                        seen_deploy_checksums
                                                            .insert(key_str.clone(), checksum);
                                                    }
                                                }
                                                Err(e) => {
                                                    error!("Failed to parse updated deployment YAML for {}: {}", deployment_name, e);
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // 处理已删除的 deployments：具体销毁交由 KubeController 的孤儿回收调度。

                            // 用当前 map 覆盖已见集合，保持与etcd一致
                            seen_deploy_checksums = current_map;
                        }
                        Err(e) => {
                            warn!("Failed to check deployments: {}", e);
                        }
                    }

                    // 检查 daemonsets
                    let daemonset_prefix = format!("/nokube/{}/daemonsets/", cluster_name);
                    match etcd_manager_clone.get_prefix(daemonset_prefix).await {
                        Ok(daemonset_kvs) => {
                            info!(
                                "DaemonSet monitor: total keys={} (seen={})",
                                daemonset_kvs.len(),
                                seen_daemon_checksums.len()
                            );
                            // 构建当前 map
                            let mut current_map: HashMap<String, u64> = HashMap::new();
                            for kv in &daemonset_kvs {
                                let key = String::from_utf8_lossy(&kv.key).to_string();
                                let val = String::from_utf8_lossy(&kv.value);
                                current_map.insert(key, Self::calc_hash_u64(&val));
                            }
                            for kv in daemonset_kvs {
                                let key_str = String::from_utf8_lossy(&kv.key).to_string();
                                let val = String::from_utf8_lossy(&kv.value);
                                let checksum = Self::calc_hash_u64(&val);
                                if !seen_daemon_checksums.contains_key(&key_str) {
                                    let daemonset_name =
                                        key_str.split('/').last().unwrap_or("unknown");
                                    match serde_yaml::from_str::<serde_yaml::Value>(&val) {
                                        Ok(daemonset_yaml) => {
                                            info!(
                                                "Processing new daemonset: {} (key={})",
                                                daemonset_name, key_str
                                            );
                                            // 后续可添加统一创建逻辑（目前仅记录校验和）
                                            seen_daemon_checksums.insert(key_str, checksum);
                                        }
                                        Err(e) => {
                                            error!(
                                                "Failed to parse daemonset YAML for {}: {}",
                                                daemonset_name, e
                                            );
                                        }
                                    }
                                } else {
                                    // update 检测占位（若需要同部署一样重启）
                                    if let Some(prev) = seen_daemon_checksums.get(&key_str) {
                                        if *prev != checksum {
                                            let daemonset_name =
                                                key_str.split('/').last().unwrap_or("unknown");
                                            info!("🔁 Detected daemonset update: {} (key={}), will recreate local ds container", daemonset_name, key_str);
                                            let ds_container_name = format!(
                                                "nokube-pod-{}-{}",
                                                daemonset_name, "${NODE}"
                                            );
                                            // 由于当前节点名不可用于此处（闭包内），仅更新校验和，实际容器重建在 create_daemonset_from_yaml 中处理
                                            seen_daemon_checksums.insert(key_str.clone(), checksum);
                                        }
                                    }
                                }
                            }
                            // 移除检测
                            let removed: Vec<String> = seen_daemon_checksums
                                .keys()
                                .filter(|k| !current_map.contains_key(*k))
                                .cloned()
                                .collect();
                            for key in removed {
                                seen_daemon_checksums.remove(&key);
                            }
                        }
                        Err(e) => {
                            warn!("Failed to check daemonsets: {}", e);
                        }
                    }
                }
            });
        }
        Ok(())
    }

    /// 计算简单的稳定哈希（用于 YAML 变更检测）
    fn calc_hash_u64(s: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish()
    }

    /// 统一的deployment容器创建方法 - 使用最新的DockerRunner
    async fn create_deployment_container_unified(
        deployment_yaml: &serde_yaml::Value,
        deployment_name: &str,
        cluster_name: &str,
        etcd_manager: &Arc<EtcdManager>,
        workspace: &str,
    ) -> Result<()> {
        info!(
            "Creating deployment container (unified): {}",
            deployment_name
        );

        // 解析deployment配置
        let spec = deployment_yaml
            .get("spec")
            .ok_or_else(|| anyhow::anyhow!("Missing spec in deployment"))?;

        // Debug: 打印整个YAML结构
        info!(
            "🔍 Debug: Full YAML structure: {}",
            serde_json::to_string_pretty(&deployment_yaml)
                .unwrap_or_else(|_| "Failed to serialize".to_string())
        );
        info!(
            "🔍 Debug: Spec keys: {:?}",
            spec.as_mapping().map(|m| m.keys().collect::<Vec<_>>())
        );

        if let Some(template) = spec.get("template") {
            info!(
                "🔍 Debug: Template found, keys: {:?}",
                template.as_mapping().map(|m| m.keys().collect::<Vec<_>>())
            );
        }

        // 检查是否是GitOps类型的部署 (包含configMap字段)
        let configmap_data = spec.get("configMap").and_then(|cm| cm.get("data"));

        // 解析spec结构 - 区分标准K8s Deployment和GitOpsCluster格式
        let deployment_spec = if spec.get("template").is_some() {
            // 标准Kubernetes Deployment格式: 直接使用spec
            info!("🎯 Processing standard K8s Deployment format");
            spec
        } else if let Some(deploy) = spec.get("deployment") {
            // GitOpsCluster格式: deployment
            info!("🎯 Processing GitOpsCluster deployment format");
            deploy
        } else {
            return Err(anyhow::anyhow!("Missing template or deployment in spec"));
        };

        // 提取containerSpec - 需要支持多种YAML格式
        let container_spec = if let Some(template) = deployment_spec.get("template") {
            // 标准Kubernetes格式: template.spec.containers[0]
            if let Some(template_spec) = template.get("spec") {
                if let Some(containers) = template_spec
                    .get("containers")
                    .and_then(|c| c.as_sequence())
                {
                    containers
                        .first()
                        .ok_or_else(|| anyhow::anyhow!("Empty containers array in template.spec"))?
                } else {
                    return Err(anyhow::anyhow!("Missing containers in template.spec"));
                }
            }
            // NoKube自定义格式: template.containerSpec
            else if let Some(container_spec) = template.get("containerSpec") {
                container_spec
            } else {
                return Err(anyhow::anyhow!(
                    "Missing containerSpec or spec.containers in template"
                ));
            }
        } else {
            // 直接在deployment_spec下查找containerSpec
            deployment_spec
                .get("containerSpec")
                .ok_or_else(|| anyhow::anyhow!("Missing containerSpec in deployment"))?
        };

        let image = container_spec
            .get("image")
            .and_then(|v| v.as_str())
            .unwrap_or("python:3.10-slim");

        // 提取command和args
        let command: Option<Vec<String>> = container_spec
            .get("command")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            });
        let args: Option<Vec<String>> = container_spec
            .get("args")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            });

        // 提取环境变量 - 支持多种格式
        let mut env = HashMap::new();
        if let Some(env_obj) = container_spec.get("env") {
            // Kubernetes格式: 直接是对象 {"KEY": "VALUE"}
            if let Some(env_map) = env_obj.as_mapping() {
                for (k, v) in env_map {
                    if let (Some(key), Some(value)) = (k.as_str(), v.as_str()) {
                        env.insert(key.to_string(), value.to_string());
                    }
                }
            }
            // Kubernetes标准格式: 数组 [{"name": "KEY", "value": "VALUE"}]
            else if let Some(env_array) = env_obj.as_sequence() {
                for env_item in env_array {
                    if let (Some(name), Some(value)) = (
                        env_item.get("name").and_then(|v| v.as_str()),
                        env_item.get("value").and_then(|v| v.as_str()),
                    ) {
                        env.insert(name.to_string(), value.to_string());
                    }
                }
            }
        }

        // 创建Docker容器使用新的DockerRunner
        let container_name = format!("nokube-pod-{}", deployment_name);
        let workspace = workspace.to_string();

        // 使用新的 DockerRunConfig 构建配置
        let mut config = DockerRunConfig::new(container_name.clone(), image.to_string());

        // 添加基础挂载
        config = config.add_volume(workspace.clone(), "/pod-workspace".to_string(), false);

        // 不再处理volumeMounts - 所有volume挂载由下面的标准K8s volumes处理逻辑统一处理
        // 这避免了重复挂载同一路径的问题

        // 旧的自定义configMap处理已移除，现在统一使用标准Kubernetes volumes处理

        // 跟踪已使用的挂载路径，防止重复挂载
        let mut used_mount_paths = std::collections::HashSet::new();

        // 处理标准Kubernetes volumes定义 (支持标准K8s Deployment格式)
        if let Some(template) = deployment_spec.get("template") {
            if let Some(template_spec) = template.get("spec") {
                if let Some(volumes) = template_spec.get("volumes").and_then(|v| v.as_sequence()) {
                    info!("🔍 Processing {} standard K8s volumes", volumes.len());

                    for volume in volumes {
                        if let Some(volume_name) = volume.get("name").and_then(|n| n.as_str()) {
                            // 处理ConfigMap类型的volume
                            if let Some(configmap_ref) = volume.get("configMap") {
                                if let Some(configmap_name) =
                                    configmap_ref.get("name").and_then(|n| n.as_str())
                                {
                                    info!(
                                        "📦 Processing ConfigMap volume: {} -> {}",
                                        volume_name, configmap_name
                                    );

                                    // 创建ConfigMap目录（即使未找到数据也创建空目录并挂载）
                                    let volume_config_dir =
                                        format!("{}/configmaps/{}", workspace, configmap_name);
                                    std::fs::create_dir_all(&volume_config_dir).map_err(|e| {
                                        anyhow::anyhow!(
                                            "Failed to create ConfigMap volume directory {}: {}",
                                            volume_config_dir,
                                            e
                                        )
                                    })?;
                                    info!(
                                        "📁 Prepared ConfigMap volume directory: {}",
                                        volume_config_dir
                                    );

                                    // 尝试从etcd加载ConfigMap数据并写入文件
                                    match Self::load_configmap_from_etcd(
                                        etcd_manager,
                                        cluster_name,
                                        configmap_name,
                                    )
                                    .await
                                    {
                                        Ok(Some(configmap_data)) => {
                                            if let Some(data_map) = configmap_data.as_mapping() {
                                                info!(
                                                    "📝 ConfigMap '{}' has {} entries",
                                                    configmap_name,
                                                    data_map.len()
                                                );
                                                for (filename, content) in data_map {
                                                    if let Some(name) = filename.as_str() {
                                                        if let Some(data) = content.as_str() {
                                                            let file_path = format!(
                                                                "{}/{}",
                                                                volume_config_dir, name
                                                            );
                                                            std::fs::write(&file_path, data).map_err(|e| {
                                                                anyhow::anyhow!("Failed to write ConfigMap volume file {}: {}", file_path, e)
                                                            })?;
                                                            info!("✅ Created ConfigMap volume file: {} ({} bytes)", file_path, data.len());
                                                        } else {
                                                            warn!("⚠️  ConfigMap file '{}' content is not a string: {:?}", name, content);
                                                        }
                                                    } else {
                                                        warn!("⚠️  ConfigMap filename is not a string: {:?}", filename);
                                                    }
                                                }
                                            } else {
                                                warn!("⚠️  ConfigMap '{}' data is not a mapping: {:?}", configmap_name, configmap_data);
                                            }
                                        }
                                        Ok(None) => {
                                            warn!("⚠️  ConfigMap '{}' not found in etcd, mounting empty directory", configmap_name);
                                        }
                                        Err(e) => {
                                            error!(
                                                "❌ Failed to load ConfigMap '{}' from etcd: {}",
                                                configmap_name, e
                                            );
                                            // 继续挂载空目录
                                        }
                                    }

                                    // 找到对应的volumeMount并添加到Docker配置（即使没有数据也挂载空目录）
                                    if let Some(volume_mounts) = container_spec
                                        .get("volumeMounts")
                                        .and_then(|vm| vm.as_sequence())
                                    {
                                        for volume_mount in volume_mounts {
                                            if let (Some(mount_name), Some(mount_path)) = (
                                                volume_mount.get("name").and_then(|v| v.as_str()),
                                                volume_mount
                                                    .get("mountPath")
                                                    .and_then(|v| v.as_str()),
                                            ) {
                                                if mount_name == volume_name {
                                                    if used_mount_paths.contains(mount_path) {
                                                        warn!("⚠️  Skipping duplicate mount to path '{}' for volume '{}'", mount_path, volume_name);
                                                        continue;
                                                    }
                                                    let read_only = volume_mount
                                                        .get("readOnly")
                                                        .and_then(|v| v.as_bool())
                                                        .unwrap_or(false);
                                                    config = config.add_volume(
                                                        volume_config_dir.clone(),
                                                        mount_path.to_string(),
                                                        read_only,
                                                    );
                                                    used_mount_paths.insert(mount_path.to_string());
                                                    info!("🔗 Added standard K8s volume mount: {} -> {} (readonly: {})", volume_config_dir, mount_path, read_only);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            // 处理Secret类型的volume
                            else if let Some(secret_ref) = volume.get("secret") {
                                if let Some(secret_name) = secret_ref
                                    .get("secretName")
                                    .and_then(|n| n.as_str())
                                    .or_else(|| secret_ref.get("name").and_then(|n| n.as_str()))
                                {
                                    info!(
                                        "🔐 Processing Secret volume: {} -> {}",
                                        volume_name, secret_name
                                    );

                                    // 准备Secret目录
                                    let volume_secret_dir =
                                        format!("{}/secrets/{}", workspace, secret_name);
                                    std::fs::create_dir_all(&volume_secret_dir).map_err(|e| {
                                        anyhow::anyhow!(
                                            "Failed to create Secret volume directory {}: {}",
                                            volume_secret_dir,
                                            e
                                        )
                                    })?;
                                    info!(
                                        "📁 Prepared Secret volume directory: {}",
                                        volume_secret_dir
                                    );

                                    // 加载Secret数据
                                    match Self::load_secret_from_etcd(
                                        etcd_manager,
                                        cluster_name,
                                        secret_name,
                                    )
                                    .await
                                    {
                                        Ok(Some(secret_data)) => {
                                            if let Some(data_map) = secret_data.as_mapping() {
                                                info!(
                                                    "📝 Secret '{}' has {} entries",
                                                    secret_name,
                                                    data_map.len()
                                                );
                                                for (filename, content) in data_map {
                                                    if let Some(name) = filename.as_str() {
                                                        if let Some(data) = content.as_str() {
                                                            // 尝试base64解码，否则按原文写入
                                                            let decoded = base64::engine::general_purpose::STANDARD.decode(data.as_bytes())
                                                                .ok()
                                                                .and_then(|bytes| String::from_utf8(bytes).ok())
                                                                .unwrap_or_else(|| data.to_string());
                                                            let file_path = format!(
                                                                "{}/{}",
                                                                volume_secret_dir, name
                                                            );
                                                            std::fs::write(&file_path, decoded).map_err(|e| {
                                                                anyhow::anyhow!("Failed to write Secret volume file {}: {}", file_path, e)
                                                            })?;
                                                            info!(
                                                                "✅ Created Secret volume file: {}",
                                                                file_path
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Ok(None) => {
                                            warn!("⚠️  Secret '{}' not found in etcd, mounting empty directory", secret_name);
                                        }
                                        Err(e) => {
                                            error!(
                                                "❌ Failed to load Secret '{}' from etcd: {}",
                                                secret_name, e
                                            );
                                        }
                                    }

                                    // 添加对应的挂载
                                    if let Some(volume_mounts) = container_spec
                                        .get("volumeMounts")
                                        .and_then(|vm| vm.as_sequence())
                                    {
                                        for volume_mount in volume_mounts {
                                            if let (Some(mount_name), Some(mount_path)) = (
                                                volume_mount.get("name").and_then(|v| v.as_str()),
                                                volume_mount
                                                    .get("mountPath")
                                                    .and_then(|v| v.as_str()),
                                            ) {
                                                if mount_name == volume_name {
                                                    if used_mount_paths.contains(mount_path) {
                                                        warn!("⚠️  Skipping duplicate mount to path '{}' for volume '{}'", mount_path, volume_name);
                                                        continue;
                                                    }
                                                    let read_only = volume_mount
                                                        .get("readOnly")
                                                        .and_then(|v| v.as_bool())
                                                        .unwrap_or(true);
                                                    config = config.add_volume(
                                                        volume_secret_dir.clone(),
                                                        mount_path.to_string(),
                                                        read_only,
                                                    );
                                                    used_mount_paths.insert(mount_path.to_string());
                                                    info!("🔗 Added Secret volume mount: {} -> {} (readonly: {})", volume_secret_dir, mount_path, read_only);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    info!("✅ Standard K8s volumes processing completed");
                }
            }
        }

        // 添加环境变量
        for (key, value) in env.iter() {
            config = config.add_env(key.clone(), value.clone());
        }

        // 传递代理环境变量（若存在且未在容器env中覆盖）
        let proxy_keys = [
            "http_proxy",
            "https_proxy",
            "no_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "NO_PROXY",
        ];
        for key in proxy_keys {
            if !env.contains_key(key) {
                if let Ok(val) = std::env::var(key) {
                    if !val.is_empty() {
                        config = config.add_env(key.to_string(), val);
                    }
                }
            }
        }

        // 设置重启策略
        config = config.restart_policy("unless-stopped".to_string());

        // 准备容器启动后执行的命令
        let mut container_command = Vec::new();
        if let Some(cmd) = command.as_ref() {
            container_command.extend(cmd.iter().cloned());
        }
        if let Some(args) = args.as_ref() {
            container_command.extend(args.iter().cloned());
        }

        if !container_command.is_empty() {
            config = config.command(container_command);
        }

        info!(
            "Docker config prepared: name={}, image={}, volumes={}, env={:?}, cmd={:?}",
            container_name,
            image,
            config.volumes.len(),
            env.keys().collect::<Vec<_>>(),
            config.command
        );

        // 打印将要执行的Docker命令供调试
        let docker_cmd_preview = format!(
            "docker run --name {} {} {} {} {}",
            container_name,
            config
                .volumes
                .iter()
                .map(|vol| format!("-v {}", vol.to_docker_arg()))
                .collect::<Vec<_>>()
                .join(" "),
            config
                .environment
                .iter()
                .map(|(k, v)| format!("-e {}={}", k, v))
                .collect::<Vec<_>>()
                .join(" "),
            if let Some(ref restart) = config.restart_policy {
                format!("--restart {}", restart)
            } else {
                String::new()
            },
            image
        );
        info!("🐳 Docker command preview: {}", docker_cmd_preview);

        // 附加容器标签：记录当前 YAML 校验和，便于调试与配置对账
        let yaml_text = serde_yaml::to_string(deployment_yaml).unwrap_or_default();
        let checksum = Self::calc_hash_u64(&yaml_text);
        config.extra_args.push("--label".to_string());
        config
            .extra_args
            .push(format!("nokube.actor.checksum={}", checksum));

        // 使用 DockerRunner 创建容器
        match DockerRunner::run(&config) {
            Ok(container_id) => {
                info!(
                    "Created Docker container: {} with ID: {}",
                    container_name, container_id
                );

                // 存储成功状态到etcd
                Self::store_pod_status_static(
                    etcd_manager,
                    cluster_name,
                    deployment_name,
                    "Running",
                    None,
                )
                .await?;
            }
            Err(e) => {
                error!("Failed to create deployment {}: {}", deployment_name, e);

                // 存储失败状态和错误信息到etcd
                Self::store_pod_status_static(
                    etcd_manager,
                    cluster_name,
                    deployment_name,
                    "Failed",
                    Some(&format!("{}", e)),
                )
                .await?;
            }
        }

        Ok(())
    }
    /// 静态方法：存储pod状态到etcd
    async fn store_pod_status_static(
        etcd_manager: &Arc<EtcdManager>,
        cluster_name: &str,
        pod_name: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> anyhow::Result<()> {
        let pod_key = format!("/nokube/{}/pods/{}", cluster_name, pod_name);

        let mut pod_info = serde_json::json!({
            "name": pod_name,
            "namespace": "default",
            "node": "agent-node",
            "image": "python:3.10-slim",
            "container_id": serde_json::Value::Null,
            "status": status,
            "ready": status == "Running",
            "restart_count": 0,
            "start_time": chrono::Utc::now().to_rfc3339(),
            "pod_ip": if status == "Running" { serde_json::Value::String("172.17.0.5".to_string()) } else { serde_json::Value::Null },
            "labels": {
                "app": pod_name,
                "component": "pod"
            },
            "ports": ["8080/TCP"],
            "priority": 0
        });

        // 如果有错误信息，添加到pod信息中
        if let Some(error) = error_message {
            pod_info["error_message"] = serde_json::Value::String(error.to_string());
        }

        etcd_manager.put(pod_key, pod_info.to_string()).await?;
        Ok(())
    }

    /// 从etcd加载ConfigMap数据
    async fn load_configmap_from_etcd(
        etcd_manager: &Arc<EtcdManager>,
        cluster_name: &str,
        configmap_name: &str,
    ) -> Result<Option<serde_yaml::Value>> {
        let configmap_key = format!("/nokube/{}/configmaps/{}", cluster_name, configmap_name);

        match etcd_manager.get(configmap_key.clone()).await {
            Ok(kvs) if !kvs.is_empty() => {
                let configmap_yaml = String::from_utf8_lossy(&kvs[0].value);
                info!(
                    "🔍 Loading ConfigMap '{}' from etcd: {} bytes",
                    configmap_name,
                    configmap_yaml.len()
                );

                // 解析YAML以获取ConfigMap数据
                match serde_yaml::from_str::<serde_yaml::Value>(&configmap_yaml) {
                    Ok(configmap_obj) => {
                        // 提取 data 字段
                        if let Some(data) = configmap_obj.get("data") {
                            info!(
                                "✅ Successfully loaded ConfigMap '{}' data from etcd",
                                configmap_name
                            );
                            Ok(Some(data.clone()))
                        } else {
                            warn!("⚠️  ConfigMap '{}' has no 'data' field", configmap_name);
                            Ok(None)
                        }
                    }
                    Err(e) => {
                        error!(
                            "❌ Failed to parse ConfigMap '{}' YAML: {}",
                            configmap_name, e
                        );
                        Err(anyhow::anyhow!("Failed to parse ConfigMap YAML: {}", e))
                    }
                }
            }
            Ok(_) => {
                info!(
                    "🔍 ConfigMap '{}' not found in etcd (key: {})",
                    configmap_name, configmap_key
                );
                Ok(None)
            }
            Err(e) => {
                error!(
                    "❌ Failed to query etcd for ConfigMap '{}': {}",
                    configmap_name, e
                );
                Err(anyhow::anyhow!("Failed to query etcd for ConfigMap: {}", e))
            }
        }
    }

    /// 从etcd加载Secret数据
    async fn load_secret_from_etcd(
        etcd_manager: &Arc<EtcdManager>,
        cluster_name: &str,
        secret_name: &str,
    ) -> Result<Option<serde_yaml::Value>> {
        let secret_key = format!("/nokube/{}/secrets/{}", cluster_name, secret_name);

        match etcd_manager.get(secret_key.clone()).await {
            Ok(kvs) if !kvs.is_empty() => {
                let secret_yaml = String::from_utf8_lossy(&kvs[0].value);
                info!(
                    "🔍 Loading Secret '{}' from etcd: {} bytes",
                    secret_name,
                    secret_yaml.len()
                );

                match serde_yaml::from_str::<serde_yaml::Value>(&secret_yaml) {
                    Ok(secret_obj) => {
                        if let Some(data) = secret_obj.get("data") {
                            info!(
                                "✅ Successfully loaded Secret '{}' data from etcd",
                                secret_name
                            );
                            Ok(Some(data.clone()))
                        } else {
                            warn!("⚠️  Secret '{}' has no 'data' field", secret_name);
                            Ok(None)
                        }
                    }
                    Err(e) => {
                        error!("❌ Failed to parse Secret '{}' YAML: {}", secret_name, e);
                        Err(anyhow::anyhow!("Failed to parse Secret YAML: {}", e))
                    }
                }
            }
            Ok(_) => {
                info!(
                    "🔍 Secret '{}' not found in etcd (key: {})",
                    secret_name, secret_key
                );
                Ok(None)
            }
            Err(e) => {
                error!(
                    "❌ Failed to query etcd for Secret '{}': {}",
                    secret_name, e
                );
                Err(anyhow::anyhow!("Failed to query etcd for Secret: {}", e))
            }
        }
    }
}
