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

    /// 从 extra_params 获取 workspace 路径，如果没有则报错
    fn get_workspace(&self) -> Result<&str> {
        self.extra_params
            .as_ref()
            .and_then(|params| params.get("workspace"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required workspace in extra_params"))
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

        info!("Environment configuration completed with workspace: {}", workspace);
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

        // 获取当前用户信息（从环境变量获取，如果缺失则报错）
        let current_user = std::env::var("USER")
            .map_err(|_| anyhow::anyhow!("USER environment variable not set - cannot determine user"))?;
        let home_dir = std::env::var("HOME")
            .map_err(|_| anyhow::anyhow!("HOME environment variable not set - cannot determine home directory"))?;
        
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
        let workspace = self.get_workspace()?;
        let remote_lib_path = format!("{}/nokube-remote-lib", workspace);
        
        let start_result = std::process::Command::new("sudo")
            .args(&[
                "docker", "run", "-d",
                "--name", "nokube-agent-container",
                "--restart", "unless-stopped",
                "--pid", "host", // 使用宿主机PID命名空间，观测宿主机所有进程信息
                "--privileged", // 特权模式，获得宿主机完全访问权限
                "-v", &format!("{}:{}", home_dir, home_dir),
                "-v", &format!("{}:{}", remote_lib_path, remote_lib_path),
                "-v", &format!("{}:/etc/.nokube/config.yaml", host_config_path), // 挂载配置文件到容器标准路径
                "-e", &format!("LD_LIBRARY_PATH={}", remote_lib_path),
                "-e", &format!("HOME={}", home_dir),
                "--network", "host",
                "--workdir", &home_dir,
                "ubuntu:22.04",
                "bash", "-c", 
                &format!("while [ ! -f {}/nokube ]; do echo 'Waiting for nokube binary...'; sleep 1; done && chmod +x {}/nokube && {}/nokube agent-service --extra-params {}", remote_lib_path, remote_lib_path, remote_lib_path, extra_params)
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
                    
                    // 从方法获取workspace路径
                    let workspace = self.get_workspace()?;
                    
                    // 获取Grafana配置和端口
                    let grafana_config = params.get("grafana_config")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("Missing required grafana_config in extra_params"))?;
                    let grafana_port = params.get("grafana_port")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| anyhow::anyhow!("Missing required grafana_port in extra_params"))?
                        as u16;
                    
                    // 创建workspace内的config目录
                    let config_dir = format!("{}/config", workspace);
                    std::fs::create_dir_all(&config_dir)
                        .map_err(|e| anyhow::anyhow!("Failed to create config directory {}: {}", config_dir, e))?;
                    
                    // 在workspace内创建Grafana配置文件
                    let grafana_config_path = format!("{}/grafana.ini", config_dir);
                    std::fs::write(&grafana_config_path, grafana_config)
                        .map_err(|e| anyhow::anyhow!("Failed to create Grafana config file: {}", e))?;
                    info!("Grafana config file created at {}", grafana_config_path);
                    
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
                            "-v", &format!("{}:/etc/grafana/grafana.ini", grafana_config_path),
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
                            
                            // 检查Grafana端口是否真正可用
                            let workspace = self.get_workspace()?;
                            let node_ip = self.extra_params
                                .as_ref()
                                .and_then(|params| params.get("node_ip"))
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| anyhow::anyhow!("Missing required node_ip in extra_params"))?;
                            
                            info!("Checking if Grafana is responding on {}:{}", node_ip, grafana_port);
                            let mut retries = 0;
                            let max_retries = 30; // 30秒超时
                            let mut grafana_ready = false;
                            
                            while retries < max_retries {
                                match reqwest::Client::new()
                                    .get(&format!("http://{}:{}/api/health", node_ip, grafana_port))
                                    .timeout(std::time::Duration::from_secs(1))
                                    .send()
                                    .await {
                                    Ok(response) => {
                                        if response.status().is_success() {
                                            info!("Grafana is responding on {}:{}", node_ip, grafana_port);
                                            grafana_ready = true;
                                            break;
                                        }
                                    }
                                    Err(_) => {
                                        // 继续等待
                                    }
                                }
                                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                                retries += 1;
                            }
                            
                            if !grafana_ready {
                                anyhow::bail!("Grafana container is running but not responding on {}:{} after {} seconds", node_ip, grafana_port, max_retries);
                            }
                            
                            // 启动 GreptimeDB（如果需要）
                            match self.setup_greptimedb_if_needed(&workspace).await {
                                Ok(_) => info!("GreptimeDB setup completed"),
                                Err(e) => {
                                    error!("Failed to setup GreptimeDB: {}", e);
                                    anyhow::bail!("GreptimeDB setup failed: {}", e);
                                }
                            }
                            
                            // 配置数据源和仪表板
                            match self.setup_grafana_datasource_and_dashboard(grafana_port, &workspace).await {
                                Ok(_) => info!("Grafana datasource and dashboard configured successfully"),
                                Err(e) => {
                                    error!("Failed to configure Grafana datasource/dashboard: {}", e);
                                    anyhow::bail!("Grafana configuration failed: {}", e);
                                }
                            }
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

    async fn setup_greptimedb_if_needed(&self, workspace: &str) -> Result<()> {
        info!("Setting up GreptimeDB if needed");
        
        let greptimedb_port = if let Some(params) = &self.extra_params {
            params.get("greptimedb_port")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("Missing required greptimedb_port in extra_params"))?
                as u16
        } else {
            anyhow::bail!("Missing extra_params - cannot determine GreptimeDB port");
        };

        // 检查 GreptimeDB 容器是否已经运行
        let check_result = std::process::Command::new("sudo")
            .args(&["docker", "ps", "--filter", "name=nokube-greptimedb", "--filter", "status=running", "--quiet"])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to check GreptimeDB container: {}", e))?;

        if check_result.status.success() {
            let output = String::from_utf8_lossy(&check_result.stdout);
            if !output.trim().is_empty() {
                info!("GreptimeDB container already running");
                return Ok(());
            }
        }

        info!("Starting GreptimeDB container on port {}", greptimedb_port);

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

        // 启动GreptimeDB容器
        let start_result = std::process::Command::new("sudo")
            .args(&[
                "docker", "run", "-d",
                "--name", "nokube-greptimedb",
                "-p", &format!("{}:4000", greptimedb_port),
                "-p", &format!("{}:4001", greptimedb_port + 1), // gRPC端口
                "-p", &format!("{}:4002", greptimedb_port + 2), // MySQL端口  
                "-p", &format!("{}:4003", greptimedb_port + 3), // PostgreSQL端口
                "-v", &format!("{}:/tmp/greptimedb", data_dir),
                "--restart", "unless-stopped",
                "greptime/greptimedb:latest",
                "standalone",
                "start",
                "--http-addr", "0.0.0.0:4000",
                "--rpc-addr", "0.0.0.0:4001"
            ])
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to start GreptimeDB container: {}", e))?;

        if !start_result.status.success() {
            let error_msg = String::from_utf8_lossy(&start_result.stderr);
            anyhow::bail!("Failed to start GreptimeDB container: {}", error_msg);
        }

        let container_output = String::from_utf8_lossy(&start_result.stdout);
        let container_id = container_output.trim();
        info!("GreptimeDB container started with ID: {}", container_id);

        // 等待GreptimeDB启动
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

        // 验证GreptimeDB是否正常运行
        let verify_result = std::process::Command::new("sudo")
            .args(&["docker", "ps", "--filter", "name=nokube-greptimedb", "--filter", "status=running", "--quiet"])
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
                    .output() {
                    let logs = String::from_utf8_lossy(&logs_result.stdout);
                    info!("GreptimeDB container logs: {}", logs);
                }
                anyhow::bail!("GreptimeDB container failed to start");
            }
        }

        Ok(())
    }

    async fn setup_grafana_datasource_and_dashboard(&self, grafana_port: u16, _workspace: &str) -> Result<()> {
        info!("Setting up Grafana datasource and dashboard");
        
        // 等待 Grafana 完全启动
        tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
        
        // 从extra_params获取节点IP和GreptimeDB信息
        let node_ip = self.extra_params
            .as_ref()
            .and_then(|params| params.get("node_ip"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required node_ip in extra_params"))?;
            
        let greptimedb_port = if let Some(params) = &self.extra_params {
            params.get("greptimedb_port")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("Missing required greptimedb_port in extra_params"))?
                as u16
        } else {
            anyhow::bail!("Missing extra_params - cannot determine GreptimeDB port");
        };
        
        // 使用节点IP而不是localhost
        let greptimedb_endpoint = format!("http://{}:{}", node_ip, greptimedb_port);
        let prometheus_api_url = format!("{}/v1/prometheus", greptimedb_endpoint);
        
        // 配置数据源
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
        let grafana_url = format!("http://{}:{}/api/datasources", node_ip, grafana_port);
        
        // 尝试配置数据源
        let response = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth("admin", Some("admin"))
            .json(&datasource_config)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Failed to configure datasource: {} - {}", status, error_text);
        }
        
        info!("Successfully configured GreptimeDB datasource");
        
        // 导入仪表板
        self.import_nokube_dashboard(grafana_port, node_ip).await?;
        
        Ok(())
    }

    async fn import_nokube_dashboard(&self, grafana_port: u16, node_ip: &str) -> Result<()> {
        info!("Importing NoKube cluster monitoring dashboard");
        
        let dashboard_config = serde_json::json!({
            "dashboard": {
                "id": null,
                "title": "NoKube Cluster Monitoring",
                "tags": ["nokube", "cluster"],
                "timezone": "browser",
                "refresh": "30s",
                "time": {
                    "from": "now-1h",
                    "to": "now"
                },
                "templating": {
                    "list": [
                        {
                            "allValue": null,
                            "current": {},
                            "datasource": "GreptimeDB",
                            "definition": "label_values(nokube_cpu_usage, instance)",
                            "hide": 0,
                            "includeAll": true,
                            "label": "Node",
                            "multi": true,
                            "name": "node",
                            "options": [],
                            "query": "label_values(nokube_cpu_usage, instance)",
                            "refresh": 1,
                            "regex": "",
                            "skipUrlSync": false,
                            "sort": 1,
                            "tagValuesQuery": "",
                            "tagsQuery": "",
                            "type": "query",
                            "useTags": false
                        }
                    ]
                },
                "panels": [
                    {
                        "id": 1,
                        "title": "Cluster Overview",
                        "type": "row",
                        "gridPos": {"h": 1, "w": 24, "x": 0, "y": 0},
                        "collapsed": false
                    },
                    {
                        "id": 2,
                        "title": "Cluster CPU Usage Overview",
                        "type": "timeseries",
                        "gridPos": {"h": 8, "w": 12, "x": 0, "y": 1},
                        "targets": [
                            {
                                "expr": "nokube_cpu_usage",
                                "legendFormat": "{{instance}}",
                                "interval": "30s"
                            }
                        ],
                        "fieldConfig": {
                            "defaults": {
                                "unit": "percent",
                                "min": 0,
                                "max": 100,
                                "thresholds": {
                                    "steps": [
                                        {"color": "green", "value": null},
                                        {"color": "yellow", "value": 60},
                                        {"color": "red", "value": 80}
                                    ]
                                }
                            }
                        }
                    },
                    {
                        "id": 3,
                        "title": "Cluster Memory Usage Overview",
                        "type": "timeseries",
                        "gridPos": {"h": 8, "w": 12, "x": 12, "y": 1},
                        "targets": [
                            {
                                "expr": "nokube_memory_usage",
                                "legendFormat": "{{instance}}",
                                "interval": "30s"
                            }
                        ],
                        "fieldConfig": {
                            "defaults": {
                                "unit": "percent",
                                "min": 0,
                                "max": 100,
                                "thresholds": {
                                    "steps": [
                                        {"color": "green", "value": null},
                                        {"color": "yellow", "value": 70},
                                        {"color": "red", "value": 85}
                                    ]
                                }
                            }
                        }
                    },
                    {
                        "id": 4,
                        "title": "Cluster Network RX Overview",
                        "type": "timeseries", 
                        "gridPos": {"h": 8, "w": 12, "x": 0, "y": 9},
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
                        "gridPos": {"h": 8, "w": 12, "x": 12, "y": 9},
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
                        "title": "Node Details - $node",
                        "type": "row",
                        "gridPos": {"h": 1, "w": 24, "x": 0, "y": 17},
                        "collapsed": false,
                        "repeat": "node"
                    },
                    {
                        "id": 7,
                        "title": "CPU Usage",
                        "type": "timeseries",
                        "gridPos": {"h": 6, "w": 8, "x": 0, "y": 18},
                        "repeat": "node",
                        "repeatDirection": "v",
                        "targets": [
                            {
                                "expr": "nokube_cpu_usage{instance=~\"$node\"}",
                                "legendFormat": "CPU",
                                "interval": "30s"
                            }
                        ],
                        "fieldConfig": {
                            "defaults": {
                                "unit": "percent",
                                "min": 0,
                                "max": 100,
                                "thresholds": {
                                    "steps": [
                                        {"color": "green", "value": null},
                                        {"color": "yellow", "value": 60},
                                        {"color": "red", "value": 80}
                                    ]
                                }
                            }
                        }
                    },
                    {
                        "id": 8,
                        "title": "Memory Usage",
                        "type": "timeseries",
                        "gridPos": {"h": 6, "w": 8, "x": 8, "y": 18},
                        "repeat": "node",
                        "repeatDirection": "v",
                        "targets": [
                            {
                                "expr": "nokube_memory_usage{instance=~\"$node\"}",
                                "legendFormat": "Memory",
                                "interval": "30s"
                            }
                        ],
                        "fieldConfig": {
                            "defaults": {
                                "unit": "percent",
                                "min": 0,
                                "max": 100,
                                "thresholds": {
                                    "steps": [
                                        {"color": "green", "value": null},
                                        {"color": "yellow", "value": 70},
                                        {"color": "red", "value": 85}
                                    ]
                                }
                            }
                        }
                    },
                    {
                        "id": 9,
                        "title": "Network I/O",
                        "type": "timeseries", 
                        "gridPos": {"h": 6, "w": 8, "x": 16, "y": 18},
                        "repeat": "node",
                        "repeatDirection": "v",
                        "targets": [
                            {
                                "expr": "rate(nokube_network_rx_bytes{instance=~\"$node\"}[5m])",
                                "legendFormat": "RX",
                                "interval": "30s"
                            },
                            {
                                "expr": "rate(nokube_network_tx_bytes{instance=~\"$node\"}[5m])",
                                "legendFormat": "TX",
                                "interval": "30s"
                            }
                        ],
                        "fieldConfig": {
                            "defaults": {
                                "unit": "binBps"
                            }
                        }
                    }
                ]
            },
            "overwrite": true
        });
        
        let client = reqwest::Client::new();
        let grafana_url = format!("http://{}:{}/api/dashboards/db", node_ip, grafana_port);
        
        let response = client
            .post(&grafana_url)
            .header("Content-Type", "application/json")
            .basic_auth("admin", Some("admin"))
            .json(&dashboard_config)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Failed to import dashboard: {} - {}", status, error_text);
        }
        
        info!("Successfully imported NoKube cluster monitoring dashboard");
        
        Ok(())
    }
}
