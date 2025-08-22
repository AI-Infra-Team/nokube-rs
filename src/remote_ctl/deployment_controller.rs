use crate::agent::master_agent::GrafanaManager;
use crate::config::{cluster_config::ClusterConfig, ConfigManager};
use crate::remote_ctl::SSHManager;
use anyhow::Result;
use std::collections::HashMap;
use tracing::info;

pub struct DeploymentController {
    config_manager: ConfigManager,
    ssh_managers: HashMap<String, SSHManager>,
}

impl DeploymentController {
    pub async fn new() -> Result<Self> {
        Ok(Self {
            config_manager: ConfigManager::new().await?,
            ssh_managers: HashMap::new(),
        })
    }

    pub async fn deploy_or_update(&mut self, cluster_name: &str) -> Result<()> {
        info!("Starting deployment/update for cluster: {}", cluster_name);

        // Get cluster configuration from etcd
        let cluster_config = self
            .config_manager
            .get_cluster_config(cluster_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Cluster {} not found", cluster_name))?;

        // Initialize SSH managers for all nodes
        self.initialize_ssh_managers(&cluster_config).await?;

        // Prepare binary dependencies and resources
        self.prepare_dependencies().await?;

        // Deploy agents to all nodes
        self.deploy_node_agents(&cluster_config).await?;

        // Update node configurations and restart agents
        self.update_node_configurations(&cluster_config).await?;

        // Deploy and configure bound services
        self.deploy_bound_services(&cluster_config).await?;

        info!("Deployment/update completed for cluster: {}", cluster_name);
        Ok(())
    }

    pub async fn setup_monitoring(&self, cluster_name: &str) -> Result<()> {
        info!("Setting up monitoring for cluster: {}", cluster_name);

        let cluster_config = self
            .config_manager
            .get_cluster_config(cluster_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Cluster {} not found", cluster_name))?;

        if !cluster_config.task_spec.monitoring.enabled {
            anyhow::bail!("Monitoring is not enabled for cluster {}", cluster_name);
        }

        // Setup Grafana monitoring panel
        // 直接使用head节点作为greptimedb端点
        if let Some(head_node) = cluster_config
            .nodes
            .iter()
            .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head))
        {
            let greptimedb_endpoint = format!(
                "http://{}:{}",
                head_node.get_ip()?,
                cluster_config.task_spec.monitoring.greptimedb.port
            );

            let grafana_manager = GrafanaManager::new(
                cluster_config.task_spec.monitoring.grafana.port,
                greptimedb_endpoint,
            );

            grafana_manager.setup_grafana(cluster_name).await?;
        } else {
            tracing::warn!("No head node found for monitoring setup");
        }

        // Start master agent with monitoring
        // TODO: Implement master agent creation logic
        // let mut master_agent = self.create_master_agent(cluster_config).await?;
        // master_agent.start().await?;

        info!("Monitoring setup completed for cluster: {}", cluster_name);
        Ok(())
    }

    async fn initialize_ssh_managers(&mut self, cluster_config: &ClusterConfig) -> Result<()> {
        for node in &cluster_config.nodes {
            // Parse SSH URL to extract host and port
            let ssh_parts: Vec<&str> = node.ssh_url.split(':').collect();
            let host = ssh_parts[0].to_string();

            // 用第一个用户信息
            let (username, password) = if let Some(user) = node.users.first() {
                (user.userid.clone(), Some(user.password.clone()))
            } else {
                ("root".to_string(), None)
            };

            let ssh_manager = SSHManager::new_with_password(
                host.clone(),
                username,
                None, // SSH key path
                password,
            );
            self.ssh_managers.insert(host, ssh_manager);
        }
        Ok(())
    }

    async fn prepare_dependencies(&self) -> Result<()> {
        // 复制 libcrypto.so.1.1 到待上传目录（如存在）
        let libcrypto_src = "target/libcrypto.so.1.1";
        let libcrypto_dst = "/tmp/nokube-remote-lib/libcrypto.so.1.1";
        if std::path::Path::new(libcrypto_src).exists() {
            std::fs::copy(libcrypto_src, libcrypto_dst)?;
        }
        info!("Preparing binary dependencies and resources");

        // This would involve:
        // 1. Downloading required binary dependencies
        // 2. Preparing installation scripts
        // 3. Packaging remote_lib for distribution

        // Create remote_lib directory structure
        std::fs::create_dir_all("/tmp/nokube-remote-lib")?;
        // 复制二进制文件到待上传目录
        std::fs::copy("target/nokube", "/tmp/nokube-remote-lib/nokube")?;
        // 复制 libssl.so.1.1 到待上传目录（如存在）
        let libssl_src = "target/libssl.so.1.1";
        let libssl_dst = "/tmp/nokube-remote-lib/libssl.so.1.1";
        if std::path::Path::new(libssl_src).exists() {
            std::fs::copy(libssl_src, libssl_dst)?;
        }

        // Create dependency installation script
        let install_script = r#"#!/bin/bash
set -e

echo "Installing nokube dependencies..."

# Update package lists
apt-get update

# Install Python dependencies
pip3 install requests psutil docker

# Install system dependencies
apt-get install -y htop iotop net-tools

echo "Dependencies installed successfully"
"#;
        std::fs::write(
            "/tmp/nokube-remote-lib/install_dependencies.py",
            install_script,
        )?;

        info!("Dependencies prepared");
        Ok(())
    }

    async fn deploy_node_agents(&self, cluster_config: &ClusterConfig) -> Result<()> {
        info!("Deploying agents to cluster nodes");

        use base64::{engine::general_purpose, Engine as _};
        use serde_json::json;
        for node in &cluster_config.nodes {
            // Parse SSH URL to extract host
            let ssh_parts: Vec<&str> = node.ssh_url.split(':').collect();
            let host = ssh_parts[0];

            if let Some(ssh_manager) = self.ssh_managers.get(host) {
                // 上传 remote_lib 目录（不包含 cluster config）
                ssh_manager
                    .upload_directory("/tmp/nokube-remote-lib", "/opt/nokube-remote-lib", true)
                    .await?;

                // 上传 nokube config 到远程用户配置路径
                let remote_config_path = "/home/pa/.nokube/config.yaml"; // 可根据目标用户调整
                ssh_manager
                    .upload_file("/home/pa/.nokube/config.yaml", remote_config_path)
                    .await?;

                // 构造参数对象并 base64 编码
                let extra_params = json!({
                    "cluster_name": cluster_config.cluster_name,
                    "node_id": node.name,
                    // "etcd_endpoints" 字段已移除，避免构建错误
                });
                let extra_params_str = serde_json::to_string(&extra_params).unwrap_or_default();
                let extra_params_b64 = general_purpose::STANDARD.encode(extra_params_str);

                // 远程执行 agent，传递 base64 参数
                let cmd = format!(
                    "LD_LIBRARY_PATH=/opt/nokube-remote-lib /opt/nokube-remote-lib/nokube agent-command --config-path {} --extra-params {}",
                    remote_config_path,
                    extra_params_b64
                );
                ssh_manager.execute_command(&cmd, true, true).await?;

                info!("Agent deployed successfully on node: {}", node.name);
            } else {
                anyhow::bail!("SSH manager not found for node: {}", node.name);
            }
        }

        Ok(())
    }

    async fn update_node_configurations(&self, cluster_config: &ClusterConfig) -> Result<()> {
        info!("Updating node configurations");

        for node in &cluster_config.nodes {
            // Parse SSH URL to extract host
            let ssh_parts: Vec<&str> = node.ssh_url.split(':').collect();
            let host = ssh_parts[0];

            if let Some(ssh_manager) = self.ssh_managers.get(host) {
                // Create node configuration file
                let node_config_json = serde_json::to_string_pretty(node)?;
                std::fs::write("/tmp/node_config.json", node_config_json)?;

                // Upload configuration
                ssh_manager
                    .upload_file("/tmp/node_config.json", "/opt/nokube-agent/config.json")
                    .await?;

                // Restart agent service
                ssh_manager
                    .execute_command("systemctl restart nokube-agent", true, false)
                    .await
                    .ok();

                info!("Configuration updated for node: {}", node.name);
            }
        }

        Ok(())
    }

    async fn deploy_bound_services(&self, cluster_config: &ClusterConfig) -> Result<()> {
        use tabled::{Table, Tabled};

        info!("Deploying bound services");

        #[derive(Tabled)]
        struct ServiceRow {
            name: &'static str,
            url: String,
        }

        let mut rows = Vec::new();
        if cluster_config.task_spec.monitoring.enabled {
            let grafana_port = cluster_config.task_spec.monitoring.grafana.port;
            if let Some(head_node) = cluster_config
                .nodes
                .iter()
                .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head))
            {
                let host = head_node.get_ip()?;
                let grafana_url = format!("http://{}:{}", host, grafana_port);
                rows.push(ServiceRow {
                    name: "grafana",
                    url: grafana_url,
                });

                let greptimedb_port = cluster_config.task_spec.monitoring.greptimedb.port;
                let greptimedb_url = format!("http://{}:{}", host, greptimedb_port);
                rows.push(ServiceRow {
                    name: "greptimedb",
                    url: greptimedb_url,
                });

                // 实际重启绑定服务 - 按照 .cursorrules 的要求
                info!("Stopping and restarting bound services (grafana, greptimedb) with updated configurations...");
                
                // 创建 GrafanaManager 并重新配置
                let greptimedb_endpoint = format!("http://{}:{}", host, greptimedb_port);
                let grafana_manager = GrafanaManager::new(grafana_port, greptimedb_endpoint);
                
                // 先停止现有的 Grafana 服务
                if let Err(e) = grafana_manager.stop_grafana().await {
                    tracing::warn!("Failed to stop existing Grafana service: {}", e);
                }
                
                // 重新启动 Grafana 并配置数据源
                grafana_manager.setup_grafana(&cluster_config.cluster_name).await?;
                
                info!("Successfully restarted and reconfigured Grafana with updated node IP: {}", host);
            }
        }

        if !rows.is_empty() {
            let table = Table::new(rows).to_string();
            info!("\n{}", table);
        }

        Ok(())
    }
}
