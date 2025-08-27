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
        // Monitoring setup is now handled by the agent during deployment
        info!("Monitoring setup will be handled by node agents during deployment");

        // Start master agent with monitoring
        // TODO: Implement master agent creation logic
        // let mut master_agent = self.create_master_agent(cluster_config).await?;
        // master_agent.start().await?;

        info!("Monitoring setup completed for cluster: {}", cluster_name);
        Ok(())
    }

    async fn initialize_ssh_managers(&mut self, cluster_config: &ClusterConfig) -> Result<()> {
        for node in &cluster_config.nodes {
            // 直接使用完整的SSH URL作为连接地址（包含端口）
            let host_with_port = node.ssh_url.clone();
            
            // 用第一个用户信息
            let (username, password) = if let Some(user) = node.users.first() {
                (user.userid.clone(), Some(user.password.clone()))
            } else {
                ("root".to_string(), None)
            };

            let ssh_manager = SSHManager::new_with_password(
                host_with_port.clone(),
                username,
                None, // SSH key path
                password,
            );
            // 使用完整URL作为key，这样在其他地方查找时能匹配
            self.ssh_managers.insert(host_with_port, ssh_manager);
        }
        Ok(())
    }

    async fn prepare_dependencies(&self) -> Result<()> {
        info!("Preparing binary dependencies and resources");

        // This would involve:
        // 1. Downloading required binary dependencies
        // 2. Preparing installation scripts
        // 3. Packaging remote_lib for distribution

        // 使用当前目录下的临时文件夹，不使用系统 /tmp
        let local_staging_dir = "./tmp/nokube-remote-lib";
        
        // Create local staging directory structure
        std::fs::create_dir_all(local_staging_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create directory {}: {}", local_staging_dir, e))?;

        // 复制 libcrypto.so.1.1 到待上传目录（如存在）
        let libcrypto_src = "target/libcrypto.so.1.1";
        let libcrypto_dst = format!("{}/libcrypto.so.1.1", local_staging_dir);
        if std::path::Path::new(libcrypto_src).exists() {
            std::fs::copy(libcrypto_src, &libcrypto_dst)
                .map_err(|e| anyhow::anyhow!("Failed to copy {} to {}: {}", libcrypto_src, libcrypto_dst, e))?;
        }
        // 复制二进制文件到待上传目录
        let nokube_dst = format!("{}/nokube", local_staging_dir);
        std::fs::copy("target/nokube", &nokube_dst)
            .map_err(|e| anyhow::anyhow!("Failed to copy target/nokube to {}: {}", nokube_dst, e))?;
        // 复制 libssl.so.1.1 到待上传目录（如存在）
        let libssl_src = "target/libssl.so.1.1";
        let libssl_dst = format!("{}/libssl.so.1.1", local_staging_dir);
        if std::path::Path::new(libssl_src).exists() {
            std::fs::copy(libssl_src, &libssl_dst)
                .map_err(|e| anyhow::anyhow!("Failed to copy {} to {}: {}", libssl_src, libssl_dst, e))?;
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
        let install_script_path = format!("{}/install_dependencies.py", local_staging_dir);
        std::fs::write(
            &install_script_path,
            install_script,
        ).map_err(|e| anyhow::anyhow!("Failed to write install script to {}: {}", install_script_path, e))?;

        info!("Dependencies prepared");
        Ok(())
    }

    async fn deploy_node_agents(&self, cluster_config: &ClusterConfig) -> Result<()> {
        info!("Deploying agents to cluster nodes");

        use base64::{engine::general_purpose, Engine as _};
        use serde_json::json;
        for node in &cluster_config.nodes {
            // 使用完整的SSH URL作为查找键
            let ssh_key = &node.ssh_url;

            if let Some(ssh_manager) = self.ssh_managers.get(ssh_key) {
                // 获取节点的 workspace 路径并构建远程lib路径
                let workspace = node.get_workspace()?;
                let remote_lib_path = format!("{}/nokube-remote-lib", workspace);
                let storage_path = node.get_storage_path()?;
                
                info!("Deploying to node {} with workspace: {}, storage: {}", node.name, workspace, storage_path);
                
                // 创建工作空间目录
                let create_workspace_cmd = format!("mkdir -p {}", workspace);
                ssh_manager.execute_command(&create_workspace_cmd, true, true).await
                    .map_err(|e| anyhow::anyhow!("Failed to create workspace {} on node {}: {}", workspace, node.name, e))?;
                
                // 创建存储目录
                let create_storage_cmd = format!("mkdir -p {}", storage_path);
                ssh_manager.execute_command(&create_storage_cmd, true, true).await
                    .map_err(|e| anyhow::anyhow!("Failed to create storage path {} on node {}: {}", storage_path, node.name, e))?;
                
                // 上传 remote_lib 目录（不包含 cluster config）
                let local_staging_dir = "./tmp/nokube-remote-lib";
                ssh_manager
                    .upload_directory(local_staging_dir, &remote_lib_path, true)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to upload remote lib to node {}: {}", node.name, e))?;
                
                // 验证上传的二进制文件
                info!("Verifying uploaded binary on node: {}", node.name);
                let verify_cmd = format!("ls -la {}/nokube", remote_lib_path);
                ssh_manager.execute_command(&verify_cmd, true, true).await
                    .map_err(|e| anyhow::anyhow!("Failed to verify binary on node {}: {}", node.name, e))?;

                // 上传 nokube config 到远程用户配置路径
                let remote_config_path = "/home/pa/.nokube/config.yaml"; // 可根据目标用户调整
                ssh_manager
                    .upload_file("/home/pa/.nokube/config.yaml", remote_config_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to upload config file /home/pa/.nokube/config.yaml to {}: {}", remote_config_path, e))?;

                // 构造参数对象并 base64 编码
                let mut extra_params = json!({
                    "cluster_name": cluster_config.cluster_name,
                    "node_id": node.name,
                    "workspace": workspace,  // 传递 workspace 路径
                    "node_ip": node.get_ip()?,  // 传递节点IP
                });
                
                // 如果是head节点且监控开启，添加Grafana配置
                if matches!(node.role, crate::config::cluster_config::NodeRole::Head) 
                    && cluster_config.task_spec.monitoring.enabled {
                    info!("Adding Grafana configuration for head node: {}", node.name);
                    let grafana_config = format!(r#"[server]
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
url = http://{}:{}
access = proxy
isDefault = true
"#, 
                        node.get_ip()?, 
                        cluster_config.task_spec.monitoring.greptimedb.port
                    );
                    
                    extra_params["grafana_config"] = json!(grafana_config);
                    extra_params["grafana_port"] = json!(cluster_config.task_spec.monitoring.grafana.port);
                    extra_params["greptimedb_port"] = json!(cluster_config.task_spec.monitoring.greptimedb.port);
                    extra_params["setup_grafana"] = json!(true);
                }
                let extra_params_str = serde_json::to_string(&extra_params).unwrap_or_default();
                let extra_params_b64 = general_purpose::STANDARD.encode(extra_params_str);

                // 远程执行 agent，传递 base64 参数
                let cmd = format!(
                    "LD_LIBRARY_PATH={remote_lib} {remote_lib}/nokube agent-command --config-path {} --extra-params {}",
                    remote_config_path,
                    extra_params_b64,
                    remote_lib = remote_lib_path
                );
                ssh_manager.execute_command(&cmd, true, true).await
                    .map_err(|e| anyhow::anyhow!("Failed to execute agent command on node {}: {}", node.name, e))?;

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
            // 使用完整的SSH URL作为查找键
            let ssh_key = &node.ssh_url;

            if let Some(ssh_manager) = self.ssh_managers.get(ssh_key) {
                let workspace = node.get_workspace()?;
                let config_dir = format!("{}/config", workspace);
                
                // Create node configuration file in local staging area
                let local_config_dir = "./tmp/config";
                std::fs::create_dir_all(local_config_dir)
                    .map_err(|e| anyhow::anyhow!("Failed to create local config directory: {}", e))?;
                
                let local_config_path = format!("{}/node_config.json", local_config_dir);
                let node_config_json = serde_json::to_string_pretty(node)?;
                std::fs::write(&local_config_path, node_config_json)
                    .map_err(|e| anyhow::anyhow!("Failed to write node config to {}: {}", local_config_path, e))?;

                // Create remote config directory
                let create_config_dir_cmd = format!("mkdir -p {}", config_dir);
                ssh_manager.execute_command(&create_config_dir_cmd, true, true).await
                    .map_err(|e| anyhow::anyhow!("Failed to create config directory {} on node {}: {}", config_dir, node.name, e))?;

                // Upload configuration
                let remote_config_path = format!("{}/node_config.json", config_dir);
                ssh_manager
                    .upload_file(&local_config_path, &remote_config_path)
                    .await?;

                // Restart agent container
                ssh_manager
                    .execute_command("docker restart nokube-agent-container", true, false)
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

                info!("Bound services (Grafana, GreptimeDB) are managed by node agents");
            }
        }

        if !rows.is_empty() {
            let table = Table::new(rows).to_string();
            info!("\n{}", table);
        }

        Ok(())
    }
}
