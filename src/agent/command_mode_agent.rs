use crate::config::cluster_config::ClusterConfig;
use anyhow::Result;
use std::process::Command;
use tracing::{error, info};

#[derive(Debug, Clone)]
pub enum DependencyType {
    Pip(String),
    Apt(String),
    Docker(String),
}

struct DependencyInstaller;

impl DependencyInstaller {
    fn new() -> Self {
        Self
    }

    async fn install_dependencies(&self, dependencies: Vec<DependencyType>) -> Result<()> {
        for dep in dependencies {
            match dep {
                DependencyType::Pip(package) => {
                    self.install_pip_package(&package).await?;
                }
                DependencyType::Apt(package) => {
                    self.install_apt_package(&package).await?;
                }
                DependencyType::Docker(image) => {
                    self.pull_docker_image(&image).await?;
                }
            }
        }
        Ok(())
    }

    async fn install_pip_package(&self, package: &str) -> Result<()> {
        info!("Installing pip package: {}", package);

        // Use explicit interpreter to invoke pip for reliability
        let output = Command::new("python3")
            .arg("-m")
            .arg("pip")
            .arg("install")
            .arg(package)
            .output()?;

        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to install pip package {}: {}", package, error_msg);
        }

        info!("Successfully installed pip package: {}", package);
        Ok(())
    }

    async fn install_apt_package(&self, package: &str) -> Result<()> {
        info!("Installing apt package: {}", package);

        let output = Command::new("apt")
            .arg("install")
            .arg("-y")
            .arg(package)
            .output()?;

        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to install apt package {}: {}", package, error_msg);
        }

        info!("Successfully installed apt package: {}", package);
        Ok(())
    }

    async fn pull_docker_image(&self, image: &str) -> Result<()> {
        info!("Pulling Docker image: {}", image);

        let pull_command = format!("docker pull {}", image);
        let output = Command::new("docker")
            .arg("pull")
            .arg(image)
            .output()
            .map_err(|e| {
                let detailed_error = crate::error::NokubeError::ServiceDeploymentFailed {
                    service: format!("image-{}", image),
                    node: "localhost".to_string(),
                    reason: format!(
                        "Failed to execute docker command '{}': {} (os error {})",
                        pull_command,
                        e,
                        e.raw_os_error().unwrap_or(-1)
                    ),
                };
                anyhow::Error::from(detailed_error)
            })?;

        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            let detailed_error = crate::error::NokubeError::ServiceDeploymentFailed {
                service: format!("image-{}", image),
                node: "localhost".to_string(),
                reason: format!(
                    "Docker command '{}' failed with exit code {}: {}",
                    pull_command,
                    output.status.code().unwrap_or(-1),
                    error_msg.trim()
                ),
            };
            return Err(anyhow::Error::from(detailed_error));
        }

        info!("Successfully pulled Docker image: {}", image);
        Ok(())
    }
}

/// 命令模式Agent：处理部署时的一次性任务
/// 执行环境配置、依赖安装、服务配置后退出
pub struct CommandModeAgent {
    dependency_installer: DependencyInstaller,
    config: ClusterConfig,
    extra_params: Option<serde_json::Value>,
}

impl CommandModeAgent {
    pub fn new(config: ClusterConfig, extra_params: Option<serde_json::Value>) -> Self {
        Self {
            dependency_installer: DependencyInstaller::new(),
            config,
            extra_params,
        }
    }

    /// 从 extra_params 获取 workspace 路径，如果没有则报错
    fn get_workspace(&self) -> Result<&str> {
        self.extra_params
            .as_ref()
            .and_then(|params| params.get("workspace"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required workspace in extra_params"))
    }

    fn get_deployment_version(&self) -> Result<&str> {
        self.extra_params
            .as_ref()
            .and_then(|params| params.get("deployment_version"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required deployment_version in extra_params"))
    }

    fn get_node_id(&self) -> Result<&str> {
        self.extra_params
            .as_ref()
            .and_then(|params| params.get("node_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required node_id in extra_params"))
    }

    pub async fn execute(&self) -> Result<()> {
        println!("[REALTIME] 🚀 Starting command mode agent execution");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        info!("Starting command mode agent execution");

        // 检查集群配置是否存在，若不存在则打印当前etcd cluster列表
        let config_manager = crate::config::ConfigManager::new().await?;
        let cluster_name = self.config.cluster_name.clone();
        let cluster_metas = config_manager.list_clusters().await.unwrap_or_default();
        let cluster_names: Vec<String> = cluster_metas
            .iter()
            .map(|m| format!("{}({:?})", m.config.cluster_name, m.deploy_status))
            .collect();
        if !cluster_names.iter().any(|n| n.starts_with(&cluster_name)) {
            error!(
                "Cluster not found: {}. Current clusters: {:?}",
                cluster_name, cluster_names
            );
            return Err(anyhow::anyhow!(format!(
                "Cluster not found: {}. Current clusters: {:?}",
                cluster_name, cluster_names
            )));
        }

        // 执行环境配置任务
        println!("[REALTIME] ⚙️ Configuring environment...");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        self.configure_environment().await?;

        // 安装必要依赖
        println!("[REALTIME] 📦 Installing dependencies...");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        self.install_dependencies().await?;

        // 配置Docker容器服务
        println!("[REALTIME] 🐳 Setting up Docker container service...");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        self.setup_docker_service().await?;

        println!("[REALTIME] ✅ Command mode agent execution completed successfully!");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        info!("Command mode agent execution completed");
        Ok(())
    }

    async fn configure_environment(&self) -> Result<()> {
        info!("Configuring environment");

        let workspace = self.get_workspace()?;
        let agent_dir = format!("{}/agent", workspace);
        let log_dir = format!("{}/logs", workspace);
        let config_dir = format!("{}/config", workspace);

        // 创建workspace内的目录结构
        let mkdir_result = std::process::Command::new("mkdir")
            .args(&["-p", &agent_dir, &log_dir, &config_dir])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to execute mkdir command: {}", e))?;

        if !mkdir_result.status.success() {
            let error_msg = String::from_utf8_lossy(&mkdir_result.stderr);
            anyhow::bail!("Failed to create workspace directories: {}", error_msg);
        }

        info!(
            "Environment configuration completed with workspace: {}",
            workspace
        );

        // 在环境准备后优先配置APT源（若在集群配置中提供了apt_sources）
        self.configure_apt_sources_if_provided().await?;
        Ok(())
    }

    /// 如果Node配置中提供了apt源，写入到系统并执行apt-get update
    async fn configure_apt_sources_if_provided(&self) -> Result<()> {
        use crate::config::cluster_config::NodeRole;
        // 选择当前节点配置：优先Head节点（命令模式通常在Head上执行）
        if let Some(node) = self
            .config
            .nodes
            .iter()
            .find(|n| matches!(n.role, NodeRole::Head))
        {
            if let Some(apt_cfg) = &node.apt_sources {
                info!("Configuring APT sources from cluster config");

                // 写入 /etc/apt/sources.list
                if let Some(content) = &apt_cfg.sources_list {
                    std::fs::write("/etc/apt/sources.list", content).map_err(|e| {
                        anyhow::anyhow!("Failed to write /etc/apt/sources.list: {}", e)
                    })?;
                }

                // 写入 /etc/apt/sources.list.d/*.list 文件
                if let Some(files) = &apt_cfg.sources_list_d {
                    std::fs::create_dir_all("/etc/apt/sources.list.d").map_err(|e| {
                        anyhow::anyhow!("Failed to create /etc/apt/sources.list.d: {}", e)
                    })?;
                    for (name, content) in files {
                        let path = if name.ends_with(".list") {
                            format!("/etc/apt/sources.list.d/{}", name)
                        } else {
                            format!("/etc/apt/sources.list.d/{}.list", name)
                        };
                        std::fs::write(&path, content)
                            .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path, e))?;
                    }
                }

                // 更新索引
                let output = Command::new("apt-get")
                    .arg("update")
                    .output()
                    .map_err(|e| anyhow::anyhow!("Failed to execute apt-get update: {}", e))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("apt-get update failed: {}", stderr);
                }
                info!("APT sources configured and apt-get update completed");
            }
        }
        Ok(())
    }

    async fn install_dependencies(&self) -> Result<()> {
        info!("Installing dependencies");

        // Host-side dependencies kept minimal. Python packages like 'psutil' are
        // pre-baked into the nokube container image and are not required on host.
        // This avoids runtime network installs and speeds up deployment.
        let dependencies = vec![
            DependencyType::Apt("htop".to_string()),
            DependencyType::Apt("iotop".to_string()),
            DependencyType::Apt("net-tools".to_string()),
        ];

        self.dependency_installer
            .install_dependencies(dependencies)
            .await?;

        info!("Dependencies installation completed");
        Ok(())
    }

    async fn setup_docker_service(&self) -> Result<()> {
        info!("Setting up Docker container service");

        // 获取当前用户信息（从环境变量获取，如果缺失则报错）
        let current_user = std::env::var("USER").map_err(|_| {
            anyhow::anyhow!("USER environment variable not set - cannot determine user")
        })?;
        let home_dir = std::env::var("HOME").map_err(|_| {
            anyhow::anyhow!("HOME environment variable not set - cannot determine home directory")
        })?;

        // 获取workspace并构建宿主机配置路径（将被挂载到容器标准路径）
        let workspace = self.get_workspace()?;
        let host_config_dir = format!("{}/config", workspace);
        let host_config_path = format!("{}/config.yaml", host_config_dir);

        // 创建包含cluster_name的extra_params
        let cluster_params = serde_json::json!({
            "cluster_name": self.config.cluster_name,
            "deployment_version": self.get_deployment_version()?,
            "node_id": self.get_node_id()?,
        });
        let extra_params = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            cluster_params.to_string().as_bytes(),
        );

        // 停止可能存在的旧容器
        let stop_result = std::process::Command::new("sudo")
            .args(&["docker", "stop", "nokube-agent-container"])
            .output();
        if let Ok(output) = stop_result {
            if output.status.success() {
                info!("Stopped existing nokube-agent container");
            } else {
                info!("No existing container to stop");
            }
        }

        // 删除旧容器
        let remove_result = std::process::Command::new("sudo")
            .args(&["docker", "rm", "nokube-agent-container"])
            .output();
        if let Ok(output) = remove_result {
            if output.status.success() {
                info!("Removed existing nokube-agent container");
            } else {
                info!("No existing container to remove");
            }
        }

        // 启动新的Docker容器作为服务
        let workspace = self.get_workspace()?;
        let remote_lib_path = format!("{}/nokube-remote-lib", workspace);

        let start_result = std::process::Command::new("sudo")
            .args(&[
                "docker",
                "run",
                "-d",
                "--name",
                "nokube-agent-container",
                "--restart",
                "unless-stopped",
                "--pid",
                "host",         // 使用宿主机PID命名空间，观测宿主机所有进程信息
                "--privileged", // 特权模式，获得宿主机完全访问权限
                // 挂载整个workspace到相同路径，确保agent对 {workspace}/configmaps 等目录的写入落到宿主机
                "-v",
                &format!("{}:{}", workspace, workspace),
                "-v",
                &format!("{}:{}", home_dir, home_dir),
                "-v",
                &format!("{}:{}", remote_lib_path, remote_lib_path),
                "-v",
                &format!("{}:/etc/.nokube/config.yaml", host_config_path), // 挂载配置文件到容器标准路径
                "-v",
                "/var/run/docker.sock:/var/run/docker.sock", // Docker-in-Docker socket mounting
                "-e",
                &format!("LD_LIBRARY_PATH={}", remote_lib_path),
                "-e",
                &format!("HOME={}", home_dir),
                "-e",
                "DOCKER_HOST=unix:///var/run/docker.sock", // Docker socket环境变量
                // 统一开启详细日志，便于问题排查；可由 RUST_LOG 覆盖
                "-e",
                "RUST_LOG=nokube=debug,opentelemetry_otlp=debug,opentelemetry=info",
                "-e",
                "RUST_BACKTRACE=1",
                "--network",
                "host",
                "--workdir",
                &home_dir,
                "nokube:latest", // 使用预构建的nokube镜像
                "nokube",
                "agent-service",
                "--extra-params",
                &extra_params,
            ])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to execute docker run command: {}", e))?;

        if !start_result.status.success() {
            let error_msg = String::from_utf8_lossy(&start_result.stderr);
            anyhow::bail!("Failed to start Docker container: {}", error_msg);
        }

        info!("Docker container service configured and started");
        Ok(())
    }
}
