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

        // Prepare local build artifacts (binary, libs, docker image tar)
        self.prepare_dependencies().await?;

        // Publish artifacts to head node HTTP server directory and start server if needed
        self.publish_artifacts_to_httpserver(&cluster_config).await?;

        // Deploy agents to all nodes (nodes will download artifacts from HTTP server)
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
        info!("Preparing binary dependencies and Docker image");

        // 1. 构建包含Docker的nokube镜像
        self.build_nokube_docker_image().await?;

        // 2. 准备传统的远程lib目录结构（兼容性）
        let local_staging_dir = "./tmp/nokube-remote-lib";
        std::fs::create_dir_all(local_staging_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create directory {}: {}", local_staging_dir, e))?;

        // 复制SSL库（如存在）
        for lib in &["libcrypto.so.1.1", "libssl.so.1.1"] {
            let lib_src = format!("target/{}", lib);
            let lib_dst = format!("{}/{}", local_staging_dir, lib);
            if std::path::Path::new(&lib_src).exists() {
                std::fs::copy(&lib_src, &lib_dst)
                    .map_err(|e| anyhow::anyhow!("Failed to copy {} to {}: {}", lib_src, lib_dst, e))?;
            }
        }

        // 复制二进制文件
        let nokube_dst = format!("{}/nokube", local_staging_dir);
        std::fs::copy("target/nokube", &nokube_dst)
            .map_err(|e| anyhow::anyhow!("Failed to copy target/nokube to {}: {}", nokube_dst, e))?;

        info!("Dependencies and Docker image prepared");
        Ok(())
    }

    /// Upload artifacts to head node's HTTP server directory and ensure the server is running.
    async fn publish_artifacts_to_httpserver(&mut self, cluster_config: &ClusterConfig) -> Result<()> {
        use std::path::Path;
        info!("Publishing artifacts to head node HTTP server directory");

        // Resolve head node and http server config
        let head = cluster_config
            .nodes
            .iter()
            .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head))
            .ok_or_else(|| anyhow::anyhow!("Head node not found in cluster"))?;

        let http_cfg = &cluster_config.task_spec.monitoring.httpserver;

        let ssh_key = &head.ssh_url;
        let ssh = self
            .ssh_managers
            .get(ssh_key)
            .ok_or_else(|| anyhow::anyhow!("SSH manager for head node not initialized"))?;

        let workspace = head.get_workspace()?;
        let host_dir = format!("{}/{}", workspace, crate::config::cluster_config::HTTP_SERVER_MOUNT_SUBPATH);
        let releases_dir = format!("{}/releases/nokube", host_dir);

        // Ensure directory structure exists on head node
        ssh.execute_command(&format!("mkdir -p {} {}/bin {}/lib", releases_dir, releases_dir, releases_dir), true, true).await
            .map_err(|e| anyhow::anyhow!("Failed to create HTTP server directories on head node: {}", e))?;

        // Upload docker image tar to releases
        let local_image_tar = "./tmp/nokube-image.tar";
        if Path::new(local_image_tar).exists() {
            let remote_image_tar = format!("{}/nokube-image.tar", releases_dir);
            info!("Uploading nokube-image.tar to head HTTP dir");
            ssh.upload_file(local_image_tar, &remote_image_tar).await
                .map_err(|e| anyhow::anyhow!("Failed to upload image tar to head: {}", e))?;
        } else {
            info!("No docker image tar found; skipping upload: {}", local_image_tar);
        }

        // Upload binary and libs
        let local_staging_dir = "./tmp/nokube-remote-lib";
        let remote_bin = format!("{}/bin/nokube", releases_dir);
        let remote_lib_dir = format!("{}/lib", releases_dir);

        // Upload nokube binary
        ssh.upload_file("target/nokube", &remote_bin).await
            .map_err(|e| anyhow::anyhow!("Failed to upload nokube binary to head: {}", e))?;
        // Upload libs if present
        for lib in &["libcrypto.so.1.1", "libssl.so.1.1"] {
            let src = format!("{}/{}", local_staging_dir, lib);
            if Path::new(&src).exists() {
                let dst = format!("{}/{}", remote_lib_dir, lib);
                let _ = ssh.upload_file(&src, &dst).await;
            }
        }

        // Create a simple manifest file
        let manifest_local = "./tmp/nokube-artifacts-manifest.json";
        let manifest = serde_json::json!({
            "version": "latest",
            "files": {
                "bin": ["bin/nokube"],
                "lib": ["lib/libcrypto.so.1.1", "lib/libssl.so.1.1"],
                "image": "nokube-image.tar"
            }
        });
        std::fs::write(manifest_local, serde_json::to_string_pretty(&manifest)?)
            .map_err(|e| anyhow::anyhow!("Failed to write local manifest: {}", e))?;
        let remote_manifest = format!("{}/manifest.json", releases_dir);
        let _ = ssh.upload_file(manifest_local, &remote_manifest).await;

        // Ensure HTTP server container is running on head (best-effort)
        let start_cmd = format!(
            "sh -lc 'set -e; mkdir -p {host_dir}; (docker rm -f nokube-httpserver >/dev/null 2>&1 || true); \
             docker run -d --name nokube-httpserver -p {port}:8080 -v {host_dir}:/srv/http:ro python:3.10-slim \
             python -m http.server 8080 --directory /srv/http'",
            host_dir = host_dir,
            port = http_cfg.port
        );
        ssh.execute_command(&start_cmd, true, true).await
            .map_err(|e| anyhow::anyhow!("Failed to start HTTP server on head node: {}", e))?;

        let head_ip = head.get_ip()?;
        info!("Artifacts published to head HTTP server directory: {}", releases_dir);
        info!("HTTP server base URL: http://{}:{}", head_ip, http_cfg.port);
        Ok(())
    }

    async fn build_nokube_docker_image(&self) -> Result<()> {
        info!("Building Docker-enabled nokube image");

        // 创建临时Dockerfile目录
        let dockerfile_dir = "./tmp/nokube-docker-build";
        std::fs::create_dir_all(dockerfile_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create Dockerfile directory: {}", e))?;

        // 创建Dockerfile内容
        let dockerfile_content = r#"FROM ubuntu:22.04

# 设置非交互模式和时区
ENV DEBIAN_FRONTEND=noninteractive
ENV TZ=UTC

# 更新软件包并安装Docker和其他必要工具
RUN apt-get update && apt-get install -y \
    docker.io \
    ca-certificates \
    curl \
    wget \
    net-tools \
    htop \
    iotop \
    python3 \
    python3-pip \
    && pip3 install requests psutil docker \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# 复制nokube二进制和SSL库
COPY target/nokube /usr/local/bin/nokube
COPY target/lib*.so.1.1 /usr/local/lib/

# 设置权限和环境变量
RUN chmod +x /usr/local/bin/nokube
ENV LD_LIBRARY_PATH=/usr/local/lib:$LD_LIBRARY_PATH
ENV PATH=/usr/local/bin:$PATH

# 设置工作目录
WORKDIR /root

# 默认命令
CMD ["sh"]
"#;

        let dockerfile_path = format!("{}/Dockerfile", dockerfile_dir);
        std::fs::write(&dockerfile_path, dockerfile_content)
            .map_err(|e| anyhow::anyhow!("Failed to write Dockerfile: {}", e))?;

        // 构建Docker镜像
        info!("Building nokube Docker image...");
        let build_result = std::process::Command::new("sudo")
            .args(&[
                "docker", "build",
                "-t", "nokube:latest",
                "-f", &dockerfile_path,
                "."
            ])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to execute docker build: {}", e))?;

        if !build_result.status.success() {
            let error_msg = String::from_utf8_lossy(&build_result.stderr);
            return Err(anyhow::anyhow!("Docker build failed: {}", error_msg));
        }

        info!("Docker image built successfully");

        // 导出Docker镜像为tar包
        info!("Exporting Docker image to tar file...");
        let export_result = std::process::Command::new("sudo")
            .args(&[
                "docker", "save",
                "-o", "./tmp/nokube-image.tar",
                "nokube:latest"
            ])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to execute docker save: {}", e))?;

        if !export_result.status.success() {
            let error_msg = String::from_utf8_lossy(&export_result.stderr);
            return Err(anyhow::anyhow!("Docker export failed: {}", error_msg));
        }

        // 修复tar文件权限 - 获取当前用户
        let current_user = std::env::var("USER").unwrap_or_else(|_| "pa".to_string());
        let chown_result = std::process::Command::new("sudo")
            .args(&["chown", &format!("{}:{}", current_user, current_user), "./tmp/nokube-image.tar"])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to execute chown: {}", e))?;

        if !chown_result.status.success() {
            let error_msg = String::from_utf8_lossy(&chown_result.stderr);
            return Err(anyhow::anyhow!("Failed to change tar file ownership: {}", error_msg));
        }

        info!("Docker image exported to ./tmp/nokube-image.tar");
        Ok(())
    }

    async fn deploy_node_agents(&self, cluster_config: &ClusterConfig) -> Result<()> {
        info!("Deploying agents to cluster nodes");

        use base64::{engine::general_purpose, Engine as _};
        use serde_json::json;
        // Resolve head http server
        let head = cluster_config
            .nodes
            .iter()
            .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head))
            .ok_or_else(|| anyhow::anyhow!("Head node not found in cluster"))?;
        let head_ip = head.get_ip()?;
        let http_cfg = &cluster_config.task_spec.monitoring.httpserver;
        let http_base = format!("http://{}:{}/releases/nokube", head_ip, http_cfg.port);

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
                
                // Download artifacts from head HTTP server
                // 1) Download Docker image tar and load
                let remote_image_path = format!("{}/nokube-image.tar", workspace);
                let dl_image_cmd = format!(
                    "sh -lc 'set -e; (curl -fsSL {base}/nokube-image.tar -o {img} || wget -qO {img} {base}/nokube-image.tar); sudo docker load -i {img}'",
                    base = http_base,
                    img = remote_image_path
                );
                info!("Downloading and loading Docker image on node: {}", node.name);
                ssh_manager.execute_command(&dl_image_cmd, true, true).await
                    .map_err(|e| anyhow::anyhow!("Failed to download/load Docker image on node {}: {}", node.name, e))?;

                // 2) Download nokube binary and optional libs to remote_lib
                let nokube_binary_path = format!("{}/nokube", remote_lib_path);
                let dl_bin_cmd = format!(
                    "sh -lc 'set -e; mkdir -p {rlib}; (curl -fsSL {base}/bin/nokube -o {bin} || wget -qO {bin} {base}/bin/nokube); chmod +x {bin}'",
                    rlib = remote_lib_path,
                    base = http_base,
                    bin = nokube_binary_path
                );
                info!("Downloading nokube binary on node: {}", node.name);
                ssh_manager.execute_command(&dl_bin_cmd, true, true).await
                    .map_err(|e| anyhow::anyhow!("Failed to download nokube binary on node {}: {}", node.name, e))?;

                // Optional libs
                let dl_libs_cmd = format!(
                    "sh -lc '(curl -fsSL {base}/lib/libcrypto.so.1.1 -o {rlib}/libcrypto.so.1.1 || wget -qO {rlib}/libcrypto.so.1.1 {base}/lib/libcrypto.so.1.1 || true); \
                               (curl -fsSL {base}/lib/libssl.so.1.1 -o {rlib}/libssl.so.1.1 || wget -qO {rlib}/lib/libssl.so.1.1 {base}/lib/libssl.so.1.1 || true)'",
                    base = http_base,
                    rlib = remote_lib_path
                );
                let _ = ssh_manager.execute_command(&dl_libs_cmd, true, true).await;

                // 获取目标用户信息
                let target_user = node.users.first()
                    .ok_or_else(|| anyhow::anyhow!("No user configuration found for node: {}", node.name))?;
                
                // 构建全局配置路径（将被挂载到容器标准路径）
                let remote_config_dir = format!("{}/config", workspace);
                let remote_config_path = format!("{}/config.yaml", remote_config_dir);
                
                // 创建全局配置目录
                let create_config_dir_cmd = format!("mkdir -p {}", remote_config_dir);
                ssh_manager.execute_command(&create_config_dir_cmd, true, true).await
                    .map_err(|e| anyhow::anyhow!("Failed to create config directory on node {}: {}", node.name, e))?;
                
                // 上传 nokube config 到全局配置路径（将被挂载到容器内）
                // 使用 ConfigManager 统一的解析/生成逻辑
                if let Some(local_cfg) = crate::config::ConfigManager::try_get_existing_local_config_path() {
                    ssh_manager
                        .upload_file(local_cfg.to_str().unwrap(), &remote_config_path)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to upload config file to {}: {}", remote_config_path, e))?;
                } else {
                    // 本地不存在任何配置文件：根据当前 ConfigManager 生成最小配置
                    let yaml = self.config_manager.generate_minimal_config_yaml();
                    // 写入到临时文件再上传
                    let tmp_dir = "./tmp";
                    std::fs::create_dir_all(tmp_dir)
                        .map_err(|e| anyhow::anyhow!("Failed to create tmp dir {}: {}", tmp_dir, e))?;
                    let tmp_cfg_path = format!("{}/generated_nokube_config.yaml", tmp_dir);
                    std::fs::write(&tmp_cfg_path, yaml)
                        .map_err(|e| anyhow::anyhow!("Failed to write temp config {}: {}", tmp_cfg_path, e))?;
                    ssh_manager
                        .upload_file(&tmp_cfg_path, &remote_config_path)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to upload generated config to {}: {}", remote_config_path, e))?;
                }

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
                    "LD_LIBRARY_PATH={remote_lib} {remote_lib}/nokube agent-command --extra-params {}",
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
                // 使用全局配置路径
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

                // Upload configuration to global config directory
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

                // HTTP server link (served from workspace subdir)
                let http_url = format!("http://{}:{}", host, cluster_config.task_spec.monitoring.httpserver.port);
                rows.push(ServiceRow { name: "httpserver", url: http_url });

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
