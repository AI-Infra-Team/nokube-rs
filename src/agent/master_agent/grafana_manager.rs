use anyhow::Result;
use std::process::Command;
use tracing::info;
use crate::remote_ctl::SSHManager;

pub struct GrafanaManager {
    port: u16,
    greptimedb_endpoint: String,
    ssh_manager: Option<SSHManager>,
}

impl GrafanaManager {
    pub fn new(port: u16, greptimedb_endpoint: String) -> Self {
        Self {
            port,
            greptimedb_endpoint,
            ssh_manager: None,
        }
    }

    pub fn with_ssh(port: u16, greptimedb_endpoint: String, ssh_manager: SSHManager) -> Self {
        Self {
            port,
            greptimedb_endpoint,
            ssh_manager: Some(ssh_manager),
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

        // 通过SSH创建配置文件
        match &self.ssh_manager {
            Some(ssh) => {
                // 先创建配置内容到远程临时文件
                let create_config_cmd = format!("cat > /tmp/grafana.ini << 'EOF'\n{}\nEOF", config);
                ssh.execute_command(&create_config_cmd, true, false).await?;
                info!("Grafana config created on remote host");
            }
            None => {
                std::fs::write("/tmp/grafana.ini", config)?;
                info!("Grafana config created locally");
            }
        }
        Ok(())
    }

    async fn start_grafana_container(&self) -> Result<()> {
        let docker_cmd = format!(
            "docker run -d --name nokube-grafana -p {}:3000 -v /tmp/grafana.ini:/etc/grafana/grafana.ini grafana/grafana:latest",
            self.port
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
                        "-v", "/tmp/grafana.ini:/etc/grafana/grafana.ini",
                        "grafana/grafana:latest"
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
        info!("Configuring GreptimeDB data source with Prometheus API endpoint: {}/v1/prometheus", self.greptimedb_endpoint);
        
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
            info!("Successfully configured GreptimeDB data source with Prometheus API endpoint: {}/v1/prometheus", self.greptimedb_endpoint);
        } else {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Failed to configure data source: {} - {}", status, error_text);
        }
        
        Ok(())
    }

    async fn import_dashboards(&self) -> Result<()> {
        info!("Importing default nokube cluster monitoring dashboard");
        
        // Create a default dashboard with panels for CPU, memory, and network metrics
        let dashboard_config = serde_json::json!({
            "dashboard": {
                "id": null,
                "title": "NoKube Cluster Monitoring",
                "tags": ["nokube", "cluster"],
                "timezone": "browser",
                "panels": [
                    {
                        "id": 1,
                        "title": "CPU Usage",
                        "type": "graph",
                        "targets": [
                            {
                                "expr": "nokube_cpu_usage",
                                "legendFormat": "{{instance}}",
                                "intervalFactor": 1,
                                "step": 30
                            }
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
                        "gridPos": {"h": 9, "w": 12, "x": 0, "y": 0}
                    },
                    {
                        "id": 2,
                        "title": "Memory Usage",
                        "type": "graph",
                        "targets": [
                            {
                                "expr": "nokube_memory_usage",
                                "legendFormat": "{{instance}}",
                                "intervalFactor": 1,
                                "step": 30
                            }
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
                        "gridPos": {"h": 9, "w": 12, "x": 12, "y": 0}
                    },
                    {
                        "id": 3,
                        "title": "Network RX (Download)",
                        "type": "graph",
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
                        "gridPos": {"h": 9, "w": 12, "x": 0, "y": 9}
                    },
                    {
                        "id": 4,
                        "title": "Network TX (Upload)",
                        "type": "graph",
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
                        "gridPos": {"h": 9, "w": 12, "x": 12, "y": 9}
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
            anyhow::bail!("Failed to import dashboard: {} - {}", status, error_text);
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