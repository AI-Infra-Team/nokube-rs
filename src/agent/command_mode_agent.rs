use crate::agent::general::RemoteExecutor;
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
    remote_executor: RemoteExecutor,
    config: ClusterConfig,
}

impl CommandModeAgent {
    pub fn new(config: ClusterConfig) -> Self {
        let remote_executor = if let Some(node) = config.nodes.first() {
            if let Some(proxy) = &node.proxy {
                RemoteExecutor::with_proxy(proxy.clone())
            } else {
                RemoteExecutor::new()
            }
        } else {
            RemoteExecutor::new()
        };

        Self {
            dependency_installer: DependencyInstaller::new(),
            remote_executor,
            config,
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

        // 配置systemctl服务
        self.setup_systemctl_service().await?;

        info!("Command mode agent execution completed");
        Ok(())
    }

    async fn configure_environment(&self) -> Result<()> {
        info!("Configuring environment");

        let create_dirs_script = r#"
            mkdir -p /opt/nokube-agent
            mkdir -p /var/log/nokube
            mkdir -p /etc/nokube
        "#;

        let result = self
            .remote_executor
            .execute_script(create_dirs_script)
            .await?;
        if !result.success {
            anyhow::bail!("Failed to create directories: {}", result.stderr);
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

    async fn setup_systemctl_service(&self) -> Result<()> {
        info!("Setting up systemctl service");

        let service_content = format!(
            r#"[Unit]
Description=Nokube Agent Service
After=network.target

[Service]
Type=simple
ExecStart=/opt/nokube-agent/nokube agent-service --config-path /etc/nokube/config.json
Restart=always
RestartSec=5
User=root
WorkingDirectory=/opt/nokube-agent

[Install]
WantedBy=multi-user.target
"#
        );

        std::fs::write("/etc/systemd/system/nokube-agent.service", service_content)?;

        let result = self
            .remote_executor
            .execute_command("systemctl daemon-reload")
            .await?;
        if !result.success {
            anyhow::bail!("Failed to reload systemd: {}", result.stderr);
        }

        let result = self
            .remote_executor
            .execute_command("systemctl enable nokube-agent")
            .await?;
        if !result.success {
            anyhow::bail!("Failed to enable service: {}", result.stderr);
        }

        info!("Systemctl service configured");
        Ok(())
    }
}
