use anyhow::Result;
use std::process::Command;
use tracing::info;
use crate::remote_ctl::SSHManager;

pub struct GrafanaManager {
    port: u16,
    greptimedb_endpoint: String,
    ssh_manager: Option<SSHManager>,
    workspace: String,
}

impl GrafanaManager {
    pub fn new(port: u16, greptimedb_endpoint: String, workspace: String) -> Self {
        Self {
            port,
            greptimedb_endpoint,
            ssh_manager: None,
            workspace,
        }
    }

    pub fn with_ssh(port: u16, greptimedb_endpoint: String, workspace: String, ssh_manager: SSHManager) -> Self {
        Self {
            port,
            greptimedb_endpoint,
            ssh_manager: Some(ssh_manager),
            workspace,
        }
    }

    pub async fn setup_grafana(&self, cluster_name: &str) -> Result<()> {
        info!("Setting up Grafana for cluster: {}", cluster_name);
        
        // Create Grafana configuration
        self.create_grafana_config().await?;
        
        // Start Grafana container
        self.start_grafana_container().await?;
        
        // Configure data source
        self.configure_data_source().await?;
        
        // Import default dashboards
        self.import_dashboards().await?;
        
        info!("Grafana setup completed on port: {}", self.port);
        Ok(())
    }

    async fn create_grafana_config(&self) -> Result<()> {
        let config = format!(r#"[server]
http_port = 3000

[security]
admin_user = admin
admin_password = admin

[users]
allow_sign_up = false

[auth.anonymous]
enabled = true
org_name = Main Org.
org_role = Viewer

[datasources]
name = GreptimeDB
type = prometheus
url = {}
access = proxy
isDefault = true
"#, self.greptimedb_endpoint);

        let grafana_config_path = format!("{}/config/grafana.ini", self.workspace);

        // 通过SSH创建配置文件
        match &self.ssh_manager {
            Some(ssh) => {
                // 先创建配置目录
                let create_dir_cmd = format!("mkdir -p {}/config", self.workspace);
                ssh.execute_command(&create_dir_cmd, true, false).await?;
                
                // 创建配置内容到远程文件
                let create_config_cmd = format!("cat > {} << 'EOF'\n{}\nEOF", grafana_config_path, config);
                ssh.execute_command(&create_config_cmd, true, false).await?;
                info!("Grafana config created on remote host at: {}", grafana_config_path);
            }
            None => {
                // 本地创建配置目录
                std::fs::create_dir_all(format!("{}/config", self.workspace))?;
                std::fs::write(&grafana_config_path, config)?;
                info!("Grafana config created locally at: {}", grafana_config_path);
            }
        }
        Ok(())
    }

    async fn stop_existing_container(&self) -> Result<()> {
        match &self.ssh_manager {
            Some(ssh) => {
                // 使用SSH执行命令，启用require_root模式
                // 先检查容器是否存在
                let check_cmd = "docker ps -aq --filter name=nokube-grafana";
                if let Ok(result) = ssh.execute_command(check_cmd, true, false).await {
                    if !result.trim().is_empty() {
                        // 容器存在，先停止再删除
                        let _ = ssh.execute_command("docker stop nokube-grafana", true, false).await;
                        let _ = ssh.execute_command("docker rm nokube-grafana", true, false).await;
                        info!("Stopped and removed existing nokube-grafana container via SSH");
                    }
                }
            }
            None => {
                // 本地执行，使用sudo
                // 先检查容器是否存在
                let check_output = Command::new("sudo")
                    .args(&["docker", "ps", "-aq", "--filter", "name=nokube-grafana"])
                    .output()?;
                
                if check_output.status.success() && !check_output.stdout.is_empty() {
                    // 容器存在，先停止再删除
                    let _ = Command::new("sudo")
                        .args(&["docker", "stop", "nokube-grafana"])
                        .output();
                    let _ = Command::new("sudo")
                        .args(&["docker", "rm", "nokube-grafana"])
                        .output();
                    info!("Stopped and removed existing nokube-grafana container locally");
                }
            }
        }
        Ok(())
    }

    async fn start_grafana_container(&self) -> Result<()> {
        // First, stop and remove any existing container with the same name
        self.stop_existing_container().await?;
        
        let grafana_config_path = format!("{}/config/grafana.ini", self.workspace);
        let docker_cmd = format!(
            "docker run -d --name nokube-grafana -p {}:3000 -v {}:/etc/grafana/grafana.ini greptime/grafana-greptimedb:latest",
            self.port,
            grafana_config_path
        );

        match &self.ssh_manager {
            Some(ssh) => {
                // 使用SSH执行命令，启用require_root模式
                match ssh.execute_command(&docker_cmd, true, false).await {
                    Ok(result) => {
                        let container_id = result.trim();
                        if container_id.len() == 64 && container_id.chars().all(|c| c.is_ascii_hexdigit()) {
                            // 验证容器是否真正在运行
                            self.verify_container_running(ssh, container_id).await?;
                            info!("Grafana container started via SSH with ID: {}", container_id);
                        } else {
                            info!("Grafana container started via SSH: {}", container_id);
                        }
                    }
                    Err(e) => {
                        // 检查错误消息中是否包含容器ID
                        let error_msg = e.to_string();
                        if let Some(container_id) = self.extract_container_id_from_error(&error_msg) {
                            // 验证容器是否真正在运行
                            self.verify_container_running(ssh, &container_id).await?;
                            info!("Grafana container started via SSH with ID: {} (exit code was non-zero but container created)", container_id);
                        } else {
                            anyhow::bail!("Failed to start Grafana container: {}", error_msg);
                        }
                    }
                }
            }
            None => {
                // 本地执行，使用sudo
                let output = Command::new("sudo")
                    .args(&[
                        "docker", "run", "-d",
                        "--name", "nokube-grafana",
                        "-p", &format!("{}:3000", self.port),
                        "-v", &format!("{}:/etc/grafana/grafana.ini", grafana_config_path),
                        "greptime/grafana-greptimedb:latest"
                    ])
                    .output()?;

                if !output.status.success() {
                    let error_msg = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("Failed to start Grafana container: {}", error_msg);
                }
                info!("Grafana container started locally with sudo");
            }
        }

        Ok(())
    }

    async fn verify_container_running(&self, ssh: &SSHManager, container_id: &str) -> Result<()> {
        // 使用 docker ps -q --filter id=<container_id> 验证容器是否运行
        let verify_cmd = format!("docker ps -q --filter id={}", &container_id[..12]); // 只使用前12位ID
        
        match ssh.execute_command(&verify_cmd, true, false).await {
            Ok(result) => {
                let running_id = result.trim();
                if running_id.is_empty() {
                    // 容器不在运行，检查容器状态和日志
                    info!("Container {} not running, checking status and logs...", container_id);
                    
                    // 检查容器状态
                    let status_cmd = format!("docker ps -a --filter id={} --format 'table {{{{.Status}}}}'", &container_id[..12]);
                    if let Ok(status_result) = ssh.execute_command(&status_cmd, true, false).await {
                        info!("Container status: {}", status_result.trim());
                    }
                    
                    // 获取容器日志
                    let logs_cmd = format!("docker logs {}", &container_id[..12]);
                    if let Ok(logs_result) = ssh.execute_command(&logs_cmd, true, false).await {
                        info!("Container logs: {}", logs_result.trim());
                    }
                    
                    anyhow::bail!("Container {} is not running. Check logs above for details.", container_id);
                } else {
                    info!("Verified container {} is running (short ID: {})", container_id, running_id);
                }
            }
            Err(e) => {
                anyhow::bail!("Failed to verify container status: {}", e);
            }
        }
        Ok(())
    }

    fn extract_container_id_from_error(&self, error_msg: &str) -> Option<String> {
        if let Some(start) = error_msg.find("stdout:\n") {
            let stdout_part = if let Some(end) = error_msg[start + 8..].find("\nstderr:\n") {
                &error_msg[start + 8..start + 8 + end]
            } else {
                &error_msg[start + 8..]
            };
            
            let container_id = stdout_part.trim();
            if container_id.len() == 64 && container_id.chars().all(|c| c.is_ascii_hexdigit()) {
                Some(container_id.to_string())
            } else {
                None
            }
        } else {
            None
        }
    }

    async fn configure_data_source(&self) -> Result<()> {
        info!("Configuring GreptimeDB data sources (Prometheus + Postgres) for endpoint: {}", self.greptimedb_endpoint);
        
        // Wait for Grafana to be ready
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        
        // Configure data source via Grafana HTTP API
        // GreptimeDB Prometheus API endpoint needs /v1/prometheus/ suffix
        let prometheus_api_url = format!("{}/v1/prometheus", self.greptimedb_endpoint);
        
        let datasource_config = serde_json::json!({
            "name": "GreptimeDB",
            "type": "prometheus",
            "url": prometheus_api_url,
            "access": "proxy",
            "isDefault": true,
            "jsonData": {
                "httpMethod": "POST",
                "prometheusType": "Prometheus",
                "prometheusVersion": "2.40.0"
            }
        });
        
        let client = reqwest::Client::new();
        let grafana_url = format!("http://localhost:{}/api/datasources", self.port);
        
        let response = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth("admin", Some("admin"))
            .json(&datasource_config)
            .send()
            .await?;
            
        if response.status().is_success() {
            info!("Successfully configured Prometheus datasource for GreptimeDB: {}/v1/prometheus", self.greptimedb_endpoint);
        } else {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Failed to configure data source: {} - {}", status, error_text);
        }

        // 不再配置 GreptimeSQL（Postgres）数据源；统一使用 Greptime HTTP 插件 greptimeplugin

        // Configure GreptimeDB plugin datasource for richer queries (v2.x plugin)
        let plugin_ds_config = serde_json::json!({
            "name": "greptimeplugin",
            "type": "info8fcc-greptimedb-datasource",
            "url": self.greptimedb_endpoint,
            "access": "proxy",
            "isDefault": false,
            "basicAuth": false,
            "jsonData": {
                "server": self.greptimedb_endpoint,
                "defaultDatabase": "public"
            }
        });

        let plugin_resp = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth("admin", Some("admin"))
            .json(&plugin_ds_config)
            .send()
            .await?;

        if plugin_resp.status().is_success() {
            info!("Successfully configured GreptimeDB plugin datasource: greptimeplugin");
        } else {
            let status = plugin_resp.status();
            let error_text = plugin_resp.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            if status.as_u16() == 409 || error_text.to_lowercase().contains("already exists") {
                // Try to update existing datasource to ensure URL set
                let get_url = format!("http://localhost:{}/api/datasources/name/greptimeplugin", self.port);
                if let Ok(get_resp) = client
                    .get(&get_url)
                    .basic_auth("admin", Some("admin"))
                    .send()
                    .await
                {
                    if get_resp.status().is_success() {
                        if let Ok(val) = get_resp.json::<serde_json::Value>().await {
                            if let Some(id) = val.get("id").and_then(|v| v.as_i64()) {
                                let put_url = format!("http://localhost:{}/api/datasources/{}", self.port, id);
                                let _ = client
                                    .put(&put_url)
                                    .header("Content-Type", "application/json")
                                    .basic_auth("admin", Some("admin"))
                                    .json(&plugin_ds_config)
                                    .send()
                                    .await?;
                                info!("Updated existing greptimeplugin datasource with server URL");
                            }
                        }
                    }
                }
            } else {
                anyhow::bail!("Failed to configure Greptime plugin datasource: {} - {}", status, error_text);
            }
        }
        
        Ok(())
    }

    async fn import_dashboards(&self) -> Result<()> {
        info!("Importing default nokube dashboards");
        
        // Wait for Grafana API to be available
        self.wait_for_grafana_api().await?;
        
        // Import cluster monitoring dashboard
        self.import_cluster_dashboard().await?;
        
        // Wait between imports to avoid rate limiting
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        
        // Import service dashboard for k8s elements
        self.import_service_dashboard().await?;
        
        // Wait between imports
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        
        // Import actor monitoring dashboard
        self.import_actor_dashboard().await?;
        
        // Wait between imports
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        
        // Import logs dashboard for viewing GreptimeDB logs
        self.import_logs_dashboard().await?;
        
        info!("All NoKube dashboards imported successfully");
        Ok(())
    }

    async fn wait_for_grafana_api(&self) -> Result<()> {
        info!("Waiting for Grafana API to be available...");
        let client = reqwest::Client::new();
        let health_url = format!("http://localhost:{}/api/health", self.port);
        
        for attempt in 1..=10 {
            match client.get(&health_url).send().await {
                Ok(response) if response.status().is_success() => {
                    info!("Grafana API is ready after {} attempts", attempt);
                    // Additional wait to ensure full readiness
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    return Ok(());
                }
                Ok(response) => {
                    info!("Grafana API not ready yet (attempt {}/10): status {}", attempt, response.status());
                }
                Err(e) => {
                    info!("Grafana API connection failed (attempt {}/10): {}", attempt, e);
                }
            }
            
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
        
        anyhow::bail!("Grafana API did not become available after 10 attempts")
    }

    async fn import_cluster_dashboard(&self) -> Result<()> {
        // Create a default dashboard with panels for CPU, memory, and network metrics
        let dashboard_config = serde_json::json!({
            "dashboard": {
                "id": null,
                "uid": "nokube-cluster-monitoring",
                "title": "NoKube Cluster Monitoring",
                "tags": ["nokube", "cluster"],
                "timezone": "browser",
                "panels": [
                    {
                        "id": 1,
                        "title": "Cluster Memory Usage (%)",
                        "type": "graph",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "nokube_memory_usage", "legendFormat": "{{instance}}", "intervalFactor": 1, "step": 30}
                        ],
                        "yAxes": [
                            {
                                "label": "Percent",
                                "max": 100,
                                "min": 0
                            },
                            {
                                "show": false
                            }
                        ],
                        "lines": true,
                        "fill": 1,
                        "linewidth": 2,
                        "pointradius": 2,
                        "points": false,
                        "renderer": "flot",
                        "seriesOverrides": [],
                        "spaceLength": 10,
                        "stack": false,
                        "steppedLine": false,
                        "nullPointMode": "null as zero",
                        "gridPos": {"h": 9, "w": 8, "x": 0, "y": 0}
                    },
                    {
                        "id": 2,
                        "title": "Cluster Container Memory (bytes, stacked)",
                        "type": "graph",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "sum by (container) (nokube_container_mem_bytes)", "legendFormat": "{{container}}", "intervalFactor": 1, "step": 30},
                            {"expr": "sum(nokube_memory_used_bytes)", "legendFormat": "Cluster Used", "intervalFactor": 1, "step": 30},
                            {"expr": "sum(nokube_memory_total_bytes)", "legendFormat": "Cluster Total", "intervalFactor": 1, "step": 30}
                        ],
                        "yAxes": [{"label": "Bytes", "min": 0}, {"show": false}],
                        "lines": true, "fill": 2, "linewidth": 2, "pointradius": 2, "points": false, "renderer": "flot",
                        "seriesOverrides": [
                            {"alias": "Cluster Used", "stack": false, "lines": true, "fill": 0, "linewidth": 2},
                            {"alias": "Cluster Total", "stack": false, "lines": true, "fill": 0, "linewidth": 1}
                        ],
                        "spaceLength": 10, "stack": true, "steppedLine": false, "nullPointMode": "null as zero",
                        "gridPos": {"h": 9, "w": 8, "x": 8, "y": 0}
                    },
                    {
                        "id": 3,
                        "title": "Network RX (Download)",
                        "type": "graph",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {
                                "expr": "rate(nokube_network_rx_bytes[5m])",
                                "legendFormat": "RX {{instance}}",
                                "intervalFactor": 1,
                                "step": 30
                            }
                        ],
                        "yAxes": [
                            {
                                "label": "Bytes/sec"
                            },
                            {
                                "show": false
                            }
                        ],
                        "lines": true,
                        "fill": 2,
                        "linewidth": 2,
                        "pointradius": 2,
                        "points": false,
                        "renderer": "flot",
                        "seriesOverrides": [],
                        "spaceLength": 10,
                        "stack": true,
                        "steppedLine": false,
                        "nullPointMode": "null as zero",
                        "gridPos": {"h": 9, "w": 12, "x": 0, "y": 8}
                    },
                    {
                        "id": 4,
                        "title": "Network TX (Upload)",
                        "type": "graph",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {
                                "expr": "rate(nokube_network_tx_bytes[5m])",
                                "legendFormat": "TX {{instance}}",
                                "intervalFactor": 1,
                                "step": 30
                            }
                        ],
                        "yAxes": [
                            {
                                "label": "Bytes/sec"
                            },
                            {
                                "show": false
                            }
                        ],
                        "lines": true,
                        "fill": 2,
                        "linewidth": 2,
                        "pointradius": 2,
                        "points": false,
                        "renderer": "flot",
                        "seriesOverrides": [],
                        "spaceLength": 10,
                        "stack": true,
                        "steppedLine": false,
                        "nullPointMode": "null as zero",
                        "gridPos": {"h": 9, "w": 12, "x": 12, "y": 8}
                    },
                    {
                        "id": 5,
                        "title": "Cluster Container CPU (%) (stacked)",
                        "type": "graph",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "sum by (container) (nokube_container_cpu)", "legendFormat": "{{container}}", "intervalFactor": 1, "step": 30}
                        ],
                        "yAxes": [{"label": "Percent", "max": 100, "min": 0}, {"show": false}],
                        "lines": true, "fill": 1, "linewidth": 2, "pointradius": 2, "points": false, "renderer": "flot",
                        "seriesOverrides": [], "spaceLength": 10, "stack": true, "steppedLine": false, "nullPointMode": "null as zero",
                        "gridPos": {"h": 9, "w": 8, "x": 16, "y": 0}
                    },
                    
                    {
                        "id": 7,
                        "title": "Node Memory Used vs Total (bytes)",
                        "type": "graph",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "nokube_memory_total_bytes", "legendFormat": "Total {{instance}}", "intervalFactor": 1, "step": 30},
                            {"expr": "nokube_memory_used_bytes", "legendFormat": "Used {{instance}}", "intervalFactor": 1, "step": 30}
                        ],
                        "yAxes": [{"label": "Bytes", "min": 0}, {"show": false}],
                        "lines": true,
                        "fill": 0,
                        "linewidth": 2,
                        "pointradius": 2,
                        "points": false,
                        "renderer": "flot",
                        "seriesOverrides": [],
                        "spaceLength": 10,
                        "stack": false,
                        "steppedLine": false,
                        "nullPointMode": "null as zero",
                        "gridPos": {"h": 9, "w": 24, "x": 0, "y": 27}
                    }
                ],
                "time": {
                    "from": "now-1h",
                    "to": "now"
                },
                "refresh": "30s",
                "schemaVersion": 16,
                "version": 0
            },
            "overwrite": true
        });
        
        let client = reqwest::Client::new();
        let grafana_url = format!("http://localhost:{}/api/dashboards/db", self.port);
        
        let response = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth("admin", Some("admin"))
            .json(&dashboard_config)
            .send()
            .await?;
            
        if response.status().is_success() {
            info!("Successfully imported NoKube cluster monitoring dashboard");
        } else {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Failed to import cluster dashboard: {} - {}", status, error_text);
        }
        
        Ok(())
    }

    async fn import_service_dashboard(&self) -> Result<()> {
        info!("Importing NoKube service dashboard for k8s elements");
        
        // Create service dashboard with filters for namespace, daemonset, deployment, pod, container
        let service_dashboard_config = serde_json::json!({
            "dashboard": {
                "id": null,
                "title": "NoKube Service Dashboard",
                "tags": ["nokube", "k8s", "service"],
                "timezone": "browser",
                "templating": {
                    "list": [
                        {
                            "name": "namespace",
                            "type": "query",
                            "label": "Namespace",
                            "datasource": "GreptimeDB",
                            "query": "label_values(nokube_k8s_object_info, namespace)",
                            "refresh": 1,
                            "includeAll": true,
                            "allValue": ".*",
                            "multi": true,
                            "current": {
                                "text": "All",
                                "value": ["$__all"]
                            }
                        },
                        {
                            "name": "object_type",
                            "type": "custom",
                            "label": "Object Type",
                            "options": [
                                {"text": "All", "value": ".*", "selected": true},
                                {"text": "DaemonSet", "value": "daemonset", "selected": false},
                                {"text": "Deployment", "value": "deployment", "selected": false},
                                {"text": "Pod", "value": "pod", "selected": false},
                                {"text": "Container", "value": "container", "selected": false}
                            ],
                            "includeAll": true,
                            "allValue": ".*",
                            "multi": true,
                            "current": {
                                "text": "All",
                                "value": ["$__all"]
                            }
                        }
                    ]
                },
                "panels": [
                    {
                        "id": 1,
                        "title": "K8s Objects Overview",
                        "type": "table",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {
                                "expr": "nokube_k8s_object_info{namespace=~\"$namespace\", object_type=~\"$object_type\"}",
                                "format": "table",
                                "instant": true
                            }
                        ],
                        "columns": [
                            {"text": "Namespace", "value": "namespace"},
                            {"text": "Object Type", "value": "object_type"},
                            {"text": "Name", "value": "object_name"},
                            {"text": "Status", "value": "status"},
                            {"text": "Parent", "value": "parent_object"}
                        ],
                        "sort": {
                            "col": 1,
                            "desc": false
                        },
                        "styles": [
                            {
                                "alias": "Status",
                                "colorMode": "cell",
                                "colors": ["rgba(245, 54, 54, 0.9)", "rgba(237, 129, 40, 0.89)", "rgba(50, 172, 45, 0.97)"],
                                "dateFormat": "YYYY-MM-DD HH:mm:ss",
                                "decimals": 2,
                                "pattern": "status",
                                "thresholds": ["0.5", "0.8"],
                                "type": "string",
                                "unit": "string"
                            }
                        ],
                        "gridPos": {"h": 12, "w": 24, "x": 0, "y": 0}
                    },
                    {
                        "id": 2,
                        "title": "Pod-DaemonSet Relationship",
                        "type": "graph",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {
                                "expr": "nokube_k8s_pod_status{namespace=~\"$namespace\", parent_daemonset!=\"\"}",
                                "legendFormat": "{{parent_daemonset}}/{{pod_name}} - {{status}}",
                                "intervalFactor": 1,
                                "step": 30
                            }
                        ],
                        "yAxes": [
                            {
                                "label": "Status (1=Running, 0=Not Running)",
                                "max": 1.5,
                                "min": -0.5
                            },
                            {
                                "show": false
                            }
                        ],
                        "lines": true,
                        "fill": 0,
                        "linewidth": 2,
                        "pointradius": 5,
                        "points": true,
                        "renderer": "flot",
                        "seriesOverrides": [],
                        "spaceLength": 10,
                        "stack": false,
                        "steppedLine": true,
                        "nullPointMode": "null as zero",
                        "gridPos": {"h": 9, "w": 12, "x": 0, "y": 12}
                    },
                    {
                        "id": 3,
                        "title": "Container CPU (%) (stacked)",
                        "type": "graph",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "nokube_container_cpu", "legendFormat": "{{container}}", "intervalFactor": 1, "step": 30}
                        ],
                        "yAxes": [{"label": "Percent", "max": 100, "min": 0}, {"show": false}],
                        "lines": true,
                        "fill": 1,
                        "linewidth": 2,
                        "pointradius": 2,
                        "points": false,
                        "renderer": "flot",
                        "seriesOverrides": [],
                        "spaceLength": 10,
                        "stack": true,
                        "steppedLine": false,
                        "nullPointMode": "null as zero",
                        "gridPos": {"h": 9, "w": 12, "x": 12, "y": 12}
                    },
                    {
                        "id": 9,
                        "title": "Container Memory (%) (stacked)",
                        "type": "graph",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "nokube_container_mem_percent", "legendFormat": "{{container}}", "intervalFactor": 1, "step": 30}
                        ],
                        "yAxes": [{"label": "Percent", "max": 100, "min": 0}, {"show": false}],
                        "lines": true,
                        "fill": 1,
                        "linewidth": 2,
                        "pointradius": 2,
                        "points": false,
                        "renderer": "flot",
                        "seriesOverrides": [],
                        "spaceLength": 10,
                        "stack": true,
                        "steppedLine": false,
                        "nullPointMode": "null as zero",
                        "gridPos": {"h": 9, "w": 12, "x": 12, "y": 21}
                    },
                    {
                        "id": 4,
                        "title": "Service Events Timeline",
                        "type": "graph",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {
                                "expr": "increase(nokube_k8s_events_total{namespace=~\"$namespace\", object_type=~\"$object_type\"}[5m])",
                                "legendFormat": "{{event_type}} - {{object_name}}",
                                "intervalFactor": 1,
                                "step": 30
                            }
                        ],
                        "yAxes": [
                            {
                                "label": "Events per 5min"
                            },
                            {
                                "show": false
                            }
                        ],
                        "lines": true,
                        "fill": 2,
                        "linewidth": 2,
                        "pointradius": 2,
                        "points": false,
                        "renderer": "flot",
                        "seriesOverrides": [],
                        "spaceLength": 10,
                        "stack": true,
                        "steppedLine": false,
                        "nullPointMode": "null as zero",
                        "gridPos": {"h": 9, "w": 24, "x": 0, "y": 21}
                    }
                ],
                "time": {
                    "from": "now-1h",
                    "to": "now"
                },
                "refresh": "30s",
                "schemaVersion": 16,
                "version": 0
            },
            "overwrite": true
        });
        
        let client = reqwest::Client::new();
        let grafana_url = format!("http://localhost:{}/api/dashboards/db", self.port);
        
        let response = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth("admin", Some("admin"))
            .json(&service_dashboard_config)
            .send()
            .await?;
            
        if response.status().is_success() {
            info!("Successfully imported NoKube service dashboard for k8s elements");
        } else {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Failed to import service dashboard: {} - {}", status, error_text);
        }
        
        Ok(())
    }

    async fn import_actor_dashboard(&self) -> Result<()> {
        info!("Importing NoKube actor monitoring dashboard");

        // Actor dashboard: root_actor rows; containers grouped by root actor
        let actor_dashboard_config = serde_json::json!({
            "dashboard": {
                "id": null,
                "uid": "nokube-actor-dashboard",
                "title": "NoKube Actor Dashboard",
                "tags": ["nokube", "actor", "root", "hierarchy", "pod", "container"],
                "timezone": "browser",
                "templating": {
                    "list": [
                        {
                            "name": "cluster",
                            "type": "query",
                            "label": "Cluster",
                            "datasource": "GreptimeDB",
                            "query": "label_values(nokube_container_cpu_cores, cluster_name)",
                            "refresh": 1,
                            "includeAll": true,
                            "allValue": ".*",
                            "multi": true,
                            "current": {"text": "All", "value": ["$__all"]}
                        },
                        {
                            "name": "root_actor",
                            "type": "query",
                            "label": "Root Actor",
                            "datasource": "GreptimeDB",
                            "query": "label_values(nokube_container_cpu_cores{cluster_name=~\"$cluster\"}, root_actor)",
                            "refresh": 1,
                            "includeAll": false,
                            "multi": true,
                            "current": {"text": "", "value": []}
                        },
                        {"name": "node", "type": "query", "label": "Node", "datasource": "GreptimeDB", "query": "label_values(nokube_container_mem_bytes{cluster_name=~\"$cluster\", root_actor=~\"$root_actor\"}, node)", "refresh": 1, "includeAll": true, "allValue": ".*", "multi": true, "current": {"text": "All", "value": ["$__all"]}}
                    ]
                },
                "panels": [
                    {
                        "id": 1,
                        "title": "Root Actors",
                        "type": "stat",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "count(count by (root_actor) (nokube_container_cpu_cores{cluster_name=~\"$cluster\"}))", "legendFormat": "Roots"}
                        ],
                        "gridPos": {"h": 4, "w": 24, "x": 0, "y": 0}
                    },
                    {
                        "id": 2,
                        "title": "Container Count ($cluster)",
                        "type": "stat",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "count(count by (pod, container) (nokube_container_mem_bytes{cluster_name=~\"$cluster\"}))", "instant": true}
                        ],
                        "gridPos": {"h": 4, "w": 24, "x": 0, "y": 4}
                    },

                    // Repeated row per root actor
                    {
                        "id": 100,
                        "type": "row",
                        "title": "$root_actor",
                        "repeat": "root_actor",
                        "collapsed": false,
                        "gridPos": {"h": 1, "w": 24, "x": 0, "y": 4}
                    },
                    {
                        "id": 101,
                        "title": "Pods of $root_actor (node/pod)",
                        "type": "table",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "sum by (pod, node) (nokube_container_mem_bytes{cluster_name=~\"$cluster\", root_actor=~\"$root_actor\"})", "format": "table", "instant": true}
                        ],
                        "transformations": [
                            {"id": "labelsToFields", "options": {"mode": "columns"}},
                            {"id": "organize", "options": {"excludeByName": {"Time": true, "__name__": true, "instance": true, "job": true, "metric": true, "Value": false}, "renameByName": {"node": "Node", "pod": "Pod", "Value": "Mem Bytes"}}}
                        ],
                        "gridPos": {"h": 8, "w": 24, "x": 0, "y": 5}
                    },
                    {
                        "id": 102,
                        "title": "Containers of $root_actor (node/pod/container)",
                        "type": "table",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "nokube_container_mem_bytes{cluster_name=~\"$cluster\", root_actor=~\"$root_actor\"}", "format": "table", "instant": true}
                        ],
                        "transformations": [
                            {"id": "labelsToFields", "options": {"mode": "columns"}},
                            {"id": "organize", "options": {"excludeByName": {"Time": true, "__name__": true, "instance": true, "job": true, "metric": true, "Value": false}, "renameByName": {"node": "Node", "pod": "Pod", "container": "Container", "root_actor": "Root Actor", "Value": "Mem Bytes"}}}
                        ],
                        "fieldConfig": {"defaults": {}, "overrides": [
                            {"matcher": {"id": "byName", "options": "Container"},
                             "properties": [
                                 {"id": "links", "value": [
                                     {"title": "View Logs", "url": "/d/nokube-logs-mysql?var-container_path=${__data.fields.container_path}", "targetBlank": true}
                                 ]}
                             ]}
                            ,
                            {"matcher": {"id": "byName", "options": "container_path"},
                             "properties": [
                                 {"id": "links", "value": [
                                     {"title": "View Logs (by Path)", "url": "/d/nokube-logs-mysql?var-container_path=${__value.raw}", "targetBlank": true}
                                 ]},
                                 {"id": "custom.hidden", "value": true}
                             ]}
                        ]},
                        "gridPos": {"h": 8, "w": 24, "x": 0, "y": 13}
                    },
                    {
                        "id": 103,
                        "title": "Container CPU (cores) [$root_actor]",
                        "type": "timeseries",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "sum by (pod, container, node) (nokube_container_cpu_cores{cluster_name=~\"$cluster\", root_actor=~\"$root_actor\"})", "legendFormat": "{{pod}}/{{container}} @ {{node}}"}
                        ],
                        "fieldConfig": {"defaults": {"unit": "cores", "min": 0, "custom": {"stacking": {"mode": "none"}}}},
                        "gridPos": {"h": 8, "w": 12, "x": 0, "y": 21}
                    },
                    {
                        "id": 104,
                        "title": "Container Memory (bytes) [$root_actor]",
                        "type": "timeseries",
                        "datasource": "GreptimeDB",
                        "targets": [
                            {"expr": "sum by (pod, container, node) (nokube_container_mem_bytes{cluster_name=~\"$cluster\", root_actor=~\"$root_actor\"})", "legendFormat": "{{pod}}/{{container}} @ {{node}}"},
                            {"expr": "nokube_memory_used_bytes{cluster_name=~\"$cluster\", node=~\"$node\"}", "legendFormat": "Node Used: {{node}}"},
                            {"expr": "nokube_memory_total_bytes{cluster_name=~\"$cluster\", node=~\"$node\"}", "legendFormat": "Node Total: {{node}}"}
                        ],
                        "fieldConfig": {"defaults": {"unit": "bytes", "min": 0, "custom": {"stacking": {"mode": "normal", "group": "A"}}},
                            "overrides": [
                                {"matcher": {"id": "byRegexp", "options": "^Node Used.*$"},
                                 "properties": [
                                     {"id": "custom.stacking", "value": {"mode": "none"}},
                                     {"id": "custom.fillOpacity", "value": 0}
                                 ]},
                                {"matcher": {"id": "byRegexp", "options": "^Node Total.*$"},
                                 "properties": [
                                     {"id": "custom.stacking", "value": {"mode": "none"}},
                                     {"id": "custom.fillOpacity", "value": 0}
                                 ]}
                            ]
                        },
                        "gridPos": {"h": 8, "w": 12, "x": 12, "y": 21}
                    }
                ],
                "time": {"from": "now-1h", "to": "now"},
                "refresh": "30s",
                "schemaVersion": 16,
                "version": 0
            },
            "overwrite": true
        });
        
        let client = reqwest::Client::new();
        let grafana_url = format!("http://localhost:{}/api/dashboards/db", self.port);
        
        let response = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth("admin", Some("admin"))
            .json(&actor_dashboard_config)
            .send()
            .await?;
            
        if response.status().is_success() {
            info!("Successfully imported NoKube actor monitoring dashboard");
        } else {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Failed to import actor dashboard: {} - {}", status, error_text);
        }
        
        Ok(())
    }

    async fn import_logs_dashboard(&self) -> Result<()> {
        info!("Importing NoKube logs dashboard for OTLP GreptimeDB logs");
        
        // Create a simplified logs dashboard for OTLP format
        let logs_dashboard_config = serde_json::json!({
            "dashboard": {
                "id": null,
                "title": "NoKube Logs Dashboard (OTLP)",
                "tags": ["nokube", "logs", "greptimedb", "otlp"],
                "timezone": "browser",
                "templating": {
                    "list": [
                        {
                            "name": "cluster",
                            "type": "query",
                            "label": "Cluster",
                            "datasource": "GreptimeSQL",
                            "query": "SELECT DISTINCT log_attributes->>'cluster_name' as value FROM opentelemetry_logs WHERE log_attributes ? 'cluster_name'",
                            "refresh": 1,
                            "includeAll": true,
                            "allValue": "",
                            "multi": true,
                            "current": {
                                "text": "All",
                                "value": ["$__all"]
                            }
                        },
                        {
                            "name": "source",
                            "type": "query", 
                            "label": "Source",
                            "datasource": "GreptimeSQL",
                            "query": "SELECT DISTINCT log_attributes->>'source' as value FROM opentelemetry_logs WHERE log_attributes->>'cluster_name' IN (${cluster:csv}) AND log_attributes ? 'source'",
                            "refresh": 1,
                            "includeAll": true,
                            "allValue": "",
                            "multi": true,
                            "current": {
                                "text": "All",
                                "value": ["$__all"]
                            }
                        },
                        {
                            "name": "source_id",
                            "type": "query",
                            "label": "Source ID",
                            "datasource": "GreptimeSQL",
                            "query": "SELECT DISTINCT log_attributes->>'source_id' as value FROM opentelemetry_logs WHERE log_attributes->>'cluster_name' IN (${cluster:csv}) AND log_attributes->>'source' IN (${source:csv}) AND log_attributes ? 'source_id'",
                            "refresh": 1,
                            "includeAll": true,
                            "allValue": "",
                            "multi": true,
                            "current": {
                                "text": "All",
                                "value": ["$__all"]
                            }
                        },
                        {"name": "level", "type": "custom", "label": "Log Level", "options": [{"text": "All", "value": ".*", "selected": true}, {"text": "ERROR", "value": "ERROR"}, {"text": "WARN", "value": "WARN"}, {"text": "INFO", "value": "INFO"}, {"text": "DEBUG", "value": "DEBUG"}], "includeAll": true, "allValue": "", "multi": true, "current": {"text": "All", "value": ["$__all"]}}
                    ]
                },
                "panels": [
                    {
                        "id": 1,
                        "title": "Log Level Distribution",
                        "type": "piechart",
                        "datasource": "greptimeplugin",
                        "targets": [
                            {
                                "rawSql": "SELECT severity_text as metric, count(*) as value FROM opentelemetry_logs WHERE timestamp >= now() - INTERVAL '1 hour' GROUP BY severity_text",
                                "format": "table"
                            }
                        ],
                        "fieldConfig": {
                            "defaults": {
                                "color": {
                                    "mode": "palette-classic"
                                }
                            }
                        },
                        "gridPos": {"h": 8, "w": 8, "x": 0, "y": 0}
                    },
                    {
                        "id": 2,
                        "title": "Log Rate by Source",
                        "type": "timeseries",
                        "datasource": "greptimeplugin",
                        "targets": [
                            {
                                "rawSql": "SELECT date_trunc('minute', timestamp) as time, 'All Logs' as metric, count(*) as value FROM opentelemetry_logs WHERE timestamp >= now() - INTERVAL '1 hour' GROUP BY time ORDER BY time",
                                "format": "time_series"
                            }
                        ],
                        "fieldConfig": {
                            "defaults": {
                                "color": {
                                    "mode": "palette-classic"
                                }
                            }
                        },
                        "gridPos": {"h": 8, "w": 16, "x": 8, "y": 0}
                    },
                    {
                        "id": 3,
                        "title": "Recent Logs Timeline", 
                        "type": "timeseries",
                        "datasource": "greptimeplugin",
                        "targets": [
                            {
                                "rawSql": "SELECT date_trunc('second', timestamp) as time, severity_text as metric, count(*) as value FROM opentelemetry_logs WHERE timestamp >= now() - INTERVAL '1 hour' GROUP BY time, severity_text ORDER BY time",
                                "format": "time_series"
                            }
                        ],
                        "fieldConfig": {
                            "defaults": {
                                "color": {
                                    "mode": "palette-classic"
                                }
                            }
                        },
                        "gridPos": {"h": 8, "w": 24, "x": 0, "y": 8}
                    },
                    {
                        "id": 4,
                        "title": "Log Messages (Latest 100)",
                        "type": "logs",
                        "datasource": "greptimeplugin",
                        "targets": [
                            {
                                "rawSql": "SELECT timestamp as time, body as message, severity_text as level FROM opentelemetry_logs WHERE timestamp >= now() - INTERVAL '1 hour' ORDER BY time DESC LIMIT 100",
                                "format": "table"
                            }
                        ],
                        "options": {"showTime": true, "showLabels": false, "showCommonLabels": false, "wrapLogMessage": false, "enableLogDetails": false, "messageField": "message"},
                        "options": {"showTime": true, "showLabels": false, "showCommonLabels": false, "wrapLogMessage": false, "enableLogDetails": false},
                        "fieldConfig": {
                            "overrides": [
                                {
                                    "matcher": {"id": "byName", "options": "message"},
                                    "properties": [{"id": "custom.width", "value": 400}]
                                },
                                {
                                    "matcher": {"id": "byName", "options": "time"},
                                    "properties": [{"id": "custom.displayMode", "value": "auto"}]
                                }
                            ]
                        },
                        "gridPos": {"h": 12, "w": 24, "x": 0, "y": 16}
                    }
                ],
                "time": {
                    "from": "now-1h",
                    "to": "now"
                },
                "refresh": "30s",
                "schemaVersion": 16,
                "version": 0
            },
            "overwrite": true
        });
        
        let client = reqwest::Client::new();
        let grafana_url = format!("http://localhost:{}/api/dashboards/db", self.port);
        
        let response = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth("admin", Some("admin"))
            .json(&logs_dashboard_config)
            .send()
            .await?;
            
        if response.status().is_success() {
            info!("Successfully imported NoKube logs dashboard for GreptimeDB logs");
        } else {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Failed to import logs dashboard: {} - {}", status, error_text);
        }
        
        Ok(())
    }

    pub async fn stop_grafana(&self) -> Result<()> {
        info!("Stopping Grafana");
        
        match &self.ssh_manager {
            Some(ssh) => {
                // 使用SSH执行命令，启用require_root模式
                let _result = ssh.execute_command("docker stop nokube-grafana", true, false).await?;
                let _result = ssh.execute_command("docker rm nokube-grafana", true, false).await.ok();
                info!("Grafana container stopped via SSH");
            }
            None => {
                // 本地执行，使用sudo
                let output = Command::new("sudo")
                    .args(&["docker", "stop", "nokube-grafana"])
                    .output()?;

                if !output.status.success() {
                    let error_msg = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("Failed to stop Grafana container: {}", error_msg);
                }

                let _ = Command::new("sudo")
                    .args(&["docker", "rm", "nokube-grafana"])
                    .output();
                    
                info!("Grafana container stopped locally with sudo");
            }
        }

        Ok(())
    }
}
