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
        info!("Configuring GreptimeDB data source");
        // In a real implementation, you'd use Grafana's HTTP API to configure data sources
        // This is a placeholder for the API calls
        Ok(())
    }

    async fn import_dashboards(&self) -> Result<()> {
        info!("Importing default dashboards");
        // In a real implementation, you'd import dashboard JSON files
        // This is a placeholder for the dashboard import logic
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