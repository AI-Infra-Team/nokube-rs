use anyhow::Result;
use std::process::Command;
use tracing::info;

pub struct GrafanaManager {
    port: u16,
    greptimedb_endpoint: String,
}

impl GrafanaManager {
    pub fn new(port: u16, greptimedb_endpoint: String) -> Self {
        Self {
            port,
            greptimedb_endpoint,
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
        let config = format!(r#"
[server]
http_port = {}

[datasources]
name = GreptimeDB
type = prometheus
url = {}
access = proxy
isDefault = true
"#, self.port, self.greptimedb_endpoint);

        std::fs::write("/tmp/grafana.ini", config)?;
        Ok(())
    }

    async fn start_grafana_container(&self) -> Result<()> {
        let output = Command::new("docker")
            .args(&[
                "run", "-d",
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

        Ok(())
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
        
        let output = Command::new("docker")
            .args(&["stop", "nokube-grafana"])
            .output()?;

        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to stop Grafana container: {}", error_msg);
        }

        let _ = Command::new("docker")
            .args(&["rm", "nokube-grafana"])
            .output();

        Ok(())
    }
}