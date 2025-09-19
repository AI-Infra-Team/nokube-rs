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

    pub async fn deploy_or_update(
        &mut self,
        cluster_name: &str,
        deployment_version: &str,
    ) -> Result<()> {
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
        self.publish_artifacts_to_httpserver(&cluster_config)
            .await?;

        // Deploy agents to all nodes (nodes will download artifacts from HTTP server)
        self.deploy_node_agents(&cluster_config, deployment_version)
            .await?;

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
        std::fs::create_dir_all(local_staging_dir).map_err(|e| {
            anyhow::anyhow!("Failed to create directory {}: {}", local_staging_dir, e)
        })?;

        // 复制SSL库（如存在）
        for lib in &["libcrypto.so.1.1", "libssl.so.1.1"] {
            let lib_src = format!("target/{}", lib);
            let lib_dst = format!("{}/{}", local_staging_dir, lib);
            if std::path::Path::new(&lib_src).exists() {
                std::fs::copy(&lib_src, &lib_dst).map_err(|e| {
                    anyhow::anyhow!("Failed to copy {} to {}: {}", lib_src, lib_dst, e)
                })?;
            }
        }

        // 复制二进制文件
        let nokube_dst = format!("{}/nokube", local_staging_dir);
        std::fs::copy("target/nokube", &nokube_dst).map_err(|e| {
            anyhow::anyhow!("Failed to copy target/nokube to {}: {}", nokube_dst, e)
        })?;

        info!("Dependencies and Docker image prepared");
        Ok(())
    }

    /// Upload artifacts to head node's HTTP server directory and ensure the server is running.
    async fn publish_artifacts_to_httpserver(
        &mut self,
        cluster_config: &ClusterConfig,
    ) -> Result<()> {
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
        let host_dir = format!(
            "{}/{}",
            workspace,
            crate::config::cluster_config::HTTP_SERVER_MOUNT_SUBPATH
        );
        let releases_dir = format!("{}/releases/nokube", host_dir);

        // Ensure directory structure exists on head node
        ssh.execute_command(
            &format!(
                "mkdir -p {} {}/bin {}/lib",
                releases_dir, releases_dir, releases_dir
            ),
            true,
            true,
        )
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to create HTTP server directories on head node: {}",
                e
            )
        })?;

        // Upload docker image tar to releases
        let local_image_tar = "./tmp/nokube-image.tar";
        if Path::new(local_image_tar).exists() {
            let remote_image_tar = format!("{}/nokube-image.tar", releases_dir);
            info!("Uploading nokube-image.tar to head HTTP dir");
            ssh.upload_file(local_image_tar, &remote_image_tar)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to upload image tar to head: {}", e))?;
        } else {
            info!(
                "No docker image tar found; skipping upload: {}",
                local_image_tar
            );
        }

        // Upload binary and libs
        let local_staging_dir = "./tmp/nokube-remote-lib";
        let remote_bin = format!("{}/bin/nokube", releases_dir);
        let remote_lib_dir = format!("{}/lib", releases_dir);

        // Upload nokube binary
        ssh.upload_file("target/nokube", &remote_bin)
            .await
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
        ssh.execute_command(&start_cmd, true, true)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to start HTTP server on head node: {}", e))?;

        let head_ip = head.get_ip()?;
        info!(
            "Artifacts published to head HTTP server directory: {}",
            releases_dir
        );
        info!("HTTP server base URL: http://{}:{}", head_ip, http_cfg.port);
        Ok(())
    }

    async fn build_nokube_docker_image(&self) -> Result<()> {
        use std::path::Path;

        // Resolve build configuration path (prefer project configs over 3rd_party)
        let config_env = std::env::var("NOKUBE_IMAGE_CONFIG").ok();
        let mut tried: Vec<String> = Vec::new();
        let cfg_path_env = if let Some(p) = config_env {
            tried.push(format!("env:NOKUBE_IMAGE_CONFIG={}", p));
            if Path::new(&p).exists() {
                Some(p)
            } else {
                None
            }
        } else {
            None
        };

        let config_path = if let Some(p) = cfg_path_env {
            p
        } else {
            let candidates = vec![
                "configs/images/nokube_base.yaml",
                "./configs/images/nokube_base.yaml",
            ];
            let mut found = None;
            for c in candidates {
                tried.push(c.to_string());
                if Path::new(c).exists() {
                    found = Some(c.to_string());
                    break;
                }
            }
            found.ok_or_else(|| {
                anyhow::anyhow!("Build configuration not found. Tried: {}", tried.join(", "))
            })?
        };
        // Use 3rd_party orchestrator (upgraded to snapshot mode internally)
        info!(
            "Building nokube image via dependency_img_build orchestrator, config={}",
            config_path
        );
        let cli_path = "3rd_party/dependency_img_build/cli.py";
        if !Path::new(cli_path).exists() {
            anyhow::bail!("Build orchestrator CLI not found at {}", cli_path);
        }
        std::fs::create_dir_all("./tmp")
            .map_err(|e| anyhow::anyhow!("Failed to create ./tmp: {}", e))?;
        let status = std::process::Command::new("python3")
            .args(&[cli_path, "build", "-c", &config_path])
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .map_err(|e| anyhow::anyhow!("Failed to execute orchestrator: {}", e))?;
        if !status.success() {
            let code = status.code().unwrap_or(-1);
            anyhow::bail!(
                "Image build orchestrator exited with code {} (config={}).",
                code,
                config_path
            );
        }
        info!("Base image built via orchestrator (tag: nokube:base)");

        // Assemble final image by injecting the compiled binary without writing a Dockerfile
        // 1) create a temp container from base image
        let container_name = "nokube-image-assembler";
        // Best-effort cleanup of any stale container
        let _ = std::process::Command::new("sudo")
            .args(&["docker", "rm", "-f", container_name])
            .output();

        let create_out = std::process::Command::new("sudo")
            .args(&[
                "docker",
                "create",
                "--name",
                container_name,
                "nokube:base",
                "sleep",
                "infinity",
            ])
            .output()
            .map_err(|e| {
                anyhow::anyhow!("Failed to create temp container from nokube:base: {}", e)
            })?;
        if !create_out.status.success() {
            let stderr = String::from_utf8_lossy(&create_out.stderr);
            anyhow::bail!("Failed to create temp container: {}", stderr);
        }

        // 2) copy binary (required for container entry)
        let bin_src = Path::new("target/nokube");
        if !bin_src.exists() {
            // Clean up the temp container before bailing
            let _ = std::process::Command::new("sudo")
                .args(&["docker", "rm", "-f", container_name])
                .output();
            anyhow::bail!(
                "Binary not found at {} — ensure 'target/nokube' is built",
                bin_src.display()
            );
        }
        let cp_out = std::process::Command::new("sudo")
            .args(&[
                "docker",
                "cp",
                bin_src.to_str().unwrap(),
                &format!("{}:/usr/local/bin/nokube", container_name),
            ])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to docker cp binary: {}", e))?;
        if !cp_out.status.success() {
            let stderr = String::from_utf8_lossy(&cp_out.stderr);
            // Attempt cleanup
            let _ = std::process::Command::new("sudo")
                .args(&["docker", "rm", "-f", container_name])
                .output();
            anyhow::bail!("Failed to copy binary into container: {}", stderr);
        }
        // Note: docker cp preserves executable bit from host. Cargo builds binaries with +x by default,
        // so we avoid needing to exec inside the container (which would require it running).

        // 3) commit as nokube:latest
        let commit_out = std::process::Command::new("sudo")
            .args(&["docker", "commit", container_name, "nokube:latest"])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to commit container to nokube:latest: {}", e))?;
        if !commit_out.status.success() {
            let stderr = String::from_utf8_lossy(&commit_out.stderr);
            let _ = std::process::Command::new("sudo")
                .args(&["docker", "rm", "-f", container_name])
                .output();
            anyhow::bail!("Failed to commit final image: {}", stderr);
        }

        // Remove temp container
        let _ = std::process::Command::new("sudo")
            .args(&["docker", "rm", "-f", container_name])
            .output();

        info!("Final image assembled and tagged: nokube:latest");

        // Export image tar (same as before)
        info!("Exporting Docker image to tar file...");
        let export_result = std::process::Command::new("sudo")
            .args(&[
                "docker",
                "save",
                "-o",
                "./tmp/nokube-image.tar",
                "nokube:latest",
            ])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to execute docker save: {}", e))?;

        if !export_result.status.success() {
            let error_msg = String::from_utf8_lossy(&export_result.stderr);
            return Err(anyhow::anyhow!("Docker export failed: {}", error_msg));
        }

        // chown tar to current user for convenience
        let current_user = std::env::var("USER").unwrap_or_else(|_| "pa".to_string());
        let chown_result = std::process::Command::new("sudo")
            .args(&[
                "chown",
                &format!("{}:{}", current_user, current_user),
                "./tmp/nokube-image.tar",
            ])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to execute chown: {}", e))?;
        if !chown_result.status.success() {
            let error_msg = String::from_utf8_lossy(&chown_result.stderr);
            return Err(anyhow::anyhow!(
                "Failed to change tar file ownership: {}",
                error_msg
            ));
        }

        info!("Docker image exported to ./tmp/nokube-image.tar");
        Ok(())
    }

    /// Snapshot-mode image build: avoid `docker build`, use `docker run` + `commit` with full logs.
    async fn build_nokube_image_snapshot(&self, config_path: &str) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct HeavySetup {
            apt_packages: Option<Vec<String>>,
            pip_packages: Option<Vec<String>>,
        }
        #[derive(serde::Deserialize)]
        struct SnapshotCfg {
            base_image: String,
            heavy_setup: Option<HeavySetup>,
        }

        let cfg_str = std::fs::read_to_string(config_path)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", config_path, e))?;
        let cfg: SnapshotCfg = serde_yaml::from_str(&cfg_str)
            .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", config_path, e))?;

        let base = cfg.base_image;
        let apt = cfg
            .heavy_setup
            .as_ref()
            .and_then(|h| h.apt_packages.clone())
            .unwrap_or_default();
        let pip = cfg
            .heavy_setup
            .as_ref()
            .and_then(|h| h.pip_packages.clone())
            .unwrap_or_default();

        let builder = "nokube-snap-builder";

        fn run(cmd: &mut std::process::Command, desc: &str) -> anyhow::Result<()> {
            let out = cmd
                .output()
                .map_err(|e| anyhow::anyhow!("{} exec failed: {}", desc, e))?;
            if !out.status.success() {
                let s = String::from_utf8_lossy(&out.stderr);
                let o = String::from_utf8_lossy(&out.stdout);
                return Err(anyhow::anyhow!(
                    "{} failed (code {:?}):\nSTDOUT:\n{}\nSTDERR:\n{}",
                    desc,
                    out.status.code(),
                    o,
                    s
                ));
            }
            Ok(())
        }

        // Best-effort cleanup
        let _ = std::process::Command::new("sudo")
            .args(["docker", "rm", "-f", builder])
            .output();
        info!("Snapshot: pulling base image {}", base);
        run(
            std::process::Command::new("sudo").args(["docker", "pull", &base]),
            "docker pull",
        )?;

        // Start long-running container with proxy env forwarded
        let mut run_args: Vec<String> = vec![
            "docker".into(),
            "run".into(),
            "-d".into(),
            "--name".into(),
            builder.into(),
        ];
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
                    run_args.push("-e".into());
                    run_args.push(format!("{}={}", key, val));
                }
            }
        }
        run_args.push(base.clone());
        run_args.push("sleep".into());
        run_args.push("infinity".into());
        info!("Snapshot: starting builder container from {}", base);
        run(
            std::process::Command::new("sudo").args(&run_args),
            "docker run builder",
        )?;

        // APT install
        if !apt.is_empty() {
            let apt_cmd = format!("bash -lc 'apt-get update -y && DEBIAN_FRONTEND=noninteractive apt-get install -y {}'", apt.join(" "));
            info!("Snapshot: apt install {}", apt.join(","));
            run(
                std::process::Command::new("sudo")
                    .args(["docker", "exec", builder, "sh", "-lc", &apt_cmd]),
                "docker exec apt install",
            )?;
            info!("Snapshot: apt install completed");
        }

        // PIP install
        if !pip.is_empty() {
            let pip_cmd = format!(
                "bash -lc 'python3 -m pip install -U pip && pip3 install {}'",
                pip.join(" ")
            );
            info!("Snapshot: pip install {}", pip.join(","));
            run(
                std::process::Command::new("sudo")
                    .args(["docker", "exec", builder, "sh", "-lc", &pip_cmd]),
                "docker exec pip install",
            )?;
            info!("Snapshot: pip install completed");
        }

        // Commit as base
        info!("Snapshot: committing builder container to image nokube:base");
        run(
            std::process::Command::new("sudo").args(["docker", "commit", builder, "nokube:base"]),
            "docker commit",
        )?;

        // Cleanup builder
        let _ = std::process::Command::new("sudo")
            .args(["docker", "rm", "-f", builder])
            .output();
        Ok(())
    }

    async fn deploy_node_agents(
        &self,
        cluster_config: &ClusterConfig,
        deployment_version: &str,
    ) -> Result<()> {
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

                info!(
                    "Deploying to node {} with workspace: {}, storage: {}",
                    node.name, workspace, storage_path
                );

                // 创建工作空间目录
                let create_workspace_cmd = format!("mkdir -p {}", workspace);
                ssh_manager
                    .execute_command(&create_workspace_cmd, true, true)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to create workspace {} on node {}: {}",
                            workspace,
                            node.name,
                            e
                        )
                    })?;

                // 创建存储目录
                let create_storage_cmd = format!("mkdir -p {}", storage_path);
                ssh_manager
                    .execute_command(&create_storage_cmd, true, true)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to create storage path {} on node {}: {}",
                            storage_path,
                            node.name,
                            e
                        )
                    })?;

                // Download artifacts from head HTTP server
                // 1) Download Docker image tar and load
                let remote_image_path = format!("{}/nokube-image.tar", workspace);
                let dl_image_cmd = format!(
                    "sh -lc 'set -e; (curl --noproxy \"*\" -fsSL {base}/nokube-image.tar -o {img} || wget --no-proxy -qO {img} {base}/nokube-image.tar); sudo docker load -i {img}'",
                    base = http_base,
                    img = remote_image_path
                );
                info!(
                    "Downloading and loading Docker image on node: {}",
                    node.name
                );
                ssh_manager
                    .execute_command(&dl_image_cmd, true, true)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to download/load Docker image on node {}: {}",
                            node.name,
                            e
                        )
                    })?;

                // 2) Download nokube binary and optional libs to remote_lib
                let nokube_binary_path = format!("{}/nokube", remote_lib_path);
                let dl_bin_cmd = format!(
                    "sh -lc 'set -e; mkdir -p {rlib}; (curl --noproxy \"*\" -fsSL {base}/bin/nokube -o {bin} || wget --no-proxy -qO {bin} {base}/bin/nokube); chmod +x {bin}'",
                    rlib = remote_lib_path,
                    base = http_base,
                    bin = nokube_binary_path
                );
                info!("Downloading nokube binary on node: {}", node.name);
                ssh_manager
                    .execute_command(&dl_bin_cmd, true, true)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to download nokube binary on node {}: {}",
                            node.name,
                            e
                        )
                    })?;

                // Optional libs
                let dl_libs_cmd = format!(
                    "sh -lc '(curl -fsSL {base}/lib/libcrypto.so.1.1 -o {rlib}/libcrypto.so.1.1 || wget -qO {rlib}/libcrypto.so.1.1 {base}/lib/libcrypto.so.1.1 || true); \
                               (curl -fsSL {base}/lib/libssl.so.1.1 -o {rlib}/libssl.so.1.1 || wget -qO {rlib}/lib/libssl.so.1.1 {base}/lib/libssl.so.1.1 || true)'",
                    base = http_base,
                    rlib = remote_lib_path
                );
                let _ = ssh_manager.execute_command(&dl_libs_cmd, true, true).await;

                // 获取目标用户信息
                let target_user = node.users.first().ok_or_else(|| {
                    anyhow::anyhow!("No user configuration found for node: {}", node.name)
                })?;

                // 构建全局配置路径（将被挂载到容器标准路径）
                let remote_config_dir = format!("{}/config", workspace);
                let remote_config_path = format!("{}/config.yaml", remote_config_dir);

                // 创建全局配置目录
                let create_config_dir_cmd = format!("mkdir -p {}", remote_config_dir);
                ssh_manager
                    .execute_command(&create_config_dir_cmd, true, true)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to create config directory on node {}: {}",
                            node.name,
                            e
                        )
                    })?;

                // 上传 nokube config 到全局配置路径（将被挂载到容器内）
                // 使用 ConfigManager 统一的解析/生成逻辑
                if let Some(local_cfg) =
                    crate::config::ConfigManager::try_get_existing_local_config_path()
                {
                    ssh_manager
                        .upload_file(local_cfg.to_str().unwrap(), &remote_config_path)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "Failed to upload config file to {}: {}",
                                remote_config_path,
                                e
                            )
                        })?;
                } else {
                    // 本地不存在任何配置文件：根据当前 ConfigManager 生成最小配置
                    let yaml = self.config_manager.generate_minimal_config_yaml();
                    // 写入到临时文件再上传
                    let tmp_dir = "./tmp";
                    std::fs::create_dir_all(tmp_dir).map_err(|e| {
                        anyhow::anyhow!("Failed to create tmp dir {}: {}", tmp_dir, e)
                    })?;
                    let tmp_cfg_path = format!("{}/generated_nokube_config.yaml", tmp_dir);
                    std::fs::write(&tmp_cfg_path, yaml).map_err(|e| {
                        anyhow::anyhow!("Failed to write temp config {}: {}", tmp_cfg_path, e)
                    })?;
                    ssh_manager
                        .upload_file(&tmp_cfg_path, &remote_config_path)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "Failed to upload generated config to {}: {}",
                                remote_config_path,
                                e
                            )
                        })?;
                }

                // 构造参数对象并 base64 编码
                let mut extra_params = json!({
                    "cluster_name": cluster_config.cluster_name,
                    "node_id": node.name,
                    "workspace": workspace,  // 传递 workspace 路径
                    "node_ip": node.get_ip()?,  // 传递节点IP
                    "deployment_version": deployment_version,
                });

                let extra_params_str = serde_json::to_string(&extra_params).unwrap_or_default();
                let extra_params_b64 = general_purpose::STANDARD.encode(extra_params_str);

                // 远程执行 agent，传递 base64 参数
                let cmd = format!(
                    "LD_LIBRARY_PATH={remote_lib} {remote_lib}/nokube agent-command --extra-params {}",
                    extra_params_b64,
                    remote_lib = remote_lib_path
                );
                ssh_manager
                    .execute_command(&cmd, true, true)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to execute agent command on node {}: {}",
                            node.name,
                            e
                        )
                    })?;

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
                std::fs::create_dir_all(local_config_dir).map_err(|e| {
                    anyhow::anyhow!("Failed to create local config directory: {}", e)
                })?;

                let local_config_path = format!("{}/node_config.json", local_config_dir);
                let node_config_json = serde_json::to_string_pretty(node)?;
                std::fs::write(&local_config_path, node_config_json).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to write node config to {}: {}",
                        local_config_path,
                        e
                    )
                })?;

                // Create remote config directory
                let create_config_dir_cmd = format!("mkdir -p {}", config_dir);
                ssh_manager
                    .execute_command(&create_config_dir_cmd, true, true)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to create config directory {} on node {}: {}",
                            config_dir,
                            node.name,
                            e
                        )
                    })?;

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
                let http_url = format!(
                    "http://{}:{}",
                    host, cluster_config.task_spec.monitoring.httpserver.port
                );
                rows.push(ServiceRow {
                    name: "httpserver",
                    url: http_url,
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
