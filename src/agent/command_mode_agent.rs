use crate::agent::master_agent::GrafanaManager;
use crate::config::cluster_config::ClusterConfig;
use anyhow::Result;
use std::io::Write;
use std::process::Command;
use tracing::{debug, error, info};

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

    /// 构建不走代理的 HTTP 客户端，避免访问本地容器时被环境代理截走
    fn create_local_http_client() -> Result<reqwest::Client> {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build local HTTP client: {}", e))
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

        // 如果需要，设置Grafana
        println!("[REALTIME] 📋 Setting up Grafana if needed...");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        self.setup_grafana_if_needed().await?;

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
            "cluster_name": self.config.cluster_name
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

    async fn setup_grafana_if_needed(&self) -> Result<()> {
        // 检查是否需要设置Grafana
        if let Some(params) = &self.extra_params {
            if let Some(setup_grafana) = params.get("setup_grafana").and_then(|v| v.as_bool()) {
                if setup_grafana {
                    println!("[REALTIME] 🚀 Setting up Grafana as requested in extra params");
                    std::io::Write::flush(&mut std::io::stdout()).ok();
                    info!("Setting up Grafana as requested in extra params");

                    // 从方法获取workspace路径
                    let workspace = self.get_workspace()?;

                    // 获取Grafana配置和端口
                    let grafana_config = params
                        .get("grafana_config")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            anyhow::anyhow!("Missing required grafana_config in extra_params")
                        })?;
                    let grafana_port = params
                        .get("grafana_port")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| {
                            anyhow::anyhow!("Missing required grafana_port in extra_params")
                        })? as u16;

                    // 创建workspace内的config目录
                    let config_dir = format!("{}/config", workspace);
                    std::fs::create_dir_all(&config_dir).map_err(|e| {
                        anyhow::anyhow!("Failed to create config directory {}: {}", config_dir, e)
                    })?;

                    // 在workspace内创建Grafana配置文件
                    let grafana_config_path = format!("{}/grafana.ini", config_dir);
                    std::fs::write(&grafana_config_path, grafana_config).map_err(|e| {
                        anyhow::anyhow!("Failed to create Grafana config file: {}", e)
                    })?;
                    println!(
                        "[REALTIME] 📝 Grafana config file created at {}",
                        grafana_config_path
                    );
                    info!("Grafana config file created at {}", grafana_config_path);

                    // 创建 Grafana provisioning 目录（数据源 + 仪表板）
                    let provisioning_dir = format!("{}/provisioning/datasources", config_dir);
                    let dashboards_prov_dir = format!("{}/provisioning/dashboards", config_dir);
                    let dashboards_nokube_dir =
                        format!("{}/provisioning/dashboards/nokube", config_dir);
                    std::fs::create_dir_all(&provisioning_dir).map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to create Grafana provisioning dir {}: {}",
                            provisioning_dir,
                            e
                        )
                    })?;
                    std::fs::create_dir_all(&dashboards_nokube_dir).map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to create Grafana dashboards provisioning dir {}: {}",
                            dashboards_nokube_dir,
                            e
                        )
                    })?;
                    let ds_yaml_path = format!("{}/nokube-datasource.yaml", provisioning_dir);
                    let head_ip = self
                        .config
                        .nodes
                        .iter()
                        .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head))
                        .and_then(|n| n.get_ip().ok())
                        .unwrap_or("127.0.0.1");
                    let greptime_port = self.config.task_spec.monitoring.greptimedb.port;
                    let mysql_port = greptime_port + 2;
                    let mysql_user = params
                        .get("greptimedb_mysql_user")
                        .and_then(|v| v.as_str())
                        .unwrap_or("root");
                    let mysql_pass_opt = params
                        .get("greptimedb_mysql_password")
                        .and_then(|v| v.as_str());
                    let secure_block = match mysql_pass_opt {
                        Some(p) if !p.is_empty() => {
                            format!("\n    secureJsonData:\n      password: {}\n", p)
                        }
                        _ => String::new(),
                    };
                    let ds_yaml = format!(
                        r#"apiVersion: 1
datasources:
  - name: GreptimeDB
    type: prometheus
    access: proxy
    url: http://{head}:{port}/v1/prometheus
    isDefault: true
    editable: true
  - name: greptimeplugin
    type: info8fcc-greptimedb-datasource
    access: proxy
    url: http://{head}:{port}
    isDefault: false
    editable: true
    jsonData:
      server: http://{head}:{port}
      defaultDatabase: public
  - name: greptimemysql
    type: mysql
    access: proxy
    url: {head}:{mysql_port}
    database: public
    user: {mysql_user}
    isDefault: false
    editable: true
    jsonData:
      timeInterval: 1s{secure}
"#,
                        head = head_ip,
                        port = greptime_port,
                        mysql_port = mysql_port,
                        mysql_user = mysql_user,
                        secure = secure_block
                    );
                    std::fs::write(&ds_yaml_path, ds_yaml).map_err(|e| {
                        anyhow::anyhow!("Failed to write Grafana datasource YAML: {}", e)
                    })?;
                    info!(
                        "Grafana datasource provisioning written at {}",
                        ds_yaml_path
                    );

                    // 写入 Dashboards provider 与 MySQL 日志仪表盘 JSON
                    let provider_yaml_path =
                        format!("{}/nokube-provider.yaml", dashboards_prov_dir);
                    let provider_yaml = r#"apiVersion: 1
providers:
  - name: 'nokube'
    orgId: 1
    type: file
    disableDeletion: false
    updateIntervalSeconds: 10
    allowUiUpdates: true
    options:
      path: /etc/grafana/provisioning/dashboards/nokube
      foldersFromFilesStructure: true
"#;
                    std::fs::write(&provider_yaml_path, provider_yaml).map_err(|e| {
                        anyhow::anyhow!("Failed to write Grafana dashboards provider YAML: {}", e)
                    })?;

                    let logs_dash_json_path =
                        format!("{}/nokube-logs-mysql.json", dashboards_nokube_dir);
                    let logs_dash_json = serde_json::json!({
                        "id": null,
                        "uid": "nokube-logs-mysql",
                        "title": "NoKube Logs (MySQL)",
                        "tags": ["nokube", "logs", "mysql", "greptimedb"],
                        "timezone": "browser",
                        "panels": [
                            {"id": 1, "title": "Log Messages (Latest)", "type": "logs", "datasource": "greptimemysql",
                     "targets": [{"format":"table","rawSql":"SELECT timestamp AS time, body AS message, severity_text AS level FROM opentelemetry_logs WHERE $__timeFilter(timestamp) AND (${container_path:sqlstring} = '' OR scope_name = ${container_path:sqlstring}) ORDER BY timestamp DESC LIMIT 1000"}],
                             "options": {"showTime": true, "showLabels": false, "showCommonLabels": false, "wrapLogMessage": false, "enableLogDetails": false, "messageField": "message"},
                             "gridPos": {"h": 12, "w": 24, "x": 0, "y": 0}},
                            {"id": 2, "title": "Log Level Distribution", "type": "piechart", "datasource": "greptimemysql",
                             "targets": [{"format":"table","rawSql":"SELECT severity_text AS metric, COUNT(*) AS value FROM opentelemetry_logs WHERE $__timeFilter(timestamp) GROUP BY severity_text"}],
                             "gridPos": {"h": 6, "w": 8, "x": 0, "y": 12}},
                            {"id": 3, "title": "Logs per Minute", "type": "timeseries", "datasource": "greptimemysql",
                             "targets": [{"format":"time_series","rawSql":"SELECT $__timeGroup(timestamp, '1m') AS time, 'All Logs' AS metric, COUNT(*) AS value FROM opentelemetry_logs WHERE $__timeFilter(timestamp) GROUP BY 1 ORDER BY 1"}],
                             "gridPos": {"h": 6, "w": 16, "x": 8, "y": 12}}
                        ],
                        "time": {"from": "now-6h", "to": "now"},
                        "refresh": "30s",
                        "schemaVersion": 30,
                        "version": 1
                    });
                    std::fs::write(
                        &logs_dash_json_path,
                        serde_json::to_string_pretty(&logs_dash_json).unwrap_or_default(),
                    )
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to write MySQL logs dashboard JSON: {}", e)
                    })?;

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
                            "docker",
                            "run",
                            "-d",
                            "--name",
                            "nokube-grafana",
                            "-p",
                            &format!("{}:3000", grafana_port),
                            "-v",
                            &format!("{}:/etc/grafana/grafana.ini", grafana_config_path),
                            "-v",
                            &format!("{}:/etc/grafana/provisioning/datasources", provisioning_dir),
                            "-v",
                            &format!(
                                "{}:/etc/grafana/provisioning/dashboards",
                                dashboards_prov_dir
                            ),
                            "--restart",
                            "unless-stopped",
                            "greptime/grafana-greptimedb:latest",
                        ])
                        .output()
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to execute docker run command: {}", e)
                        })?;

                    if !start_result.status.success() {
                        let error_msg = String::from_utf8_lossy(&start_result.stderr);
                        anyhow::bail!("Failed to start Grafana container: {}", error_msg);
                    }

                    let container_output = String::from_utf8_lossy(&start_result.stdout);
                    let container_id = container_output.trim();
                    println!(
                        "[REALTIME] 🐳 Grafana container started with ID: {}",
                        container_id
                    );
                    info!("Grafana container started with ID: {}", container_id);

                    // 验证容器是否运行
                    let verify_result = std::process::Command::new("sudo")
                        .args(&[
                            "docker",
                            "ps",
                            "--filter",
                            "name=nokube-grafana",
                            "--filter",
                            "status=running",
                            "--quiet",
                        ])
                        .output()
                        .map_err(|e| anyhow::anyhow!("Failed to verify container status: {}", e))?;

                    if verify_result.status.success() {
                        let output = String::from_utf8_lossy(&verify_result.stdout);
                        let running_id = output.trim();
                        if !running_id.is_empty() {
                            info!("Verified Grafana container is running (ID: {})", running_id);

                            // 检查Grafana端口是否真正可用
                            let workspace = self.get_workspace()?;
                            let node_ip = self
                                .extra_params
                                .as_ref()
                                .and_then(|params| params.get("node_ip"))
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| {
                                    anyhow::anyhow!("Missing required node_ip in extra_params")
                                })?;

                            println!(
                                "[REALTIME] ⏳ Checking if Grafana is responding on {}:{}",
                                node_ip, grafana_port
                            );
                            info!(
                                "Checking if Grafana is responding on {}:{}",
                                node_ip, grafana_port
                            );
                            let check_hosts = vec![node_ip.to_string(), "127.0.0.1".to_string()];
                            let mut retries = 0;
                            let max_retries = 180; // allow up to 3 minutes for heavy migrations/plugins
                            let mut grafana_ready = false;
                            let client = Self::create_local_http_client()?;

                            while retries < max_retries && !grafana_ready {
                                for host in &check_hosts {
                                    let url =
                                        format!("http://{}:{}/api/health", host, grafana_port);
                                    match client
                                        .get(&url)
                                        .timeout(std::time::Duration::from_secs(2))
                                        .send()
                                        .await
                                    {
                                        Ok(response) => {
                                            if response.status().is_success() {
                                                println!(
                                                    "[REALTIME] ✅ Grafana is responding on {}:{}",
                                                    host, grafana_port
                                                );
                                                info!(
                                                    "Grafana is responding on {}:{}",
                                                    host, grafana_port
                                                );
                                                grafana_ready = true;
                                                break;
                                            } else {
                                                debug!(
                                                    "Grafana health endpoint {} returned status {}",
                                                    url,
                                                    response.status()
                                                );
                                            }
                                        }
                                        Err(err) => {
                                            debug!(
                                                "Grafana health check to {} failed: {}",
                                                url, err
                                            );
                                        }
                                    }
                                }
                                if grafana_ready {
                                    break;
                                }
                                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                                retries += 1;
                            }

                            if !grafana_ready {
                                // 输出部分容器日志帮助定位
                                if let Ok(logs) = std::process::Command::new("sudo")
                                    .args(&["docker", "logs", "--since", "2m", "nokube-grafana"])
                                    .output()
                                {
                                    let out = String::from_utf8_lossy(&logs.stdout);
                                    println!(
                                        "[REALTIME] 🔍 Grafana recent logs (last 2m):\n{}",
                                        out
                                    );
                                    info!("Grafana recent logs (last 2m): {}", out);
                                }
                                anyhow::bail!("Grafana container is running but not responding on {}:{} after {} seconds", node_ip, grafana_port, max_retries);
                            }

                            // 启动 GreptimeDB（如果需要）
                            println!("[REALTIME] 📊 Setting up GreptimeDB...");
                            std::io::Write::flush(&mut std::io::stdout()).ok();
                            let selected_port = match self
                                .setup_greptimedb_if_needed(&workspace)
                                .await
                            {
                                Ok(selected_port) => {
                                    println!(
                                        "[REALTIME] ✅ GreptimeDB setup completed on port {}",
                                        selected_port
                                    );
                                    std::io::Write::flush(&mut std::io::stdout()).ok();
                                    info!("GreptimeDB setup completed on port {}", selected_port);
                                    selected_port
                                }
                                Err(e) => {
                                    println!("[REALTIME] ❌ Failed to setup GreptimeDB: {}", e);
                                    error!("Failed to setup GreptimeDB: {}", e);
                                    anyhow::bail!("GreptimeDB setup failed: {}", e);
                                }
                            };

                            // 配置数据源和仪表板
                            println!("[REALTIME] Starting Grafana datasource and dashboard configuration...");
                            std::io::Write::flush(&mut std::io::stdout()).ok();
                            match self
                                .setup_grafana_datasource_and_dashboard(
                                    grafana_port,
                                    &workspace,
                                    selected_port,
                                )
                                .await
                            {
                                Ok(_) => {
                                    println!("[REALTIME] ✅ Grafana datasource and dashboard configured successfully");
                                    std::io::Write::flush(&mut std::io::stdout()).ok();
                                    info!(
                                        "Grafana datasource and dashboard configured successfully"
                                    );
                                }
                                Err(e) => {
                                    println!("[REALTIME] ❌ Failed to configure Grafana datasource/dashboard: {}", e);
                                    std::io::Write::flush(&mut std::io::stdout()).ok();
                                    error!(
                                        "Failed to configure Grafana datasource/dashboard: {}",
                                        e
                                    );
                                    anyhow::bail!("Grafana configuration failed: {}", e);
                                }
                            }
                        } else {
                            // 获取容器日志帮助调试
                            if let Ok(logs_result) = std::process::Command::new("sudo")
                                .args(&["docker", "logs", "nokube-grafana"])
                                .output()
                            {
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

    async fn setup_greptimedb_if_needed(&self, workspace: &str) -> Result<u16> {
        info!("Setting up GreptimeDB if needed");

        let greptimedb_port = if let Some(params) = &self.extra_params {
            params
                .get("greptimedb_port")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| {
                    anyhow::anyhow!("Missing required greptimedb_port in extra_params")
                })? as u16
        } else {
            anyhow::bail!("Missing extra_params - cannot determine GreptimeDB port");
        };

        // 检查 GreptimeDB 容器是否已经运行
        let check_result = std::process::Command::new("sudo")
            .args(&[
                "docker",
                "ps",
                "--filter",
                "name=nokube-greptimedb",
                "--filter",
                "status=running",
                "--quiet",
            ])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to check GreptimeDB container: {}", e))?;

        if check_result.status.success() {
            let output = String::from_utf8_lossy(&check_result.stdout);
            if !output.trim().is_empty() {
                info!("GreptimeDB container already running; restarting to apply config");
                // Fall through to stop/remove and recreate with desired flags
            }
        }

        // Determine a free 4-port block [base..base+3] starting from configured port
        let is_port_free = |p: u16| -> bool {
            match std::net::TcpListener::bind(("0.0.0.0", p)) {
                Ok(listener) => {
                    drop(listener);
                    true
                }
                Err(_) => false,
            }
        };
        let mut selected_port = greptimedb_port;
        let mut found = false;
        for base in greptimedb_port..greptimedb_port.saturating_add(50) {
            let mut ok = true;
            for off in 0..4u16 {
                if !is_port_free(base.saturating_add(off)) {
                    ok = false;
                    break;
                }
            }
            if ok {
                selected_port = base;
                found = true;
                break;
            }
        }
        if !found {
            anyhow::bail!(
                "No free 4-port block available starting from {}",
                greptimedb_port
            );
        }
        if selected_port != greptimedb_port {
            println!(
                "[REALTIME] ⚠️ GreptimeDB port {} busy, switching to {}-{}",
                greptimedb_port,
                selected_port,
                selected_port + 3
            );
            info!(
                "GreptimeDB port {} busy, switching to {}-{}",
                greptimedb_port,
                selected_port,
                selected_port + 3
            );
        }

        info!("Starting GreptimeDB container on port {}", selected_port);

        // 停止并删除可能存在的旧容器
        let _ = std::process::Command::new("sudo")
            .args(&["docker", "stop", "nokube-greptimedb"])
            .output();
        let _ = std::process::Command::new("sudo")
            .args(&["docker", "rm", "nokube-greptimedb"])
            .output();

        // 在workspace中创建数据目录
        let data_dir = format!("{}/greptimedb-data", workspace);
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create GreptimeDB data directory: {}", e))?;

        // 先清理可能存在的旧容器，避免名称冲突
        let _ = std::process::Command::new("sudo")
            .args(&["docker", "rm", "-f", "nokube-greptimedb"])
            .output();

        // 启动GreptimeDB容器，若端口已占用则顺延重试
        let mut start_ok = false;
        let mut base = selected_port;
        for _ in 0..50 {
            info!("Attempting to start GreptimeDB on base port {}", base);
            let start_result = std::process::Command::new("sudo")
                .args(&[
                    "docker",
                    "run",
                    "-d",
                    "--name",
                    "nokube-greptimedb",
                    "-p",
                    &format!("{}:4000", base),
                    "-p",
                    &format!("{}:4001", base + 1),
                    "-p",
                    &format!("{}:4002", base + 2),
                    "-p",
                    &format!("{}:4003", base + 3),
                    "-v",
                    &format!("{}:/tmp/greptimedb", data_dir),
                    "--restart",
                    "unless-stopped",
                    "greptime/greptimedb:v0.15.1",
                    "standalone",
                    "start",
                    "--http-addr",
                    "0.0.0.0:4000",
                    "--rpc-addr",
                    "0.0.0.0:4001",
                    "--mysql-addr",
                    "0.0.0.0:4002",
                    "--postgres-addr",
                    "0.0.0.0:4003",
                ])
                .output()
                .map_err(|e| anyhow::anyhow!("Failed to start GreptimeDB container: {}", e))?;

            if start_result.status.success() {
                selected_port = base;
                start_ok = true;
                break;
            } else {
                let error_msg = String::from_utf8_lossy(&start_result.stderr);
                let em = error_msg.to_lowercase();
                if em.contains("already in use by container")
                    || em.contains("container name \"/nokube-greptimedb\" is already in use")
                {
                    // 清理重名容器后重试同一端口
                    let _ = std::process::Command::new("sudo")
                        .args(&["docker", "rm", "-f", "nokube-greptimedb"])
                        .output();
                    continue;
                } else if em.contains("port is already allocated")
                    || em.contains("bind for 0.0.0.0")
                {
                    base = base.saturating_add(1);
                    continue;
                } else {
                    anyhow::bail!("Failed to start GreptimeDB container: {}", error_msg);
                }
            }
        }
        if !start_ok {
            anyhow::bail!(
                "Failed to start GreptimeDB after trying multiple ports starting from {}",
                greptimedb_port
            );
        }

        // Fetch container ID after successful start
        let inspect = std::process::Command::new("sudo")
            .args(&["docker", "ps", "-aq", "--filter", "name=nokube-greptimedb"])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to query GreptimeDB container ID: {}", e))?;
        let container_id = String::from_utf8_lossy(&inspect.stdout).trim().to_string();
        info!("GreptimeDB container started with ID: {}", container_id);

        // 等待GreptimeDB启动
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

        // 验证GreptimeDB是否正常运行
        let verify_result = std::process::Command::new("sudo")
            .args(&[
                "docker",
                "ps",
                "--filter",
                "name=nokube-greptimedb",
                "--filter",
                "status=running",
                "--quiet",
            ])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to verify GreptimeDB status: {}", e))?;

        if verify_result.status.success() {
            let output = String::from_utf8_lossy(&verify_result.stdout);
            if !output.trim().is_empty() {
                info!("GreptimeDB is running and ready");
            } else {
                // 获取容器日志帮助调试
                if let Ok(logs_result) = std::process::Command::new("sudo")
                    .args(&["docker", "logs", "nokube-greptimedb"])
                    .output()
                {
                    let logs = String::from_utf8_lossy(&logs_result.stdout);
                    info!("GreptimeDB container logs: {}", logs);
                }
                anyhow::bail!("GreptimeDB container failed to start");
            }
        }

        Ok(selected_port)
    }

    async fn setup_grafana_datasource_and_dashboard(
        &self,
        grafana_port: u16,
        _workspace: &str,
        greptimedb_port: u16,
    ) -> Result<()> {
        println!("[REALTIME] 🎛️ Setting up Grafana datasource and dashboard");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        info!("Setting up Grafana datasource and dashboard");

        // 等待 Grafana 完全启动
        println!("[REALTIME] ⏳ Waiting 15 seconds for Grafana to fully start...");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;

        // 从extra_params获取节点IP，GreptimeDB端口由调用方传入（可能因冲突被调整）
        let node_ip = self
            .extra_params
            .as_ref()
            .and_then(|params| params.get("node_ip"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required node_ip in extra_params"))?;

        // 使用 Head 节点IP 作为 GreptimeDB 访问地址（而不是当前节点IP）
        let head_ip = if let Some(head_node) = self
            .config
            .nodes
            .iter()
            .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head))
        {
            head_node.get_ip().unwrap_or(node_ip)
        } else {
            node_ip
        };
        let greptimedb_endpoint = format!("http://{}:{}", head_ip, greptimedb_port);
        let prometheus_api_url = format!("{}/v1/prometheus", greptimedb_endpoint);

        println!(
            "[REALTIME] 🔌 Configuring datasource with endpoint: {}",
            prometheus_api_url
        );
        std::io::Write::flush(&mut std::io::stdout()).ok();

        // 配置 Prometheus 数据源（Greptime PromQL API）
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

        let client = Self::create_local_http_client()?;
        let grafana_url = format!("http://{}:{}/api/datasources", node_ip, grafana_port);

        // 尝试配置数据源
        let grafana_user = self
            .config
            .task_spec
            .monitoring
            .grafana
            .admin_user
            .clone()
            .unwrap_or_else(|| "admin".to_string());
        let grafana_pass = self
            .config
            .task_spec
            .monitoring
            .grafana
            .admin_password
            .clone()
            .unwrap_or_else(|| "admin".to_string());

        let response = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth(&grafana_user, Some(&grafana_pass))
            .json(&datasource_config)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            // Ignore 409 already exists
            if status.as_u16() != 409 && !error_text.to_lowercase().contains("already exists") {
                anyhow::bail!(
                    "Failed to configure datasource: {} - {}",
                    status,
                    error_text
                );
            } else {
                println!("[REALTIME] ⚠️ Datasource already exists; continuing");
                std::io::Write::flush(&mut std::io::stdout()).ok();
                info!("Datasource already exists; continuing");
            }
        } else {
            println!("[REALTIME] ✅ Successfully configured GreptimeDB datasource");
            std::io::Write::flush(&mut std::io::stdout()).ok();
            info!("Successfully configured GreptimeDB datasource");
        }

        // 移除 GreptimeSQL（Postgres）数据源配置，统一用 HTTP 插件 greptimeplugin 查询 SQL

        // 配置 GreptimeDB 插件数据源（info8fcc-greptimedb-datasource）
        let plugin_ds_config = serde_json::json!({
            "name": "greptimeplugin",
            "type": "info8fcc-greptimedb-datasource",
            "url": greptimedb_endpoint,
            "access": "proxy",
            "isDefault": false,
            "basicAuth": false,
            "jsonData": {
                "server": greptimedb_endpoint,
                "defaultDatabase": "public"
            }
        });
        let response3 = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth(&grafana_user, Some(&grafana_pass))
            .json(&plugin_ds_config)
            .send()
            .await?;
        if !response3.status().is_success() {
            let status = response3.status();
            let errt = response3.text().await.unwrap_or_default();
            if status.as_u16() == 409 || errt.to_lowercase().contains("already exists") {
                // Update existing datasource to ensure URL is set
                let get_url = format!(
                    "http://{}:{}/api/datasources/name/{}",
                    node_ip, grafana_port, "greptimeplugin"
                );
                if let Ok(get_resp) = client
                    .get(&get_url)
                    .basic_auth(&grafana_user, Some(&grafana_pass))
                    .send()
                    .await
                {
                    if get_resp.status().is_success() {
                        if let Ok(val) = get_resp.json::<serde_json::Value>().await {
                            if let Some(id) = val.get("id").and_then(|v| v.as_i64()) {
                                let put_url = format!(
                                    "http://{}:{}/api/datasources/{}",
                                    node_ip, grafana_port, id
                                );
                                let _ = client
                                    .put(&put_url)
                                    .header("Content-Type", "application/json")
                                    .basic_auth(&grafana_user, Some(&grafana_pass))
                                    .json(&plugin_ds_config)
                                    .send()
                                    .await?;
                            }
                        }
                    }
                }
            } else {
                anyhow::bail!(
                    "Failed to configure greptimeplugin datasource: {} - {}",
                    status,
                    errt
                );
            }
        }

        // 导入仪表板
        println!("[REALTIME] 📊 Importing NoKube dashboards (Cluster + Actor)...");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        // 导入第一个仪表板：Cluster Monitoring
        println!("[REALTIME] 📋 Importing NoKube Cluster Monitoring dashboard...");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        self.import_cluster_monitoring_dashboard(grafana_port, node_ip, greptimedb_port)
            .await?;

        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // 导入第二个仪表板：Actor Dashboard（合并了 service 和 actor）
        println!(
            "[REALTIME] 📋 Importing NoKube Actor Dashboard (K8s actors, services, containers)..."
        );
        std::io::Write::flush(&mut std::io::stdout()).ok();
        self.import_actor_dashboard(grafana_port, node_ip).await?;

        // 额外：创建 MySQL 数据源（用于日志查询）
        println!("[REALTIME] 🔌 Ensuring MySQL datasource for GreptimeDB logs...");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        self.ensure_mysql_logs_datasource(grafana_port, node_ip, greptimedb_port)
            .await?;

        // 导入日志仪表板（使用 MySQL 数据源）
        println!("[REALTIME] 📋 Importing NoKube Logs (MySQL) dashboard...");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        self.import_logs_dashboard_mysql(grafana_port, node_ip)
            .await?;

        // 设置集群仪表盘为首页
        if let Err(e) = self
            .set_home_dashboard(grafana_port, node_ip, "nokube-cluster-monitoring")
            .await
        {
            println!("[REALTIME] ⚠️ Failed to set home dashboard: {}", e);
            info!("Failed to set home dashboard: {}", e);
        } else {
            println!("[REALTIME] 🏠 Set cluster dashboard as home");
        }

        println!("[REALTIME] 🎉 All Grafana setup completed successfully!");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        Ok(())
    }

    async fn set_home_dashboard(&self, grafana_port: u16, node_ip: &str, uid: &str) -> Result<()> {
        let client = Self::create_local_http_client()?;
        let prefs_url = format!("http://{}:{}/api/org/preferences", node_ip, grafana_port);
        let grafana_user = self
            .config
            .task_spec
            .monitoring
            .grafana
            .admin_user
            .clone()
            .unwrap_or_else(|| "admin".to_string());
        let grafana_pass = self
            .config
            .task_spec
            .monitoring
            .grafana
            .admin_password
            .clone()
            .unwrap_or_else(|| "admin".to_string());
        let body = serde_json::json!({
            "homeDashboardUID": uid
        });
        let resp = client
            .put(&prefs_url)
            .header("Content-Type", "application/json")
            .basic_auth(&grafana_user, Some(&grafana_pass))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Failed to set home dashboard: {} - {}", status, text);
        }

        // Star the dashboard for visibility
        let get_url = format!(
            "http://{}:{}/api/dashboards/uid/{}",
            node_ip, grafana_port, uid
        );
        let dash = client
            .get(&get_url)
            .basic_auth(&grafana_user, Some(&grafana_pass))
            .send()
            .await?;
        if dash.status().is_success() {
            if let Ok(val) = dash.json::<serde_json::Value>().await {
                if let Some(id) = val
                    .get("dashboard")
                    .and_then(|d| d.get("id"))
                    .and_then(|v| v.as_i64())
                {
                    let star_url = format!(
                        "http://{}:{}/api/user/stars/dashboard/{}",
                        node_ip, grafana_port, id
                    );
                    let _ = client
                        .post(&star_url)
                        .basic_auth(&grafana_user, Some(&grafana_pass))
                        .send()
                        .await;
                }
            }
        }
        Ok(())
    }

    async fn import_cluster_monitoring_dashboard(
        &self,
        grafana_port: u16,
        node_ip: &str,
        greptimedb_port: u16,
    ) -> Result<()> {
        println!("[REALTIME] 📋 Importing NoKube cluster monitoring dashboard");
        info!("Importing NoKube cluster monitoring dashboard");

        let http_port = self.config.task_spec.monitoring.httpserver.port;
        let greptime_endpoint = format!("http://{}:{}", node_ip, greptimedb_port);

        let links_markdown = format!(
            "### 关键服务\n\n- [Actor Dashboard](/d/nokube-actor-dashboard)\n- [Logs (MySQL)](/d/nokube-logs-mysql)\n- [HTTP 文件服务器](http://{node}:{http_port})\n- [Greptime Metrics]({greptime}/v1/prometheus)\n",
            node = node_ip,
            http_port = http_port,
            greptime = greptime_endpoint,
        );

        // delete existing to avoid stale layout
        {
            let client_pre = Self::create_local_http_client()?;
            let uid = "nokube-cluster-monitoring";
            let get_url = format!(
                "http://{}:{}/api/dashboards/uid/{}",
                node_ip, grafana_port, uid
            );
            if let Ok(resp) = client_pre
                .get(&get_url)
                .basic_auth("admin", Some("admin"))
                .send()
                .await
            {
                if resp.status().is_success() {
                    let del_url = format!(
                        "http://{}:{}/api/dashboards/uid/{}",
                        node_ip, grafana_port, uid
                    );
                    let _ = client_pre
                        .delete(&del_url)
                        .basic_auth("admin", Some("admin"))
                        .send()
                        .await;
                }
            }
        }

        let dashboard_config = serde_json::json!({
            "dashboard": {
                "id": null,
                "uid": "nokube-cluster-monitoring",
                "title": "NoKube Cluster Monitoring",
                "tags": ["nokube", "cluster"],
                "timezone": "browser",
                "links": [
                    {"type": "link", "title": "Actor Dashboard", "url": "/d/nokube-actor-dashboard", "targetBlank": true},
                    {"type": "link", "title": "Logs (MySQL)", "url": "/d/nokube-logs-mysql", "targetBlank": true},
                    {"type": "link", "title": "HTTP 文件服务器", "url": format!("http://{}:{}", node_ip, http_port), "targetBlank": true},
                    {"type": "link", "title": "Greptime Metrics", "url": format!("{}/v1/prometheus", greptime_endpoint), "targetBlank": true}
                ],
                "refresh": "15s",
                "time": {
                    "from": "now-1h",
                    "to": "now"
                },
                "templating": {
                    "list": [
                        {
                            "datasource": "GreptimeDB",
                            "label": "Node",
                            "name": "node",
                            "type": "query",
                            "query": "label_values(nokube_cpu_usage, node)",
                            "multi": true,
                            "refresh": 1,
                            "includeAll": false,
                            "current": {"text": "", "value": []}
                        }
                    ]
                },
                "panels": [
                    {
                        "id": 10,
                        "title": "关键链接",
                        "type": "text",
                        "gridPos": {"h": 8, "w": 8, "x": 0, "y": 0},
                        "options": {"mode": "markdown", "content": links_markdown}
                    },

                    {
                        "id": 14,
                        "title": "Cluster Container Memory (bytes, stacked)",
                        "type": "timeseries",
                        "datasource": "GreptimeDB",
                        "gridPos": {"h": 8, "w": 8, "x": 8, "y": 0},
                        "targets": [
                            {"expr": "sum by (container) (last_over_time(nokube_container_mem_bytes[60s]))", "legendFormat": "{{container}}", "interval": "30s"},
                            {"expr": "sum(last_over_time(nokube_node_mem_other_bytes[60s]))", "legendFormat": "Other Used", "interval": "30s"},
                            {"expr": "sum(last_over_time(nokube_node_mem_free_bytes[60s]))", "legendFormat": "Free", "interval": "30s"}
                        ],
                        "fieldConfig": {
                            "defaults": {"unit": "bytes", "min": 0, "custom": {"stacking": {"mode": "normal", "group": "A"}, "fillOpacity": 40}},
                            "overrides": [
                                {"matcher": {"id": "byName", "options": "Other Used"},
                                 "properties": [
                                     {"id": "color", "value": {"mode": "fixed", "fixedColor": "red"}}
                                 ]},
                                {"matcher": {"id": "byName", "options": "Free"},
                                 "properties": [
                                     {"id": "color", "value": {"mode": "fixed", "fixedColor": "blue"}}
                                 ]}
                            ]
                        }
                    },
                    {
                        "id": 15,
                        "title": "Cluster Container CPU (%) (stacked)",
                        "type": "timeseries",
                        "datasource": "GreptimeDB",
                        "gridPos": {"h": 8, "w": 8, "x": 16, "y": 0},
                        "targets": [
                            {"expr": "sum by (container) (nokube_container_cpu)", "legendFormat": "{{container}}", "interval": "30s"}
                        ],
                        "fieldConfig": {"defaults": {"unit": "percent", "min": 0, "max": 100, "custom": {"stacking": {"mode": "normal", "group": "A"}}}}
                    },
                    {
                        "id": 4,
                        "title": "Cluster Network RX Overview",
                        "type": "timeseries",
                        "datasource": "GreptimeDB",
                        "gridPos": {"h": 8, "w": 12, "x": 0, "y": 8},
                        "targets": [
                            {
                                "expr": "rate(nokube_network_rx_bytes[5m])",
                                "legendFormat": "{{instance}} RX",
                                "interval": "30s"
                            }
                        ],
                        "fieldConfig": {
                            "defaults": {
                                "unit": "binBps"
                            }
                        }
                    },
                    {
                        "id": 5,
                        "title": "Cluster Network TX Overview",
                        "type": "timeseries",
                        "datasource": "GreptimeDB",
                        "gridPos": {"h": 8, "w": 12, "x": 12, "y": 8},
                        "targets": [
                            {
                                "expr": "rate(nokube_network_tx_bytes[5m])",
                                "legendFormat": "{{instance}} TX",
                                "interval": "30s"
                            }
                        ],
                        "fieldConfig": {
                            "defaults": {
                                "unit": "binBps"
                            }
                        }
                    },
                    {
                        "id": 6,
                        "title": "Node CPU (%) by Container [$node]",
                        "type": "timeseries",
                        "datasource": "GreptimeDB",
                        "gridPos": {"h": 8, "w": 12, "x": 0, "y": 16},
                        "targets": [
                            {"expr": "nokube_container_cpu{node=~\"$node\"}", "legendFormat": "{{container}}", "interval": "30s"}
                        ],
                        "fieldConfig": {"defaults": {"unit": "percent", "min": 0, "max": 100, "custom": {"stacking": {"mode": "normal", "group": "A"}, "fillOpacity": 40}}},
                        "repeat": "node",
                        "repeatDirection": "h"
                    },
                    {
                        "id": 7,
                        "title": "Node Memory (bytes) by Container [$node]",
                        "type": "timeseries",
                        "datasource": "GreptimeDB",
                        "gridPos": {"h": 8, "w": 12, "x": 12, "y": 16},
                        "targets": [
                            {"expr": "last_over_time(nokube_container_mem_bytes{node=~\"$node\"}[60s])", "legendFormat": "{{container}}", "interval": "30s"},
                            {"expr": "last_over_time(nokube_node_mem_other_bytes{node=~\"$node\"}[60s])", "legendFormat": "Other Used", "interval": "30s"},
                            {"expr": "last_over_time(nokube_node_mem_free_bytes{node=~\"$node\"}[60s])", "legendFormat": "Free", "interval": "30s"}
                        ],
                        "fieldConfig": {
                            "defaults": {"unit": "bytes", "min": 0, "custom": {"stacking": {"mode": "normal", "group": "A"}, "fillOpacity": 40}},
                            "overrides": [
                                {"matcher": {"id": "byName", "options": "Other Used"},
                                 "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": "red"}}]},
                                {"matcher": {"id": "byName", "options": "Free"},
                                 "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": "blue"}}]}
                            ]
                        },
                        "repeat": "node",
                        "repeatDirection": "h"
                    }
                ]
            },
            "overwrite": true
        });

        let client = Self::create_local_http_client()?;
        let grafana_url = format!("http://{}:{}/api/dashboards/db", node_ip, grafana_port);

        let grafana_user = self
            .config
            .task_spec
            .monitoring
            .grafana
            .admin_user
            .clone()
            .unwrap_or_else(|| "admin".to_string());
        let grafana_pass = self
            .config
            .task_spec
            .monitoring
            .grafana
            .admin_password
            .clone()
            .unwrap_or_else(|| "admin".to_string());
        let response = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth(&grafana_user, Some(&grafana_pass))
            .json(&dashboard_config)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Failed to import dashboard: {} - {}", status, error_text);
        }

        println!("[REALTIME] ✅ Successfully imported NoKube cluster monitoring dashboard");
        info!("Successfully imported NoKube cluster monitoring dashboard");

        Ok(())
    }

    async fn import_actor_dashboard(&self, grafana_port: u16, node_ip: &str) -> Result<()> {
        println!(
            "[REALTIME] 📋 Importing NoKube Actor Dashboard (root-actor rows, absolute metrics)"
        );
        info!("Importing NoKube Actor Dashboard (root-actor rows, absolute metrics)");

        let actor_dashboard_config = serde_json::json!({
            "dashboard": {
                "id": null,
                "uid": "nokube-actor-dashboard",
                "title": "NoKube Actor Dashboard",
                "tags": ["nokube", "actor", "root", "hierarchy", "container"],
                "timezone": "browser",
                "templating": {"list": [
                    {"name": "cluster", "type": "query", "label": "Cluster", "datasource": "GreptimeDB", "query": "label_values(nokube_container_cpu_cores, cluster_name)", "refresh": 1, "includeAll": true, "allValue": ".*", "multi": true, "current": {"text": "All", "value": ["$__all"]}},
                    {"name": "root_actor", "type": "query", "label": "Root Actor", "datasource": "GreptimeDB", "query": "label_values(nokube_container_cpu_cores{cluster_name=~\"$cluster\"}, root_actor)", "refresh": 1, "includeAll": false, "allValue": "", "multi": true, "current": {"text": "", "value": []}},
                    {"name": "node", "type": "query", "label": "Node", "datasource": "GreptimeDB", "query": "label_values(nokube_container_mem_bytes{cluster_name=~\"$cluster\", root_actor=~\"$root_actor\"}, node)", "refresh": 1, "includeAll": true, "allValue": ".*", "multi": true, "current": {"text": "All", "value": ["$__all"]}}
                ]},
                "panels": [
                    {"id": 1, "title": "Root Actors", "type": "stat", "datasource": "GreptimeDB",
                        "targets": [ {"expr": "count(count by (root_actor) (nokube_container_cpu_cores{cluster_name=~\"$cluster\"}))"} ],
                        "gridPos": {"h": 4, "w": 24, "x": 0, "y": 0}
                    },
                    {"id": 2, "title": "Container Count ($cluster)", "type": "stat", "datasource": "GreptimeDB",
                        "targets": [ {"expr": "count(count by (pod, container) (nokube_container_mem_bytes{cluster_name=~\"$cluster\"}))", "instant": true} ],
                        "gridPos": {"h": 4, "w": 24, "x": 0, "y": 4}
                    },

                    {"id": 100, "type": "row", "title": "$root_actor", "repeat": "root_actor", "collapsed": false, "gridPos": {"h": 1, "w": 24, "x": 0, "y": 8}},
                    {"id": 101, "title": "Pods of $root_actor (node/pod)", "type": "table", "datasource": "GreptimeDB",
                        "targets": [ {"expr": "sum by (pod, node) (nokube_container_mem_bytes{cluster_name=~\"$cluster\", root_actor=~\"$root_actor\"})", "format": "table", "instant": true} ],
                        "transformations": [
                            {"id": "labelsToFields", "options": {"mode": "columns"}},
                            {"id": "organize", "options": {"excludeByName": {"Time": true, "__name__": true, "instance": true, "job": true, "metric": true, "Value": false}, "renameByName": {"node": "Node", "pod": "Pod", "Value": "Mem Bytes"}}}
                        ],
                        "gridPos": {"h": 8, "w": 24, "x": 0, "y": 9}
                    },
                    {"id": 102, "title": "Containers of $root_actor (node/pod/container)", "type": "table", "datasource": "GreptimeDB",
                        "targets": [ {"expr": "nokube_container_mem_bytes{cluster_name=~\"$cluster\", root_actor=~\"$root_actor\"}", "format": "table", "instant": true} ],
                        "transformations": [
                            {"id": "labelsToFields", "options": {"mode": "columns"}},
                            {"id": "organize", "options": {"excludeByName": {"Time": true, "__name__": true, "instance": true, "job": true, "metric": true, "Value": false}, "renameByName": {"node": "Node", "pod": "Pod", "container": "Container", "root_actor": "Root Actor", "Value": "Mem Bytes"}}}
                        ],
                        "fieldConfig": {"defaults": {}, "overrides": [
                            {"matcher": {"id": "byName", "options": "Container"},
                             "properties": [
                                 {"id": "links", "value": [
                                     {"title": "View Logs", "url": "/d/nokube-logs-mysql?var-container_path=${__data.fields.container_path}", "targetBlank": true}
                                 ]}
                             ]},
                            {"matcher": {"id": "byName", "options": "container_path"},
                             "properties": [
                                 {"id": "links", "value": [
                                     {"title": "View Logs (by Path)", "url": "/d/nokube-logs-mysql?var-container_path=${__value.raw}", "targetBlank": true}
                                 ]},
                                 {"id": "custom.hidden", "value": true}
                             ]}
                        ]},
                        "gridPos": {"h": 8, "w": 24, "x": 0, "y": 17}
                    },
                    {"id": 103, "title": "Container CPU (cores) [$root_actor]", "type": "timeseries", "datasource": "GreptimeDB",
                        "targets": [ {"expr": "sum by (pod, container, node) (nokube_container_cpu_cores{cluster_name=~\"$cluster\", root_actor=~\"$root_actor\"})", "legendFormat": "{{pod}}/{{container}} @ {{node}}"} ],
                        "fieldConfig": {"defaults": {"unit": "cores", "min": 0, "custom": {"stacking": {"mode": "none"}}}},
                        "gridPos": {"h": 8, "w": 12, "x": 0, "y": 25}
                    },
                            {"id": 104, "title": "Container Memory (bytes) [$root_actor]", "type": "timeseries", "datasource": "GreptimeDB",
                                "targets": [
                                    {"expr": "sum by (pod, container, node) (nokube_container_mem_bytes{cluster_name=~\"$cluster\", root_actor=~\"$root_actor\"})", "legendFormat": "{{pod}}/{{container}} @ {{node}}"},
                                    {"expr": "nokube_memory_used_bytes{cluster_name=~\"$cluster\", node=~\"$node\"}", "legendFormat": "Node Used: {{node}}"},
                                    {"expr": "nokube_memory_total_bytes{cluster_name=~\"$cluster\", node=~\"$node\"}", "legendFormat": "Node Total: {{node}}"}
                                ],
                                "fieldConfig": {
                                    "defaults": {"unit": "bytes", "min": 0, "custom": {"stacking": {"mode": "normal", "group": "A"}}},
                                    "overrides": [
                                        {"matcher": {"id": "byRegexp", "options": "^Node Used.*$"},
                                         "properties": [
                                             {"id": "custom.stacking", "value": {"mode": "none"}},
                                             {"id": "custom.fillOpacity", "value": 0}
                                         ]},
                                        {"matcher": {"id": "byRegexp", "options": "^Node Total.*$"},
                                         "properties": [
                                             {"id": "custom.stacking", "value": {"mode": "none"}},
                                             {"id": "custom.fillOpacity", "value": 0}
                                         ]}
                                    ]
                                },
                                "gridPos": {"h": 8, "w": 12, "x": 12, "y": 25}
                            }
                ],
                "time": {"from": "now-1h", "to": "now"},
                "refresh": "30s",
                "schemaVersion": 30,
                "version": 2
            },
            "overwrite": true
        });

        let client = Self::create_local_http_client()?;
        let grafana_url = format!("http://{}:{}/api/dashboards/db", node_ip, grafana_port);

        let response = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth("admin", Some("admin"))
            .json(&actor_dashboard_config)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!(
                "Failed to import actor dashboard: {} - {}",
                status,
                error_text
            );
        }

        println!("[REALTIME] ✅ Successfully imported NoKube Actor Dashboard (root-actor rows, absolute metrics)");
        info!("Successfully imported NoKube Actor Dashboard (root-actor rows, absolute metrics)");

        Ok(())
    }

    async fn ensure_mysql_logs_datasource(
        &self,
        grafana_port: u16,
        node_ip: &str,
        greptimedb_port: u16,
    ) -> Result<()> {
        // Build MySQL DS config (GreptimeDB exposes MySQL protocol on base+2)
        let mysql_port = greptimedb_port + 2;
        let mysql_url = format!("{}:{}", node_ip, mysql_port);
        let (mysql_user, mysql_pass_opt) = if let Some(p) = &self.extra_params {
            (
                p.get("greptimedb_mysql_user")
                    .and_then(|v| v.as_str())
                    .unwrap_or("root")
                    .to_string(),
                p.get("greptimedb_mysql_password")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            )
        } else {
            ("root".to_string(), None)
        };

        let mut mysql_ds = serde_json::json!({
            "name": "greptimemysql",
            "type": "mysql",
            "url": mysql_url,
            "access": "proxy",
            "isDefault": false,
            "database": "public",
            "user": mysql_user,
            "jsonData": {"timeInterval": "1s"}
        });
        if let Some(pw) = mysql_pass_opt {
            mysql_ds.as_object_mut().unwrap().insert(
                "secureJsonData".to_string(),
                serde_json::json!({"password": pw}),
            );
        }

        let client = Self::create_local_http_client()?;
        let url = format!("http://{}:{}/api/datasources", node_ip, grafana_port);
        let grafana_user = self
            .config
            .task_spec
            .monitoring
            .grafana
            .admin_user
            .clone()
            .unwrap_or_else(|| "admin".to_string());
        let grafana_pass = self
            .config
            .task_spec
            .monitoring
            .grafana
            .admin_password
            .clone()
            .unwrap_or_else(|| "admin".to_string());
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .basic_auth(&grafana_user, Some(&grafana_pass))
            .json(&mysql_ds)
            .send()
            .await?;
        if resp.status().as_u16() == 409 {
            // Update existing
            if let Ok(val) = client
                .get(&format!(
                    "http://{}:{}/api/datasources/name/greptimemysql",
                    node_ip, grafana_port
                ))
                .basic_auth(&grafana_user, Some(&grafana_pass))
                .send()
                .await?
                .json::<serde_json::Value>()
                .await
            {
                if let Some(id) = val.get("id").and_then(|v| v.as_i64()) {
                    let _ = client
                        .put(&format!(
                            "http://{}:{}/api/datasources/{}",
                            node_ip, grafana_port, id
                        ))
                        .header("Content-Type", "application/json")
                        .basic_auth(&grafana_user, Some(&grafana_pass))
                        .json(&mysql_ds)
                        .send()
                        .await?;
                }
            }
        } else if !resp.status().is_success() {
            let status = resp.status();
            let error_text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Failed to configure MySQL datasource: {} - {}",
                status,
                error_text
            );
        }
        Ok(())
    }

    async fn import_logs_dashboard_mysql(&self, grafana_port: u16, node_ip: &str) -> Result<()> {
        let grafana_user = self
            .config
            .task_spec
            .monitoring
            .grafana
            .admin_user
            .clone()
            .unwrap_or_else(|| "admin".to_string());
        let grafana_pass = self
            .config
            .task_spec
            .monitoring
            .grafana
            .admin_password
            .clone()
            .unwrap_or_else(|| "admin".to_string());
        let dashboard_config = serde_json::json!({
            "dashboard": {
                "id": null,
                "uid": "nokube-logs-mysql",
                "title": "NoKube Logs (MySQL)",
                "tags": ["nokube", "logs", "mysql", "greptimedb"],
                "timezone": "browser",
                "panels": [
                    {"id": 1, "title": "Log Messages (Latest)", "type": "logs", "datasource": "greptimemysql",
                     "targets": [{"format":"table","rawSql":"SELECT timestamp AS time, body AS message, severity_text AS level FROM opentelemetry_logs WHERE $__timeFilter(timestamp) AND (${container_path:sqlstring} = '' OR scope_name = ${container_path:sqlstring}) ORDER BY timestamp DESC LIMIT 1000"}],
                     "options": {"showTime": true, "showLabels": false, "showCommonLabels": false, "wrapLogMessage": false, "enableLogDetails": false, "messageField": "message"},
                     "gridPos": {"h": 12, "w": 24, "x": 0, "y": 0}},
                    {"id": 2, "title": "Log Level Distribution", "type": "piechart", "datasource": "greptimemysql",
                     "targets": [{"format":"table","rawSql":"SELECT severity_text AS metric, COUNT(*) AS value FROM opentelemetry_logs WHERE $__timeFilter(timestamp) AND (${container_path:sqlstring} = '' OR scope_name = ${container_path:sqlstring}) GROUP BY severity_text"}],
                     "gridPos": {"h": 6, "w": 8, "x": 0, "y": 12}},
                    {"id": 3, "title": "Logs per Minute", "type": "timeseries", "datasource": "greptimemysql",
                     "targets": [{"format":"time_series","rawSql":"SELECT $__timeGroup(timestamp, '1m') AS time, 'All Logs' AS metric, COUNT(*) AS value FROM opentelemetry_logs WHERE $__timeFilter(timestamp) AND (${container_path:sqlstring} = '' OR scope_name = ${container_path:sqlstring}) GROUP BY 1 ORDER BY 1"}],
                     "gridPos": {"h": 6, "w": 16, "x": 8, "y": 12}}
                ],
                "templating": {"list": [
                    {"name": "container_path", "type": "query", "datasource": "greptimemysql", "query": "SELECT DISTINCT scope_name AS text FROM opentelemetry_logs WHERE scope_name <> '' ORDER BY text", "refresh": 1, "includeAll": true, "allValue": "", "multi": false, "current": {"text": "", "value": ""}}
                ]},
                "time": {"from": "now-6h", "to": "now"},
                "refresh": "30s",
                "schemaVersion": 30,
                "version": 1
            },
            "overwrite": true
        });

        let client = Self::create_local_http_client()?;
        let grafana_url = format!("http://{}:{}/api/dashboards/db", node_ip, grafana_port);
        let response = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth(&grafana_user, Some(&grafana_pass))
            .json(&dashboard_config)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!(
                "Failed to import MySQL logs dashboard: {} - {}",
                status,
                error_text
            );
        }

        // 删除可能存在的旧 OTLP 仪表盘
        let search_url = format!(
            "http://{}:{}/api/search?query=NoKube%20Logs%20Dashboard%20(OTLP)",
            node_ip, grafana_port
        );
        if let Ok(search_resp) = client
            .get(&search_url)
            .basic_auth(&grafana_user, Some(&grafana_pass))
            .send()
            .await
        {
            if search_resp.status().is_success() {
                if let Ok(items) = search_resp.json::<serde_json::Value>().await {
                    if let Some(arr) = items.as_array() {
                        for it in arr {
                            if let Some(uid) = it.get("uid").and_then(|v| v.as_str()) {
                                let _ = client
                                    .delete(&format!(
                                        "http://{}:{}/api/dashboards/uid/{}",
                                        node_ip, grafana_port, uid
                                    ))
                                    .basic_auth(&grafana_user, Some(&grafana_pass))
                                    .send()
                                    .await;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
