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

        let output = Command::new("pip").arg("install").arg(package).output()?;

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

        let output = Command::new("docker").arg("pull").arg(image).output()?;

        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to pull Docker image {}: {}", image, error_msg);
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

    pub async fn execute(&self) -> Result<()> {
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
            error!("Cluster not found: {}. Current clusters: {:?}", cluster_name, cluster_names);
            return Err(anyhow::anyhow!(
                format!("Cluster not found: {}. Current clusters: {:?}", cluster_name, cluster_names)
            ));
        }

        // 执行环境配置任务
        self.configure_environment().await?;

        // 安装必要依赖
        self.install_dependencies().await?;

        // 配置Docker容器服务
        self.setup_docker_service().await?;

        // 如果需要，设置Grafana
        self.setup_grafana_if_needed().await?;

        info!("Command mode agent execution completed");
        Ok(())
    }

    async fn configure_environment(&self) -> Result<()> {
        info!("Configuring environment");

        // 直接使用本地命令创建目录
        let mkdir_result = std::process::Command::new("mkdir")
            .args(&["-p", "/opt/nokube-agent", "/var/log/nokube", "/etc/nokube"])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to execute mkdir command: {}", e))?;

        if !mkdir_result.status.success() {
            let error_msg = String::from_utf8_lossy(&mkdir_result.stderr);
            anyhow::bail!("Failed to create directories: {}", error_msg);
        }

        info!("Environment configuration completed");
        Ok(())
    }

    async fn install_dependencies(&self) -> Result<()> {
        info!("Installing dependencies");

        let dependencies = vec![
            DependencyType::Apt("htop".to_string()),
            DependencyType::Apt("iotop".to_string()),
            DependencyType::Apt("net-tools".to_string()),
            DependencyType::Pip("psutil".to_string()),
        ];

        self.dependency_installer
            .install_dependencies(dependencies)
            .await?;

        info!("Dependencies installation completed");
        Ok(())
    }

    async fn setup_docker_service(&self) -> Result<()> {
        info!("Setting up Docker container service");

        // 获取当前用户信息（从环境变量或配置中）
        let current_user = std::env::var("USER").unwrap_or_else(|_| "pa".to_string());
        let home_dir = std::env::var("HOME").unwrap_or_else(|_| format!("/home/{}", current_user));
        
        // 使用用户级别的配置路径
        let config_path = format!("{}/.nokube/config.yaml", home_dir);

        // 创建包含cluster_name的extra_params
        let cluster_params = serde_json::json!({
            "cluster_name": self.config.cluster_name
        });
        let extra_params = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            cluster_params.to_string().as_bytes()
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
        let start_result = std::process::Command::new("sudo")
            .args(&[
                "docker", "run", "-d",
                "--name", "nokube-agent-container",
                "--restart", "unless-stopped",
                "-v", &format!("{}:{}", home_dir, home_dir),
                "-v", "/opt/nokube-remote-lib:/opt/nokube-remote-lib",
                "-e", "LD_LIBRARY_PATH=/opt/nokube-remote-lib",
                "-e", &format!("HOME={}", home_dir),
                "--network", "host",
                "--workdir", &home_dir,
                "ubuntu:22.04",
                "/opt/nokube-remote-lib/nokube",
                "agent-service",
                "--config-path", &config_path,
                "--extra-params", &extra_params
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

    async fn setup_grafana_if_needed(&self) -> Result<()> {
        // 检查是否需要设置Grafana
        if let Some(params) = &self.extra_params {
            if let Some(setup_grafana) = params.get("setup_grafana").and_then(|v| v.as_bool()) {
                if setup_grafana {
                    info!("Setting up Grafana as requested in extra params");
                    
                    // 获取Grafana配置和端口
                    let grafana_config = params.get("grafana_config")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let grafana_port = params.get("grafana_port")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(3000) as u16;
                    
                    // 直接在本地创建Grafana配置文件
                    std::fs::write("/tmp/grafana.ini", grafana_config)
                        .map_err(|e| anyhow::anyhow!("Failed to create Grafana config file: {}", e))?;
                    info!("Grafana config file created at /tmp/grafana.ini");
                    
                    // 停止可能存在的旧容器
                    let stop_result = std::process::Command::new("sudo")
                        .args(&["docker", "stop", "nokube-grafana"])
                        .output();
                    if let Ok(output) = stop_result {
                        if output.status.success() {
                            info!("Stopped existing Grafana container");
                        }
                    }
                    
                    let rm_result = std::process::Command::new("sudo")
                        .args(&["docker", "rm", "nokube-grafana"])
                        .output();
                    if let Ok(output) = rm_result {
                        if output.status.success() {
                            info!("Removed existing Grafana container");
                        }
                    }
                    
                    // 启动Grafana容器
                    let start_result = std::process::Command::new("sudo")
                        .args(&[
                            "docker", "run", "-d",
                            "--name", "nokube-grafana",
                            "-p", &format!("{}:3000", grafana_port),
                            "-v", "/tmp/grafana.ini:/etc/grafana/grafana.ini",
                            "--restart", "unless-stopped",
                            "grafana/grafana:latest"
                        ])
                        .output()
                        .map_err(|e| anyhow::anyhow!("Failed to execute docker run command: {}", e))?;
                    
                    if !start_result.status.success() {
                        let error_msg = String::from_utf8_lossy(&start_result.stderr);
                        anyhow::bail!("Failed to start Grafana container: {}", error_msg);
                    }
                    
                    let container_output = String::from_utf8_lossy(&start_result.stdout);
                    let container_id = container_output.trim();
                    info!("Grafana container started with ID: {}", container_id);
                    
                    // 验证容器是否运行
                    let verify_result = std::process::Command::new("sudo")
                        .args(&["docker", "ps", "--filter", "name=nokube-grafana", "--filter", "status=running", "--quiet"])
                        .output()
                        .map_err(|e| anyhow::anyhow!("Failed to verify container status: {}", e))?;
                    
                    if verify_result.status.success() {
                        let output = String::from_utf8_lossy(&verify_result.stdout);
                        let running_id = output.trim();
                        if !running_id.is_empty() {
                            info!("Verified Grafana container is running (ID: {})", running_id);
                        } else {
                            // 获取容器日志帮助调试
                            if let Ok(logs_result) = std::process::Command::new("sudo")
                                .args(&["docker", "logs", "nokube-grafana"])
                                .output() {
                                let logs = String::from_utf8_lossy(&logs_result.stdout);
                                info!("Grafana container logs: {}", logs);
                            }
                            anyhow::bail!("Grafana container failed to start or is not running");
                        }
                    } else {
                        anyhow::bail!("Failed to verify Grafana container status");
                    }
                }
            }
        }
        
        Ok(())
    }
}
