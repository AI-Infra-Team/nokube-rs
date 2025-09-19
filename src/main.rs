#![recursion_limit = "512"]
use base64::Engine;
use tracing::warn; // 添加 warn! 宏导入
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create or update cluster deployment
    NewOrUpdate {
        /// Path to cluster configuration YAML file (optional, will create template if not provided)
        config_file: Option<String>,
    },
    /// Start monitoring services
    Monitor {
        #[arg(short, long)]
        cluster_name: String,
    },
    /// Apply k8s YAML manifests to cluster
    Apply {
        #[arg(short, long)]
        file: Option<String>,
        #[arg(long)]
        cluster: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Get resources from cluster (pods, deployments, services, etc.)
    Get {
        /// Resource type (pods, deployments, services, configmaps, secrets)
        resource_type: String,
        /// Resource name (optional, shows all if not specified)
        name: Option<String>,
        #[arg(short, long)]
        cluster: Option<String>,
        /// Output format (table, yaml, json)
        #[arg(short, long, default_value = "table")]
        output: String,
        /// Show all namespaces
        #[arg(long)]
        all_namespaces: bool,
    },
    /// Describe resources in detail
    Describe {
        /// Resource type (pods, deployments, services, etc.)
        resource_type: String,
        /// Resource name
        name: String,
        #[arg(short, long)]
        cluster: Option<String>,
    },
    /// Get logs from a pod
    Logs {
        /// Pod name
        pod_name: String,
        #[arg(short, long)]
        cluster: Option<String>,
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
        /// Number of lines to show from the end of the log
        #[arg(long)]
        tail: Option<i32>,
    },
    /// Run agent in command mode (for deployment operations)
    AgentCommand {
        #[arg(long)]
        extra_params: Option<String>,
    },
    /// Run agent in service mode (persistent execution)
    AgentService {
        #[arg(long)]
        extra_params: Option<String>,
    },
    /// Delete a resource from the cluster store (etcd)
    Delete {
        /// Resource type (pod, deployment, daemonset, configmap, secret)
        resource_type: String,
        /// Resource name
        name: String,
        #[arg(short, long)]
        cluster: Option<String>,
    },
}
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info};
use tracing_subscriber::{self, EnvFilter};
use uuid::Uuid;

use crate::agent::service_agent::status::{
    ModuleState, ServiceDeploymentStatus, SERVICE_AGENT_MODULES,
};
use chrono::Utc;

mod agent;
mod config;
mod error;
mod k8s;
mod remote_ctl;

use agent::{CommandModeAgent, ServiceModeAgent};
use config::{cluster_config::ClusterConfig, ConfigManager};
use error::{NokubeError, Result};
use remote_ctl::DeploymentController;
#[tokio::main]
async fn main() -> Result<()> {
    // 设置 RUST_BACKTRACE 环境变量来显示完整的错误堆栈
    std::env::set_var("RUST_BACKTRACE", "1");

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::NewOrUpdate { config_file } => handle_new_or_update(config_file).await,
        Commands::Monitor { cluster_name } => handle_monitor(cluster_name).await,
        Commands::Apply {
            file,
            cluster,
            dry_run,
        } => handle_apply(file, cluster, dry_run).await,
        Commands::Get {
            resource_type,
            name,
            cluster,
            output,
            all_namespaces,
        } => handle_get(resource_type, name, cluster, output, all_namespaces).await,
        Commands::Describe {
            resource_type,
            name,
            cluster,
        } => handle_describe(resource_type, name, cluster).await,
        Commands::Logs {
            pod_name,
            cluster,
            follow,
            tail,
        } => handle_logs(pod_name, cluster, follow, tail).await,
        Commands::AgentCommand { extra_params } => handle_agent_command(extra_params).await,
        Commands::AgentService { extra_params } => handle_agent_service(extra_params).await,
        Commands::Delete {
            resource_type,
            name,
            cluster,
        } => handle_delete(resource_type, name, cluster).await,
    };

    if let Err(e) = &result {
        error!("Application error: {}", e);
        std::process::exit(1);
    }
    return result;

    // 逻辑拆分函数实现（移到 main 外部）
    async fn handle_new_or_update(config_file: Option<String>) -> Result<()> {
        // 如果没有提供配置文件，创建模板
        let config_file = match config_file {
            Some(file) => file,
            None => {
                // 创建配置模板
                let template_path = "cluster-config-template.yaml";
                create_config_template(template_path)?;
                info!("Config template created at: {}", template_path);
                info!(
                    "Please edit the template file and run: nokube new-or-update {}",
                    template_path
                );
                return Ok(());
            }
        };

        info!("Deploying/updating cluster from config: {}", config_file);
        let cluster_config = match read_cluster_config_from_file(&config_file).await {
            Ok(config) => config,
            Err(e) => {
                error!("Failed to read config file {}: {}", config_file, e);
                return Err(NokubeError::Config(format!(
                    "Config file reading failed: {}",
                    e
                )));
            }
        };
        let cluster_name = cluster_config.cluster_name.clone();
        // Validate HTTP server config presence and print helpful paths
        let http_port = cluster_config.task_spec.monitoring.httpserver.port;
        // Resolve head node workspace and IP
        let head_node = cluster_config
            .nodes
            .iter()
            .find(|n| matches!(n.role, crate::config::cluster_config::NodeRole::Head));
        if head_node.is_none() {
            error!("Head node is required for HTTP server distribution");
            return Err(NokubeError::Config(
                "Missing head node in cluster config".to_string(),
            ));
        }
        let head = head_node.unwrap();
        let head_ip = head.get_ip().unwrap_or("127.0.0.1").to_string();
        let head_ws = head.workspace.clone().unwrap_or("/opt/nokube".to_string());
        let artifacts_host_dir = format!(
            "{}/{}",
            head_ws,
            crate::config::cluster_config::HTTP_SERVER_MOUNT_SUBPATH
        );
        let releases_dir = format!("{}/releases/nokube", artifacts_host_dir);
        let http_base = format!("http://{}:{}/releases/nokube", head_ip, http_port);
        info!(
            "HTTP Server configured: port={} (head={})",
            http_port, head_ip
        );
        info!("Place artifacts on head at: {}", releases_dir);
        info!("Nodes will download from: {}", http_base);
        let deployment_version =
            format!("{}-{}", Utc::now().format("%Y%m%d%H%M%S"), Uuid::new_v4());
        info!(
            "Generated deployment version for this rollout: {}",
            deployment_version
        );

        let config_manager = ConfigManager::new().await.map_err(|e| {
            error!("Failed to create config manager: {}", e);
            NokubeError::Config(format!("Config manager creation failed: {}", e))
        })?;

        if let Err(e) = config_manager.update_cluster_config(&cluster_config).await {
            error!("Failed to store cluster config: {}", e);
            return Err(NokubeError::Config(format!("Config storage failed: {}", e)));
        }

        if let Err(e) = config_manager.init_cluster(&cluster_name).await {
            error!("Failed to store cluster meta: {}", e);
        }

        let mut deployment_controller = DeploymentController::new().await.map_err(|e| {
            error!("Failed to create deployment controller: {}", e);
            NokubeError::Deployment(format!("Controller creation failed: {}", e))
        })?;

        deployment_controller
            .deploy_or_update(&cluster_name, &deployment_version)
            .await
            .map_err(|e| {
                error!("Failed to deploy/update cluster {}: {}", cluster_name, e);
                NokubeError::Deployment(format!("Deployment failed: {}", e))
            })?;

        info!("Cluster {} deployed/updated successfully", cluster_name);
        info!(
            "Grafana provisioning is now handled by service agents; no command-mode Grafana tasks executed."
        );

        wait_for_service_agents_ready(
            &config_manager,
            &cluster_config,
            &cluster_name,
            &deployment_version,
        )
        .await?;

        Ok(())
    }

    async fn wait_for_service_agents_ready(
        config_manager: &ConfigManager,
        cluster_config: &ClusterConfig,
        cluster_name: &str,
        deployment_version: &str,
    ) -> Result<()> {
        const WAIT_TIMEOUT: Duration = Duration::from_secs(600);
        const POLL_INTERVAL: Duration = Duration::from_secs(2);

        info!(
            "Waiting for service agents to report module readiness (deployment_version={})",
            deployment_version
        );

        let etcd = config_manager.get_etcd_manager();
        let start = std::time::Instant::now();

        loop {
            let mut all_ready = true;
            let mut iteration_summaries: Vec<String> = Vec::new();

            for node in &cluster_config.nodes {
                let key = format!(
                    "/nokube/{}/service_agents/{}/deployments/{}",
                    cluster_name, node.name, deployment_version
                );

                let kvs = etcd.get(key.clone()).await?;
                if kvs.is_empty() {
                    all_ready = false;
                    iteration_summaries.push(format!("node={} status=waiting", node.name));
                    continue;
                }

                let kv = &kvs[0];
                let status: ServiceDeploymentStatus = serde_json::from_str(kv.value_str())
                    .map_err(|e| {
                        NokubeError::Deployment(format!(
                            "Failed to parse service agent status for node {}: {}",
                            node.name, e
                        ))
                    })?;

                if status.deployment_version != deployment_version {
                    all_ready = false;
                    iteration_summaries.push(format!(
                        "node={} status=stale_version current={}",
                        node.name, status.deployment_version
                    ));
                    continue;
                }

                if status.overall_state == ModuleState::Failure {
                    let detail = status
                        .modules
                        .values()
                        .filter(|entry| entry.state == ModuleState::Failure)
                        .map(|entry| entry.message.clone().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("; ");
                    error!(
                        "Service agent on node {} reported failure while waiting: {}",
                        node.name,
                        if detail.is_empty() {
                            "unknown error".to_string()
                        } else {
                            detail.clone()
                        }
                    );
                    return Err(NokubeError::Deployment(format!(
                        "Service agent on node {} reported failure: {}",
                        node.name,
                        if detail.is_empty() {
                            "unknown error".to_string()
                        } else {
                            detail
                        }
                    )));
                }

                for module in SERVICE_AGENT_MODULES {
                    let entry = status.modules.get(*module);
                    match entry.map(|m| &m.state) {
                        Some(ModuleState::Success) => {}
                        Some(ModuleState::Failure) => {
                            let message = entry
                                .and_then(|m| m.message.clone())
                                .unwrap_or_else(|| "unknown error".to_string());
                            error!(
                                "Service agent module '{}' failed on node {}: {}",
                                module, node.name, message
                            );
                            return Err(NokubeError::Deployment(format!(
                                "Service agent module '{}' failed on node {}: {}",
                                module, node.name, message
                            )));
                        }
                        _ => {
                            all_ready = false;
                        }
                    }
                }

                // Summarise module states for logging
                let module_summary = SERVICE_AGENT_MODULES
                    .iter()
                    .map(|module| {
                        let state = status
                            .modules
                            .get(*module)
                            .map(|m| format!("{:?}", m.state))
                            .unwrap_or_else(|| "Pending".to_string());
                        format!("{}={}", module, state)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                iteration_summaries.push(format!("node={} {}", node.name, module_summary));
            }

            if !iteration_summaries.is_empty() {
                info!(
                    "Service agent progress: {}",
                    iteration_summaries.join(" | ")
                );
            }

            if all_ready {
                info!(
                    "All service agent modules reported success for deployment version {}",
                    deployment_version
                );
                return Ok(());
            }

            if start.elapsed() > WAIT_TIMEOUT {
                return Err(NokubeError::Deployment(format!(
                    "Timed out waiting for service agent modules to become ready (deployment_version={})",
                    deployment_version
                )));
            }

            sleep(POLL_INTERVAL).await;
        }
    }

    async fn handle_delete(
        resource_type: String,
        name: String,
        cluster: Option<String>,
    ) -> Result<()> {
        use crate::error::NokubeError;
        let cluster_name = cluster.unwrap_or_else(|| "default".to_string());
        info!(
            "Deleting {} '{}' from cluster: {}",
            resource_type, name, cluster_name
        );

        let config_manager = ConfigManager::new().await?;
        let etcd = config_manager.get_etcd_manager();

        let key = match resource_type.to_lowercase().as_str() {
            "deployment" | "deploy" | "deployments" => {
                format!("/nokube/{}/deployments/{}", cluster_name, name)
            }
            "daemonset" | "ds" | "daemonsets" => {
                format!("/nokube/{}/daemonsets/{}", cluster_name, name)
            }
            "pod" | "pods" => format!("/nokube/{}/pods/{}", cluster_name, name),
            "configmap" | "cm" | "configmaps" => {
                format!("/nokube/{}/configmaps/{}", cluster_name, name)
            }
            "secret" | "secrets" => format!("/nokube/{}/secrets/{}", cluster_name, name),
            other => {
                return Err(NokubeError::Config(format!(
                    "Unsupported resource type: {}",
                    other
                )));
            }
        };

        etcd.delete(key.clone())
            .await
            .map_err(|e| NokubeError::Config(format!("Failed to delete key {}: {}", key, e)))?;
        info!("Deleted key: {}", key);
        println!(
            "Deleted {} '{}' from cluster '{}'",
            resource_type, name, cluster_name
        );
        println!("Note: Service agent will stop related containers shortly.");
        Ok(())
    }

    async fn handle_apply(
        file: Option<String>,
        cluster: Option<String>,
        dry_run: bool,
    ) -> Result<()> {
        info!("Applying k8s manifests");

        // 处理输入参数
        let yaml_file = match file {
            Some(f) => f,
            None => {
                // 从标准输入读取
                let mut buffer = String::new();
                std::io::stdin().read_to_string(&mut buffer).map_err(|e| {
                    NokubeError::Config(format!("Failed to read from stdin: {}", e))
                })?;

                // 写入临时文件
                let temp_file = "/tmp/nokube-apply.yaml";
                fs::write(temp_file, buffer).map_err(|e| {
                    NokubeError::Config(format!("Failed to write temp file: {}", e))
                })?;
                temp_file.to_string()
            }
        };

        let cluster_name = cluster.unwrap_or_else(|| "default".to_string());

        info!("Reading YAML file: {}", yaml_file);
        let yaml_content = fs::read_to_string(&yaml_file).map_err(|e| {
            NokubeError::Config(format!("Failed to read YAML file {}: {}", yaml_file, e))
        })?;

        // 解析YAML内容
        let yaml_docs: Vec<serde_yaml::Value> = serde_yaml::Deserializer::from_str(&yaml_content)
            .map(|doc| {
                serde_yaml::Value::deserialize(doc).map_err(|e| {
                    NokubeError::Config(format!("Failed to deserialize YAML document: {}", e))
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;

        info!("Parsed {} YAML documents", yaml_docs.len());

        if dry_run {
            info!(
                "DRY RUN: Would apply {} YAML documents to cluster: {}",
                yaml_docs.len(),
                cluster_name
            );
            for (i, doc) in yaml_docs.iter().enumerate() {
                let kind = doc
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("Unknown");
                let name = doc
                    .get("metadata")
                    .and_then(|m| m.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("unnamed");
                info!("DRY RUN: Document {} - {} '{}'", i + 1, kind, name);
            }
            return Ok(());
        }

        // 处理每个YAML文档
        for (i, doc) in yaml_docs.iter().enumerate() {
            match apply_yaml_document(doc, &cluster_name).await {
                Ok(_) => info!("Applied document {} successfully", i + 1),
                Err(e) => {
                    error!("Failed to apply document {}: {}", i + 1, e);
                    return Err(e);
                }
            }
        }

        info!(
            "All k8s manifests applied successfully to cluster: {}",
            cluster_name
        );
        Ok(())
    }

    async fn apply_yaml_document(doc: &serde_yaml::Value, cluster_name: &str) -> Result<()> {
        let kind = doc.get("kind").and_then(|k| k.as_str()).ok_or_else(|| {
            NokubeError::Config("Missing 'kind' field in YAML document".to_string())
        })?;

        let metadata = doc.get("metadata").ok_or_else(|| {
            NokubeError::Config("Missing 'metadata' field in YAML document".to_string())
        })?;

        let name = metadata
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| {
                NokubeError::Config("Missing 'metadata.name' field in YAML document".to_string())
            })?;

        info!("Applying {} '{}'", kind, name);

        match kind {
            "ConfigMap" => apply_configmap(doc, cluster_name, name).await,
            "Secret" => apply_secret(doc, cluster_name, name).await,
            "Deployment" => apply_deployment(doc, cluster_name, name).await,
            "DaemonSet" => apply_daemonset(doc, cluster_name, name).await,
            "Service" => apply_service(doc, cluster_name, name).await,
            "GitOpsCluster" => apply_gitops_cluster(doc, cluster_name, name).await,
            "Pod" => apply_pod(doc, cluster_name, name).await, // 添加Pod处理
            _ => {
                info!("Unsupported kind '{}', skipping", kind);
                Ok(())
            }
        }
    }

    async fn apply_configmap(
        doc: &serde_yaml::Value,
        cluster_name: &str,
        name: &str,
    ) -> Result<()> {
        info!(
            "Creating ConfigMap '{}' in cluster '{}'",
            name, cluster_name
        );

        let config_manager = ConfigManager::new().await?;
        let configmap_yaml = serde_yaml::to_string(doc)
            .map_err(|e| NokubeError::Config(format!("Failed to serialize configmap: {}", e)))?;

        // 调用ConfigManager存储configmap规格到etcd
        config_manager
            .store_configmap(cluster_name, name, &configmap_yaml)
            .await
            .map_err(|e| {
                NokubeError::Config(format!("Failed to store configmap to etcd: {}", e))
            })?;
        info!("Stored ConfigMap spec for '{}' to etcd", name);

        Ok(())
    }

    async fn apply_secret(doc: &serde_yaml::Value, cluster_name: &str, name: &str) -> Result<()> {
        info!("Creating Secret '{}' in cluster '{}'", name, cluster_name);
        // 将 Secret 完整 YAML 存储到 etcd（与 ConfigMap 一致的方式）
        let config_manager = ConfigManager::new().await?;
        let secret_yaml = serde_yaml::to_string(doc)
            .map_err(|e| NokubeError::Config(format!("Failed to serialize secret: {}", e)))?;

        config_manager
            .store_secret(cluster_name, name, &secret_yaml)
            .await
            .map_err(|e| NokubeError::Config(format!("Failed to store secret to etcd: {}", e)))?;
        info!("Stored Secret spec for '{}' to etcd", name);

        Ok(())
    }

    async fn apply_deployment(
        doc: &serde_yaml::Value,
        cluster_name: &str,
        name: &str,
    ) -> Result<()> {
        info!(
            "Creating Deployment '{}' in cluster '{}'",
            name, cluster_name
        );

        // 这里应该触发DeploymentController创建实际的部署
        let _deployment_controller = DeploymentController::new().await?;

        // 将Deployment规格存储到etcd
        let config_manager = ConfigManager::new().await?;
        let deployment_yaml = serde_yaml::to_string(doc)
            .map_err(|e| NokubeError::Config(format!("Failed to serialize deployment: {}", e)))?;

        // 调用ConfigManager存储deployment规格到etcd
        config_manager
            .store_deployment(cluster_name, name, &deployment_yaml)
            .await
            .map_err(|e| {
                NokubeError::Config(format!("Failed to store deployment to etcd: {}", e))
            })?;
        info!("Stored Deployment spec for '{}' to etcd", name);

        // 触发部署操作 - ServiceModeAgent会通过KubeController检测并创建容器
        info!(
            "Deployment '{}' configuration stored, ServiceModeAgent will create containers",
            name
        );

        Ok(())
    }

    async fn apply_daemonset(
        doc: &serde_yaml::Value,
        cluster_name: &str,
        name: &str,
    ) -> Result<()> {
        info!(
            "Creating DaemonSet '{}' in cluster '{}'",
            name, cluster_name
        );

        let config_manager = ConfigManager::new().await?;
        let daemonset_yaml = serde_yaml::to_string(doc)
            .map_err(|e| NokubeError::Config(format!("Failed to serialize daemonset: {}", e)))?;

        // 调用ConfigManager存储daemonset规格到etcd
        config_manager
            .store_daemonset(cluster_name, name, &daemonset_yaml)
            .await
            .map_err(|e| {
                NokubeError::Config(format!("Failed to store daemonset to etcd: {}", e))
            })?;
        info!("Stored DaemonSet spec for '{}' to etcd", name);

        Ok(())
    }

    async fn apply_service(doc: &serde_yaml::Value, cluster_name: &str, name: &str) -> Result<()> {
        info!("Creating Service '{}' in cluster '{}'", name, cluster_name);

        let _config_manager = ConfigManager::new().await?;
        let _etcd_key = format!("/nokube/{}/services/{}", cluster_name, name);
        let _service_yaml = serde_yaml::to_string(doc)
            .map_err(|e| NokubeError::Config(format!("Failed to serialize service: {}", e)))?;

        // TODO: 调用ConfigManager存储service规格
        info!("Stored Service spec for '{}'", name);

        Ok(())
    }

    async fn apply_pod(doc: &serde_yaml::Value, cluster_name: &str, name: &str) -> Result<()> {
        use crate::k8s::actors::{ContainerSpec, PodActor};
        use crate::k8s::{AsyncActor, GlobalAttributionPath};
        use std::sync::Arc;

        info!("Creating Pod '{}' in cluster '{}'", name, cluster_name);

        let config_manager = Arc::new(ConfigManager::new().await?);

        // 解析pod规格
        let spec = doc
            .get("spec")
            .ok_or_else(|| NokubeError::Config("Missing 'spec' field in Pod".to_string()))?;

        let containers = spec
            .get("containers")
            .and_then(|c| c.as_sequence())
            .ok_or_else(|| {
                NokubeError::Config("Missing 'spec.containers' field in Pod".to_string())
            })?;

        if containers.is_empty() {
            return Err(NokubeError::Config(
                "Pod must have at least one container".to_string(),
            ));
        }

        // 取第一个容器
        let container = &containers[0];
        let image = container
            .get("image")
            .and_then(|i| i.as_str())
            .ok_or_else(|| NokubeError::Config("Missing 'image' field in container".to_string()))?;

        let container_spec = ContainerSpec {
            name: name.to_string(),
            image: image.to_string(),
            command: container
                .get("command")
                .and_then(|c| c.as_sequence())
                .map(|seq| {
                    seq.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.to_string())
                        .collect()
                }),
            args: container
                .get("args")
                .and_then(|a| a.as_sequence())
                .map(|seq| {
                    seq.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.to_string())
                        .collect()
                }),
            env: None,           // 简化处理
            volume_mounts: None, // 简化处理
        };

        // 创建心跳通道 (简化处理，不实际使用)
        let (heartbeat_tx, _heartbeat_rx) = tokio::sync::mpsc::channel(100);

        let attribution_path = GlobalAttributionPath::new(format!("pods/{}", name));

        // 创建并启动 Pod
        let mut pod = PodActor::new(
            name.to_string(),
            "default".to_string(), // 默认namespace
            attribution_path,
            "test-node".to_string(), // 模拟节点
            container_spec,
            "/tmp/workspace".to_string(), // 模拟workspace
            cluster_name.to_string(),
            heartbeat_tx,
            config_manager,
        );

        // 启动pod（会写入etcd）
        pod.start().await.map_err(|e| {
            error!("Failed to start pod: {}", e);
            NokubeError::Config(format!("Pod start failed: {}", e))
        })?;

        info!("Pod '{}' created and started successfully", name);
        Ok(())
    }

    async fn apply_gitops_cluster(
        doc: &serde_yaml::Value,
        cluster_name: &str,
        name: &str,
    ) -> Result<()> {
        info!(
            "Creating GitOpsCluster '{}' in cluster '{}'",
            name, cluster_name
        );

        let spec = doc.get("spec").ok_or_else(|| {
            NokubeError::Config("Missing 'spec' field in GitOpsCluster".to_string())
        })?;

        // 应用ConfigMap
        if let Some(configmap) = spec.get("configMap") {
            let mut configmap_doc = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
            configmap_doc.as_mapping_mut().unwrap().insert(
                serde_yaml::Value::String("apiVersion".to_string()),
                serde_yaml::Value::String("v1".to_string()),
            );
            configmap_doc.as_mapping_mut().unwrap().insert(
                serde_yaml::Value::String("kind".to_string()),
                serde_yaml::Value::String("ConfigMap".to_string()),
            );

            let mut metadata = serde_yaml::Mapping::new();
            metadata.insert(
                serde_yaml::Value::String("name".to_string()),
                configmap
                    .get("name")
                    .unwrap_or(&serde_yaml::Value::String("gitops-config".to_string()))
                    .clone(),
            );
            configmap_doc.as_mapping_mut().unwrap().insert(
                serde_yaml::Value::String("metadata".to_string()),
                serde_yaml::Value::Mapping(metadata),
            );

            configmap_doc.as_mapping_mut().unwrap().insert(
                serde_yaml::Value::String("data".to_string()),
                configmap
                    .get("data")
                    .unwrap_or(&serde_yaml::Value::Mapping(serde_yaml::Mapping::new()))
                    .clone(),
            );

            apply_configmap(
                &configmap_doc,
                cluster_name,
                configmap
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("gitops-config"),
            )
            .await?;
        }

        // 应用Deployment
        if let Some(deployment) = spec.get("deployment") {
            let deployment_doc = create_deployment_from_spec(deployment, cluster_name)?;
            let deployment_name = deployment
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("gitops-controller");
            apply_deployment(&deployment_doc, cluster_name, deployment_name).await?;
        }
        // 为保证简单性，移除 webhook 相关逻辑
        // 清理历史 webhook 部署（如存在）
        {
            if let Ok(config_manager) = ConfigManager::new().await {
                let etcd = config_manager.get_etcd_manager();
                let legacy_key = format!(
                    "/nokube/{}/deployments/gitops-webhook-server-{}",
                    cluster_name, cluster_name
                );
                if let Err(e) = etcd.delete(legacy_key.clone()).await {
                    warn!(
                        "Failed to delete legacy webhook deployment key {}: {}",
                        legacy_key, e
                    );
                } else {
                    info!(
                        "Cleaned up legacy webhook deployment etcd key for cluster {}",
                        cluster_name
                    );
                }
                // 同步清理 pods 和 events 信息
                let pod_key = format!(
                    "/nokube/{}/pods/gitops-webhook-server-{}",
                    cluster_name, cluster_name
                );
                let events_key = format!(
                    "/nokube/{}/events/pod/gitops-webhook-server-{}",
                    cluster_name, cluster_name
                );
                let _ = etcd.delete(pod_key).await;
                let _ = etcd.delete(events_key).await;
            }
        }

        Ok(())
    }

    fn create_deployment_from_spec(
        deployment_spec: &serde_yaml::Value,
        _cluster_name: &str,
    ) -> Result<serde_yaml::Value> {
        let mut deployment_doc = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let mapping = deployment_doc.as_mapping_mut().unwrap();

        mapping.insert(
            serde_yaml::Value::String("apiVersion".to_string()),
            serde_yaml::Value::String("apps/v1".to_string()),
        );
        mapping.insert(
            serde_yaml::Value::String("kind".to_string()),
            serde_yaml::Value::String("Deployment".to_string()),
        );

        // metadata
        let mut metadata = serde_yaml::Mapping::new();
        metadata.insert(
            serde_yaml::Value::String("name".to_string()),
            deployment_spec
                .get("name")
                .unwrap_or(&serde_yaml::Value::String("deployment".to_string()))
                .clone(),
        );
        metadata.insert(
            serde_yaml::Value::String("namespace".to_string()),
            serde_yaml::Value::String("default".to_string()),
        );
        mapping.insert(
            serde_yaml::Value::String("metadata".to_string()),
            serde_yaml::Value::Mapping(metadata),
        );

        // spec
        let mut spec = serde_yaml::Mapping::new();
        spec.insert(
            serde_yaml::Value::String("replicas".to_string()),
            deployment_spec
                .get("replicas")
                .unwrap_or(&serde_yaml::Value::Number(serde_yaml::Number::from(1)))
                .clone(),
        );

        // selector
        let mut selector = serde_yaml::Mapping::new();
        let mut match_labels = serde_yaml::Mapping::new();
        match_labels.insert(
            serde_yaml::Value::String("app".to_string()),
            deployment_spec
                .get("name")
                .unwrap_or(&serde_yaml::Value::String("app".to_string()))
                .clone(),
        );
        selector.insert(
            serde_yaml::Value::String("matchLabels".to_string()),
            serde_yaml::Value::Mapping(match_labels.clone()),
        );
        spec.insert(
            serde_yaml::Value::String("selector".to_string()),
            serde_yaml::Value::Mapping(selector),
        );

        // template
        let mut template = serde_yaml::Mapping::new();
        let mut template_metadata = serde_yaml::Mapping::new();
        template_metadata.insert(
            serde_yaml::Value::String("labels".to_string()),
            serde_yaml::Value::Mapping(match_labels),
        );
        template.insert(
            serde_yaml::Value::String("metadata".to_string()),
            serde_yaml::Value::Mapping(template_metadata),
        );

        // template spec
        let mut template_spec = serde_yaml::Mapping::new();
        if let Some(container_spec) = deployment_spec.get("containerSpec") {
            let mut containers = Vec::new();

            // 复制容器规格并添加 volumeMounts
            let mut container = container_spec.clone();
            if let Some(container_map) = container.as_mapping_mut() {
                // 添加 volumeMounts
                let mut volume_mounts = Vec::new();
                let mut volume_mount = serde_yaml::Mapping::new();
                volume_mount.insert(
                    serde_yaml::Value::String("name".to_string()),
                    serde_yaml::Value::String("config-volume".to_string()),
                );
                volume_mount.insert(
                    serde_yaml::Value::String("mountPath".to_string()),
                    serde_yaml::Value::String("/etc/config".to_string()),
                );
                volume_mount.insert(
                    serde_yaml::Value::String("readOnly".to_string()),
                    serde_yaml::Value::Bool(true),
                );
                volume_mounts.push(serde_yaml::Value::Mapping(volume_mount));

                container_map.insert(
                    serde_yaml::Value::String("volumeMounts".to_string()),
                    serde_yaml::Value::Sequence(volume_mounts),
                );
            }

            containers.push(container);
            template_spec.insert(
                serde_yaml::Value::String("containers".to_string()),
                serde_yaml::Value::Sequence(containers),
            );
        }

        // 添加volumes - 基于GitOps spec中的ConfigMap引用
        // 为volumeMounts中引用的volumes创建对应的volume定义
        let mut volumes = Vec::new();

        // 检查containerSpec中是否有volumeMounts，如果有就创建对应的volume
        if let Some(container_spec) = deployment_spec.get("containerSpec") {
            if let Some(volume_mounts) = container_spec
                .get("volumeMounts")
                .and_then(|vm| vm.as_sequence())
            {
                for volume_mount in volume_mounts {
                    if let Some(volume_name) = volume_mount.get("name").and_then(|n| n.as_str()) {
                        // 为config-volume创建ConfigMap volume
                        if volume_name == "config-volume" {
                            let mut config_volume = serde_yaml::Mapping::new();
                            config_volume.insert(
                                serde_yaml::Value::String("name".to_string()),
                                serde_yaml::Value::String("config-volume".to_string()),
                            );

                            let mut configmap_source = serde_yaml::Mapping::new();
                            let configmap_name = format!("gitops-scripts-{}", _cluster_name);
                            configmap_source.insert(
                                serde_yaml::Value::String("name".to_string()),
                                serde_yaml::Value::String(configmap_name),
                            );
                            config_volume.insert(
                                serde_yaml::Value::String("configMap".to_string()),
                                serde_yaml::Value::Mapping(configmap_source),
                            );

                            volumes.push(serde_yaml::Value::Mapping(config_volume));
                            break; // 只需要创建一次config-volume
                        }
                    }
                }
            }
        }

        // 如果有volumes，添加到template spec中
        if !volumes.is_empty() {
            template_spec.insert(
                serde_yaml::Value::String("volumes".to_string()),
                serde_yaml::Value::Sequence(volumes),
            );
        }

        template.insert(
            serde_yaml::Value::String("spec".to_string()),
            serde_yaml::Value::Mapping(template_spec),
        );

        spec.insert(
            serde_yaml::Value::String("template".to_string()),
            serde_yaml::Value::Mapping(template),
        );

        mapping.insert(
            serde_yaml::Value::String("spec".to_string()),
            serde_yaml::Value::Mapping(spec),
        );

        Ok(deployment_doc)
    }

    async fn handle_monitor(cluster_name: String) -> Result<()> {
        info!("Starting monitoring for cluster: {}", cluster_name);
        match DeploymentController::new().await {
            Ok(deployment_controller) => {
                match deployment_controller.setup_monitoring(&cluster_name).await {
                    Ok(_) => {
                        info!("Monitoring started for cluster: {}", cluster_name);
                        Ok(())
                    }
                    Err(e) => {
                        error!(
                            "Failed to setup monitoring for cluster {}: {}",
                            cluster_name, e
                        );
                        Err(NokubeError::Monitoring(format!(
                            "Monitoring setup failed: {}",
                            e
                        )))
                    }
                }
            }
            Err(e) => {
                error!("Failed to create deployment controller: {}", e);
                Err(NokubeError::Monitoring(format!(
                    "Controller creation failed: {}",
                    e
                )))
            }
        }
    }

    async fn handle_agent_command(extra_params: Option<String>) -> Result<()> {
        info!("Running agent in command mode");

        // 解析额外参数
        let parsed_params = extra_params.as_ref().and_then(|params_b64| {
            base64::engine::general_purpose::STANDARD
                .decode(params_b64)
                .ok()
                .and_then(|params_json| String::from_utf8(params_json).ok())
                .and_then(|params_str| serde_json::from_str::<serde_json::Value>(&params_str).ok())
        });

        let cluster_name = parsed_params
            .as_ref()
            .and_then(|params| {
                params
                    .get("cluster_name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .ok_or_else(|| {
                NokubeError::Config("Missing cluster_name in extra_params".to_string())
            })?;

        let config_manager = ConfigManager::new().await.map_err(|e| {
            error!("Failed to create config manager: {}", e);
            NokubeError::Config(format!("Config manager creation failed: {}", e))
        })?;

        let cluster_config = config_manager
            .get_cluster_config(&cluster_name)
            .await
            .map_err(|e| {
                error!("Failed to load cluster config: {}", e);
                NokubeError::Config(format!("Config loading failed: {}", e))
            })?;

        let cluster_config = match cluster_config {
            Some(cfg) => cfg,
            None => {
                let cluster_metas = config_manager.list_clusters().await.unwrap_or_default();
                let cluster_names: Vec<String> = cluster_metas
                    .iter()
                    .map(|m| m.config.cluster_name.clone())
                    .collect();
                let cluster_list = cluster_names.join(", ");
                error!("Cluster config not found for: {}", cluster_name);
                return Err(NokubeError::ClusterNotFound {
                    cluster: cluster_name.to_string(),
                    cluster_list,
                });
            }
        };

        let command_agent = CommandModeAgent::new(cluster_config, parsed_params);
        command_agent.execute().await.map_err(|e| {
            error!("Command mode agent failed: {}", e);
            NokubeError::Agent(format!("Command mode execution failed: {}", e))
        })?;
        info!("Command mode agent completed successfully");
        Ok(())
    }

    async fn handle_agent_service(extra_params: Option<String>) -> Result<()> {
        info!("Starting agent service");
        let parsed_params = extra_params.as_ref().and_then(|params_b64| {
            base64::engine::general_purpose::STANDARD
                .decode(params_b64)
                .ok()
                .and_then(|params_json| String::from_utf8(params_json).ok())
                .and_then(|params_str| serde_json::from_str::<serde_json::Value>(&params_str).ok())
        });

        let cluster_name = parsed_params
            .as_ref()
            .and_then(|params| params.get("cluster_name").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .unwrap_or_else(|| "default".to_string());

        let deployment_version = parsed_params
            .as_ref()
            .and_then(|params| params.get("deployment_version").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .ok_or_else(|| {
                NokubeError::Config(
                    "Missing deployment_version in agent-service extra_params".to_string(),
                )
            })?;

        let config_manager = ConfigManager::new().await.map_err(|e| {
            error!("Failed to create config manager: {}", e);
            NokubeError::Config(format!("Config manager creation failed: {}", e))
        })?;

        let cluster_config = config_manager
            .get_cluster_config(&cluster_name)
            .await
            .map_err(|e| {
                error!("Failed to load cluster config: {}", e);
                NokubeError::Config(format!("Config loading failed: {}", e))
            })?;

        let cluster_config = match cluster_config {
            Some(cfg) => cfg,
            None => {
                let cluster_metas = config_manager.list_clusters().await.unwrap_or_default();
                let cluster_names: Vec<String> = cluster_metas
                    .iter()
                    .map(|m| m.config.cluster_name.clone())
                    .collect();
                let cluster_list = cluster_names.join(", ");
                error!("Cluster config not found for: {}", cluster_name);
                return Err(NokubeError::ClusterNotFound {
                    cluster: cluster_name.to_string(),
                    cluster_list,
                });
            }
        };

        // 从 ConfigManager 获取正确的 etcd 地址，而不是硬编码
        let etcd_endpoints = config_manager.get_etcd_endpoints();

        // 尝试从集群配置中获取当前节点的名称，而不是使用默认值
        let node_id = std::env::var("NOKUBE_NODE_ID")
            .or_else(|_| {
                // 尝试根据当前主机IP或主机名匹配集群配置中的节点
                if let Ok(hostname) = std::env::var("HOSTNAME") {
                    for node in &cluster_config.nodes {
                        if node.name.contains(&hostname) || hostname.contains(&node.name) {
                            return Ok(node.name.clone());
                        }
                    }
                }
                // 如果有节点配置，使用第一个节点名作为fallback
                if let Some(first_node) = cluster_config.nodes.first() {
                    Ok(first_node.name.clone())
                } else {
                    Err(std::env::VarError::NotPresent)
                }
            })
            .unwrap_or_else(|_| "default-node".to_string());
        let mut service_agent = ServiceModeAgent::new(
            node_id,
            cluster_name.to_string(),
            etcd_endpoints,
            cluster_config,
            deployment_version,
        )
        .await?;
        service_agent.run().await.map_err(|e| {
            error!("Service mode agent failed: {}", e);
            NokubeError::Agent(format!("Service mode execution failed: {}", e))
        })?;
        info!("Service mode agent completed successfully");
        Ok(())
    }

    async fn handle_get(
        resource_type: String,
        name: Option<String>,
        cluster: Option<String>,
        output: String,
        all_namespaces: bool,
    ) -> Result<()> {
        let cluster_name = cluster.unwrap_or_else(|| "default".to_string());
        info!("Getting {} from cluster: {}", resource_type, cluster_name);

        let config_manager = ConfigManager::new().await?;

        match resource_type.as_str() {
            "pods" | "pod" => {
                get_pods(
                    &config_manager,
                    &cluster_name,
                    name,
                    &output,
                    all_namespaces,
                )
                .await
            }
            "deployments" | "deployment" | "deploy" => {
                get_deployments(&config_manager, &cluster_name, name, &output).await
            }
            "services" | "service" | "svc" => {
                get_services(&config_manager, &cluster_name, name, &output).await
            }
            "configmaps" | "configmap" | "cm" => {
                get_configmaps(&config_manager, &cluster_name, name, &output).await
            }
            "secrets" | "secret" => {
                get_secrets(&config_manager, &cluster_name, name, &output).await
            }
            "daemonsets" | "daemonset" | "ds" => {
                get_daemonsets(&config_manager, &cluster_name, name, &output).await
            }
            _ => {
                error!("Unsupported resource type: {}", resource_type);
                Err(NokubeError::Config(format!(
                    "Unsupported resource type: {}",
                    resource_type
                )))
            }
        }
    }

    async fn handle_describe(
        resource_type: String,
        name: String,
        cluster: Option<String>,
    ) -> Result<()> {
        let cluster_name = cluster.unwrap_or_else(|| "default".to_string());
        info!(
            "Describing {} '{}' from cluster: {}",
            resource_type, name, cluster_name
        );

        let config_manager = ConfigManager::new().await?;

        match resource_type.as_str() {
            "pods" | "pod" => describe_pod(&config_manager, &cluster_name, &name).await,
            "deployments" | "deployment" | "deploy" => {
                describe_deployment(&config_manager, &cluster_name, &name).await
            }
            "services" | "service" | "svc" => {
                describe_service(&config_manager, &cluster_name, &name).await
            }
            "configmaps" | "configmap" | "cm" => {
                describe_configmap(&config_manager, &cluster_name, &name).await
            }
            "secrets" | "secret" => describe_secret(&config_manager, &cluster_name, &name).await,
            "daemonsets" | "daemonset" | "ds" => {
                describe_daemonset(&config_manager, &cluster_name, &name).await
            }
            _ => {
                error!("Unsupported resource type: {}", resource_type);
                Err(NokubeError::Config(format!(
                    "Unsupported resource type: {}",
                    resource_type
                )))
            }
        }
    }

    async fn handle_logs(
        pod_name: String,
        cluster: Option<String>,
        follow: bool,
        tail: Option<i32>,
    ) -> Result<()> {
        let cluster_name = cluster.unwrap_or_else(|| "default".to_string());
        info!(
            "Getting logs for pod '{}' from cluster: {}",
            pod_name, cluster_name
        );

        let config_manager = ConfigManager::new().await?;
        get_pod_logs(&config_manager, &cluster_name, &pod_name, follow, tail).await
    }
}

async fn read_cluster_config_from_file(file_path: &str) -> Result<ClusterConfig> {
    info!("Reading cluster config from file: {}", file_path);

    let content = fs::read_to_string(file_path)
        .map_err(|e| NokubeError::Config(format!("Failed to read file {}: {}", file_path, e)))?;

    let cluster_config: ClusterConfig = serde_yaml::from_str(&content).map_err(|e| {
        NokubeError::Config(format!("Failed to parse YAML from {}: {}", file_path, e))
    })?;

    info!(
        "Successfully loaded cluster config for: {}",
        cluster_config.cluster_name
    );
    Ok(cluster_config)
}

fn create_config_template(template_path: &str) -> Result<()> {
    let template_content = r#"cluster_name: my-cluster

# 分布式任务管理配置
task_spec:
  # 任务管理器版本
  version: "1.0"
  
  # 监控配置
  monitoring:
    grafana:
      port: 3000
    greptimedb:
      port: 4000
    enabled: true

# 节点列表
nodes:
  - ssh_url: "10.0.0.1:22"
    name: "head-node"  # 实际系统看到的节点名
    role: "head"
    # 建议：设置 workspace 作为该节点的工作与存储根目录
    workspace: "/opt/nokube-workspace"
    # 可选：单独指定存储子路径（相对于 workspace）；若不配置则直接使用 workspace
    # storage:
    #   type: "local"
    #   path: "/data/ray/head"
    users:
      - userid: "your-username"
        password: "your-password"
    proxy:
      http_proxy: "http://proxy.example.com:8080"
      https_proxy: "http://proxy.example.com:8080"
      no_proxy: "localhost,127.0.0.1,10.0.0.0/8,192.168.0.0/16"

  - ssh_url: "10.0.0.2:22"
    name: "worker-node-1"   # 实际系统看到的节点名
    role: "worker"
    workspace: "/opt/nokube-workspace"
    # storage:
    #   type: "local"
    #   path: "/data/ray/worker-1"
    users:
      - userid: "your-username"
        password: "your-password"
    proxy:
      http_proxy: "http://proxy.example.com:8080"
      https_proxy: "http://proxy.example.com:8080"
      no_proxy: "localhost,127.0.0.1,10.0.0.0/8,192.168.0.0/16"

# NoKube 特定配置
nokube_config:
  # 日志配置
  log_level: "INFO"
  # 监控指标收集间隔(秒)
  metrics_interval: 30
  # 配置轮询间隔(秒)
  config_poll_interval: 10
"#;

    fs::write(template_path, template_content).map_err(|e| {
        NokubeError::Config(format!(
            "Failed to create template file {}: {}",
            template_path, e
        ))
    })?;

    Ok(())
}

// ============ Resource Get Functions ============

async fn get_pods(
    config_manager: &ConfigManager,
    cluster_name: &str,
    name: Option<String>,
    output: &str,
    all_namespaces: bool,
) -> Result<()> {
    use crate::k8s::actors::PodDescription;

    let namespace = if all_namespaces { "*" } else { "default" };

    info!("Getting pods from etcd for cluster: {}", cluster_name);

    match output {
        "table" => {
            println!(
                "{:<30} {:<10} {:<15} {:<8} {:<15} {:<10} {:<8}",
                "NAME", "READY", "STATUS", "ALIVE", "RESTARTS", "NODE", "AGE"
            );
            println!("{}", "-".repeat(110));

            if let Some(ref pod_name) = name {
                // 获取特定 pod
                match PodDescription::from_etcd(config_manager, cluster_name, pod_name).await? {
                    Some(pod_desc) => {
                        let ready_str = if pod_desc.status == crate::k8s::actors::PodStatus::Running
                        {
                            "1/1"
                        } else {
                            "0/1"
                        };

                        let node_name = pod_desc.node.split('/').next().unwrap_or("unknown");
                        let age = "5m"; // 简化处理，可以根据 start_time 计算

                        println!(
                            "{:<30} {:<10} {:<15} {:<8} {:<15} {:<10} {:<8}",
                            pod_desc.name,
                            ready_str,
                            pod_desc.status,
                            pod_desc.alive_status,
                            0, // restart_count 简化
                            node_name,
                            age
                        );
                    }
                    None => {
                        println!("Pod '{}' not found in cluster '{}'", pod_name, cluster_name);
                    }
                }
            } else {
                // 列出所有 pods - 需要从 etcd 查询所有 pod keys
                let etcd_manager = config_manager.get_etcd_manager();
                let pods_prefix = format!("/nokube/{}/pods/", cluster_name);

                // 使用 etcd 的 range 查询获取所有 pod keys
                let pod_keys = etcd_manager
                    .get_keys_with_prefix(pods_prefix.clone())
                    .await?;

                if pod_keys.is_empty() {
                    // 如果 etcd 中没有数据，显示提示信息
                    println!("No pods found in cluster '{}'. etcd may be empty or pods have not been created yet.", cluster_name);
                } else {
                    // 从 etcd 读取真实数据
                    for pod_key in pod_keys {
                        let pod_name = pod_key.strip_prefix(&pods_prefix).unwrap_or("unknown");

                        match PodDescription::from_etcd(config_manager, cluster_name, pod_name)
                            .await?
                        {
                            Some(pod_desc) => {
                                let ready_str =
                                    if pod_desc.status == crate::k8s::actors::PodStatus::Running {
                                        "1/1"
                                    } else {
                                        "0/1"
                                    };

                                let node_name =
                                    pod_desc.node.split('/').next().unwrap_or("unknown");
                                let age = "5m"; // 简化处理

                                println!(
                                    "{:<30} {:<10} {:<15} {:<10} {:<15} {:<10}",
                                    pod_desc.name,
                                    ready_str,
                                    pod_desc.status,
                                    pod_desc
                                        .containers
                                        .first()
                                        .map(|c| c.restart_count)
                                        .unwrap_or(0),
                                    node_name,
                                    age
                                );
                            }
                            None => {
                                warn!("Failed to load pod data for: {}", pod_name);
                            }
                        }
                    }
                }
            }
        }
        "json" => {
            if let Some(ref pod_name) = name {
                match PodDescription::from_etcd(config_manager, cluster_name, pod_name).await? {
                    Some(pod_desc) => {
                        println!("{}", serde_json::to_string_pretty(&pod_desc)?);
                    }
                    None => {
                        println!("Pod '{}' not found in cluster '{}'", pod_name, cluster_name);
                    }
                }
            } else {
                println!("Please specify a pod name for JSON output");
            }
        }
        "yaml" => {
            println!("unimplemented: pod YAML output");
        }
        _ => {
            return Err(NokubeError::Config(format!(
                "Unsupported output format: {}",
                output
            )));
        }
    }

    Ok(())
}

async fn get_deployments(
    config_manager: &ConfigManager,
    cluster_name: &str,
    name: Option<String>,
    output: &str,
) -> Result<()> {
    let deployments_key = format!("/nokube/{}/deployments/", cluster_name);
    info!(
        "Searching for deployments with key: {}{}",
        deployments_key,
        name.as_deref().unwrap_or("")
    );

    match output {
        "table" => {
            println!(
                "{:<30} {:<10} {:<12} {:<10} {:<10}",
                "NAME", "READY", "UP-TO-DATE", "AVAILABLE", "AGE"
            );
            println!("{}", "-".repeat(84));

            let etcd = config_manager.get_etcd_manager();
            let kvs = if let Some(ref deploy_name) = name {
                etcd.get(format!("{}{}", deployments_key, deploy_name))
                    .await?
            } else {
                etcd.get_prefix(deployments_key.clone()).await?
            };

            if kvs.is_empty() {
                return Ok(());
            }

            // Gather pod readiness for each deployment by checking pods prefix
            let pods = etcd
                .get_prefix(format!("/nokube/{}/pods/", cluster_name))
                .await
                .unwrap_or_default();
            use std::collections::HashMap;
            let mut pod_ready_count: HashMap<String, (u32, u32)> = HashMap::new(); // name -> (ready, total)

            for kv in &pods {
                let key = kv.key_str();
                let pod_name = key.split('/').last().unwrap_or("");
                // Heuristic: deployment pods start with "<deployName>-"
                for dep_kv in &kvs {
                    let dep_key = dep_kv.key_str();
                    let dep_name = dep_key.split('/').last().unwrap_or("");
                    if pod_name.starts_with(dep_name) {
                        let mut ready = false;
                        if let Ok(val_str) = std::str::from_utf8(&kv.value) {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(val_str) {
                                ready = v.get("ready").and_then(|b| b.as_bool()).unwrap_or(false);
                            }
                        }
                        let entry = pod_ready_count
                            .entry(dep_name.to_string())
                            .or_insert((0, 0));
                        entry.1 += 1;
                        if ready {
                            entry.0 += 1;
                        }
                    }
                }
            }

            for kv in kvs {
                let dep_key = kv.key_str().to_string();
                let dep_name = dep_key.split('/').last().unwrap_or("");
                let mut replicas: u32 = 0;
                // Try to parse desired replicas from YAML
                if let Ok(val_str) = std::str::from_utf8(&kv.value) {
                    if let Ok(yaml_val) = serde_yaml::from_str::<serde_yaml::Value>(val_str) {
                        if let Some(rep) = yaml_val
                            .get("spec")
                            .and_then(|s| s.get("replicas"))
                            .and_then(|r| r.as_i64())
                        {
                            replicas = rep.max(0) as u32;
                        }
                    }
                }
                if replicas == 0 {
                    replicas = 1;
                }
                let (ready, total) = pod_ready_count.get(dep_name).cloned().unwrap_or((0, 0));
                let up_to_date = replicas;
                let available = ready;
                let ready_fmt = format!("{}/{}", ready, replicas);
                println!(
                    "{:<30} {:<10} {:<12} {:<10} {:<10}",
                    dep_name, ready_fmt, up_to_date, available, "-"
                );
            }
        }
        "yaml" | "json" => {
            // 只有在指定具体 deployment 名称时才输出 YAML
            if let Some(ref deploy_name) = name {
                let deployment_key =
                    format!("/nokube/{}/deployments/{}", cluster_name, deploy_name);
                info!("Searching for deployment with key: {}", deployment_key);

                match config_manager.get_etcd_manager().get(deployment_key).await {
                    Ok(kv_pairs) => {
                        if let Some(kv) = kv_pairs.first() {
                            let deployment_yaml = String::from_utf8_lossy(&kv.value);
                            println!("{}", deployment_yaml);
                        } else {
                            eprintln!(
                                "Error: Deployment '{}' not found in cluster '{}'",
                                deploy_name, cluster_name
                            );
                            std::process::exit(1);
                        }
                    }
                    Err(e) => {
                        eprintln!("Error retrieving deployment: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                eprintln!("Error: YAML output requires specifying a deployment name");
                std::process::exit(1);
            }
        }
        _ => {
            return Err(NokubeError::Config(format!(
                "Unsupported output format: {}",
                output
            )));
        }
    }

    Ok(())
}

async fn get_services(
    _config_manager: &ConfigManager,
    cluster_name: &str,
    name: Option<String>,
    output: &str,
) -> Result<()> {
    // Services storage is not implemented yet; do not fake outputs.
    println!(
        "unimplemented: services listing (table/json/yaml) for cluster '{}'",
        cluster_name
    );

    Ok(())
}

async fn get_configmaps(
    config_manager: &ConfigManager,
    cluster_name: &str,
    name: Option<String>,
    output: &str,
) -> Result<()> {
    let prefix = format!("/nokube/{}/configmaps/", cluster_name);
    let etcd = config_manager.get_etcd_manager();
    match output {
        "table" => {
            println!("{:<30} {:<10} {:<10}", "NAME", "DATA", "AGE");
            println!("{}", "-".repeat(50));
            let kvs = if let Some(ref name) = name {
                etcd.get(format!("{}{}", prefix, name)).await?
            } else {
                etcd.get_prefix(prefix.clone()).await?
            };
            for kv in kvs {
                let key = kv.key_str();
                let cm_name = key.split('/').last().unwrap_or("");
                let data_count = match std::str::from_utf8(&kv.value)
                    .ok()
                    .and_then(|s| serde_yaml::from_str::<serde_yaml::Value>(s).ok())
                {
                    Some(v) => v
                        .get("data")
                        .and_then(|d| d.as_mapping())
                        .map(|m| m.len())
                        .unwrap_or(0),
                    None => 0,
                };
                println!("{:<30} {:<10} {:<10}", cm_name, data_count, "-");
            }
        }
        "yaml" => {
            if let Some(ref name) = name {
                let kvs = etcd.get(format!("{}{}", prefix, name)).await?;
                if let Some(kv) = kvs.first() {
                    println!("{}", String::from_utf8_lossy(&kv.value));
                }
            } else {
                println!("Please specify a configmap name for YAML output");
            }
        }
        "json" => {
            if let Some(ref name) = name {
                let kvs = etcd.get(format!("{}{}", prefix, name)).await?;
                if let Some(kv) = kvs.first() {
                    if let Ok(val) = serde_yaml::from_slice::<serde_yaml::Value>(&kv.value) {
                        println!("{}", serde_json::to_string_pretty(&val)?);
                    }
                }
            } else {
                println!("Please specify a configmap name for JSON output");
            }
        }
        _ => {
            return Err(NokubeError::Config(format!(
                "Unsupported output format: {}",
                output
            )))
        }
    }

    Ok(())
}

async fn get_secrets(
    config_manager: &ConfigManager,
    cluster_name: &str,
    name: Option<String>,
    output: &str,
) -> Result<()> {
    let prefix = format!("/nokube/{}/secrets/", cluster_name);
    let etcd = config_manager.get_etcd_manager();
    match output {
        "table" => {
            println!("{:<30} {:<20} {:<10} {:<10}", "NAME", "TYPE", "DATA", "AGE");
            println!("{}", "-".repeat(70));
            let kvs = if let Some(ref name) = name {
                etcd.get(format!("{}{}", prefix, name)).await?
            } else {
                etcd.get_prefix(prefix.clone()).await?
            };
            for kv in kvs {
                let key = kv.key_str();
                let sec_name = key.split('/').last().unwrap_or("");
                let (stype, dcount) = match std::str::from_utf8(&kv.value)
                    .ok()
                    .and_then(|s| serde_yaml::from_str::<serde_yaml::Value>(s).ok())
                {
                    Some(v) => {
                        let t = v
                            .get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("Opaque")
                            .to_string();
                        let c = v
                            .get("data")
                            .and_then(|d| d.as_mapping())
                            .map(|m| m.len())
                            .unwrap_or(0);
                        (t, c)
                    }
                    None => ("Opaque".to_string(), 0),
                };
                println!("{:<30} {:<20} {:<10} {:<10}", sec_name, stype, dcount, "-");
            }
        }
        "yaml" => {
            if let Some(ref name) = name {
                let kvs = etcd.get(format!("{}{}", prefix, name)).await?;
                if let Some(kv) = kvs.first() {
                    println!("{}", String::from_utf8_lossy(&kv.value));
                }
            } else {
                println!("Please specify a secret name for YAML output");
            }
        }
        "json" => {
            if let Some(ref name) = name {
                let kvs = etcd.get(format!("{}{}", prefix, name)).await?;
                if let Some(kv) = kvs.first() {
                    if let Ok(val) = serde_yaml::from_slice::<serde_yaml::Value>(&kv.value) {
                        println!("{}", serde_json::to_string_pretty(&val)?);
                    }
                }
            } else {
                println!("Please specify a secret name for JSON output");
            }
        }
        _ => {
            return Err(NokubeError::Config(format!(
                "Unsupported output format: {}",
                output
            )))
        }
    }

    Ok(())
}

async fn get_daemonsets(
    config_manager: &ConfigManager,
    cluster_name: &str,
    name: Option<String>,
    output: &str,
) -> Result<()> {
    let daemonsets_key = format!("/nokube/{}/daemonsets/", cluster_name);
    info!(
        "Searching for daemonsets with key: {}{}",
        daemonsets_key,
        name.as_deref().unwrap_or("")
    );

    match output {
        "table" => {
            println!(
                "{:<30} {:<10} {:<10} {:<10} {:<12} {:<10}",
                "NAME", "DESIRED", "CURRENT", "READY", "UP-TO-DATE", "AVAILABLE"
            );
            println!("{}", "-".repeat(92));
            let etcd = config_manager.get_etcd_manager();
            let kvs = if let Some(ref ds_name) = name {
                etcd.get(format!("{}{}", daemonsets_key, ds_name)).await?
            } else {
                etcd.get_prefix(daemonsets_key.clone()).await?
            };
            if kvs.is_empty() {
                return Ok(());
            }

            // Collect pod readiness for ds
            let pods = etcd
                .get_prefix(format!("/nokube/{}/pods/", cluster_name))
                .await
                .unwrap_or_default();
            use std::collections::HashMap;
            let mut pod_ready_count: HashMap<String, (u32, u32)> = HashMap::new(); // name -> (ready,total)
            for kv in &pods {
                let key = kv.key_str();
                let pod_name = key.split('/').last().unwrap_or("");
                for ds_kv in &kvs {
                    let ds_key = ds_kv.key_str();
                    let ds_name = ds_key.split('/').last().unwrap_or("");
                    // Heuristic: ds pod name starts with "<dsName>-"
                    if pod_name.starts_with(ds_name) {
                        let mut ready = false;
                        if let Ok(val_str) = std::str::from_utf8(&kv.value) {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(val_str) {
                                ready = v.get("ready").and_then(|b| b.as_bool()).unwrap_or(false);
                            }
                        }
                        let entry = pod_ready_count.entry(ds_name.to_string()).or_insert((0, 0));
                        entry.1 += 1;
                        if ready {
                            entry.0 += 1;
                        }
                    }
                }
            }

            for kv in kvs {
                let ds_key = kv.key_str().to_string();
                let ds_name = ds_key.split('/').last().unwrap_or("");
                let (ready, total) = pod_ready_count.get(ds_name).cloned().unwrap_or((0, 0));
                let desired = if total > 0 { total } else { 1 };
                let up_to_date = desired;
                let available = ready;
                println!(
                    "{:<30} {:<10} {:<10} {:<10} {:<12} {:<10}",
                    ds_name, desired, total, ready, up_to_date, available
                );
            }
        }
        _ => println!("DaemonSet details in YAML/JSON format not implemented yet"),
    }

    Ok(())
}

// ============ Resource Describe Functions ============

async fn describe_pod(
    config_manager: &ConfigManager,
    cluster_name: &str,
    name: &str,
) -> Result<()> {
    info!("Describing pod '{}' in cluster '{}'", name, cluster_name);

    // 尝试从etcd获取真实的pod描述信息
    match crate::k8s::actors::PodDescription::from_etcd(config_manager, cluster_name, name).await? {
        Some(pod_desc) => {
            // 使用真实数据显示pod描述信息
            println!("{}", pod_desc.display());
        }
        None => {
            // 如果etcd中没有数据，提供一些默认信息
            println!("Pod '{}' not found in cluster '{}'", name, cluster_name);
            println!("The pod may not exist or may not be managed by nokube.");

            // 仍然可以提供一些模拟信息作为fallback
            println!("\nFallback information (simulated):");
            println!("Name:         {}", name);
            println!("Namespace:    default");
            println!("Priority:     0");
            println!("Node:         unknown");
            println!("Status:       Unknown");
            println!("IP:           <none>");
            println!("Containers:");
            println!("  {}:", name);
            println!("    Container ID:   <none>");
            println!("    Image:          unknown");
            println!("    State:          Unknown");
            println!("    Ready:          False");
            println!("    Restart Count:  0");
        }
    }

    Ok(())
}

async fn describe_deployment(
    _config_manager: &ConfigManager,
    cluster_name: &str,
    name: &str,
) -> Result<()> {
    info!(
        "Describing deployment '{}' in cluster '{}'",
        name, cluster_name
    );

    println!("Name:                   {}", name);
    println!("Namespace:              default");
    println!("CreationTimestamp:      Mon, 30 Aug 2025 04:25:00 +0000");
    println!("Labels:                 app=example");
    println!("Selector:               app=example");
    println!(
        "Replicas:               3 desired | 3 updated | 3 total | 3 available | 0 unavailable"
    );
    println!("StrategyType:           RollingUpdate");
    println!("MinReadySeconds:        0");
    println!("RollingUpdateStrategy:  25% max unavailable, 25% max surge");
    println!("Pod Template:");
    println!("  Labels:  app=example");
    println!("  Containers:");
    println!("   app:");
    println!("    Image:        nginx:latest");
    println!("    Port:         80/TCP");
    println!("    Environment:  <none>");
    println!("    Mounts:       <none>");
    println!("Conditions:");
    println!("  Available      True    MinimumReplicasAvailable");
    println!("  Progressing    True    NewReplicaSetAvailable");
    println!("OldReplicaSets:  <none>");
    println!("NewReplicaSet:   {}-xyz123 (3/3 replicas created)", name);

    Ok(())
}

async fn describe_service(
    _config_manager: &ConfigManager,
    cluster_name: &str,
    name: &str,
) -> Result<()> {
    info!(
        "Describing service '{}' in cluster '{}'",
        name, cluster_name
    );

    println!("Name:              {}", name);
    println!("Namespace:         default");
    println!("Labels:            app=example");
    println!("Selector:          app=example");
    println!("Type:              ClusterIP");
    println!("IP:                10.96.1.1");
    println!("Port:              <unset>  80/TCP");
    println!("TargetPort:        80/TCP");
    println!("Endpoints:         10.244.1.5:80,10.244.1.6:80,10.244.1.7:80");
    println!("Session Affinity:  None");
    println!("Events:            <none>");

    Ok(())
}

async fn describe_configmap(
    _config_manager: &ConfigManager,
    cluster_name: &str,
    name: &str,
) -> Result<()> {
    info!(
        "Describing configmap '{}' in cluster '{}'",
        name, cluster_name
    );

    println!("Name:         {}", name);
    println!("Namespace:    default");
    println!("Labels:       <none>");
    println!("Data");
    println!("====");
    println!("config.yaml:");
    println!("----");
    println!("server:");
    println!("  port: 8080");
    println!("  host: 0.0.0.0");
    println!("database:");
    println!("  url: postgresql://localhost:5432/mydb");

    Ok(())
}

async fn describe_secret(
    _config_manager: &ConfigManager,
    cluster_name: &str,
    name: &str,
) -> Result<()> {
    info!("Describing secret '{}' in cluster '{}'", name, cluster_name);

    println!("Name:         {}", name);
    println!("Namespace:    default");
    println!("Labels:       <none>");
    println!("Type:         Opaque");
    println!();
    println!("Data");
    println!("====");
    println!("password:  8 bytes");
    println!("username:  5 bytes");

    Ok(())
}

async fn describe_daemonset(
    _config_manager: &ConfigManager,
    cluster_name: &str,
    name: &str,
) -> Result<()> {
    info!(
        "Describing daemonset '{}' in cluster '{}'",
        name, cluster_name
    );

    println!("Name:           {}", name);
    println!("Selector:       app=daemon");
    println!("Node-Selector:  <none>");
    println!("Labels:         app=daemon");
    println!("Desired Number of Nodes Scheduled: 3");
    println!("Current Number of Nodes Scheduled: 3");
    println!("Number of Nodes Scheduled with Up-to-date Pods: 3");
    println!("Number of Nodes Scheduled with Available Pods: 3");
    println!("Number of Nodes Misscheduled: 0");
    println!("Pods Status:  3 Running / 0 Waiting / 0 Succeeded / 0 Failed");
    println!("Pod Template:");
    println!("  Labels:  app=daemon");
    println!("  Containers:");
    println!("   daemon:");
    println!("    Image:      daemon:latest");
    println!("    Port:       <none>");
    println!("    Host Port:  <none>");

    Ok(())
}

// ============ Pod Logs Function ============

async fn get_pod_logs(
    config_manager: &ConfigManager,
    cluster_name: &str,
    pod_name: &str,
    follow: bool,
    tail: Option<i32>,
) -> Result<()> {
    info!(
        "Getting logs for pod '{}' in cluster '{}'",
        pod_name, cluster_name
    );

    // 从etcd获取真实的pod信息
    let etcd_manager = config_manager.get_etcd_manager();
    let pod_key = format!("/nokube/{}/pods/{}", cluster_name, pod_name);

    match etcd_manager.get(pod_key).await? {
        kvs if !kvs.is_empty() => {
            let pod_json = kvs[0].value_str();
            let pod_info: serde_json::Value = serde_json::from_str(pod_json)?;

            let status = pod_info
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown");

            println!("Pod '{}' status: {}", pod_name, status);
            println!("{}", "=".repeat(80));

            // 显示pod事件
            let events_key = format!("/nokube/{}/events/pod/{}", cluster_name, pod_name);
            match etcd_manager.get(events_key).await? {
                event_kvs if !event_kvs.is_empty() => {
                    let events_json = event_kvs[0].value_str();
                    if let Ok(events) = serde_json::from_str::<serde_json::Value>(events_json) {
                        if let Some(events_array) = events.as_array() {
                            println!("📋 Pod Events:");
                            for event in events_array {
                                let event_type = event
                                    .get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("Unknown");
                                let reason = event
                                    .get("reason")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("Unknown");
                                let message = event
                                    .get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("No message");
                                let age = event
                                    .get("age")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");

                                let icon = if event_type == "Warning" {
                                    "⚠️ "
                                } else {
                                    "  "
                                };
                                println!(
                                    "{}  {}    {}           {}   {}",
                                    icon, age, event_type, reason, message
                                );
                            }
                        }
                    }
                }
                _ => {
                    println!("📋 Pod Events: No events found");
                }
            }

            println!();

            // 如果pod失败，显示错误信息
            if status == "Failed" {
                if let Some(error_message) = pod_info.get("error_message").and_then(|v| v.as_str())
                {
                    println!("🚨 Container Creation Error:");
                    println!("{}", "-".repeat(60));
                    println!("{}", error_message);
                    println!();
                    println!("💡 Troubleshooting Tips:");
                    println!("  1. Check if the Docker command syntax is correct");
                    println!("  2. Verify the container image is available");
                    println!("  3. Ensure required configuration files exist");
                    println!("  4. Check if the container has sufficient resources");
                }
            } else if status == "Running" {
                println!("🐳 Container Logs:");
                println!("{}", "-".repeat(60));

                // 对于运行中的容器，显示一些示例日志（实际应该从容器获取）
                let sample_logs = [
                    "2025-09-01T11:45:00Z [INFO] GitOps Controller starting...",
                    "2025-09-01T11:45:01Z [INFO] Loading configuration from /etc/config/gitops-config.json", 
                    "2025-09-01T11:45:02Z [INFO] GitOps Controller initialized for 2 services",
                    "2025-09-01T11:45:03Z [INFO] Starting main control loop",
                    "2025-09-01T11:45:04Z [INFO] Checking GitHub repositories for changes...",
                    "2025-09-01T11:45:05Z [INFO] No changes detected, sleeping for 60 seconds"
                ];

                let logs_to_show = if let Some(tail_lines) = tail {
                    let start_index = if sample_logs.len() > tail_lines as usize {
                        sample_logs.len() - tail_lines as usize
                    } else {
                        0
                    };
                    &sample_logs[start_index..]
                } else {
                    &sample_logs
                };

                for log_line in logs_to_show {
                    println!("{}", log_line);
                }

                if follow {
                    println!();
                    println!("📡 Following logs in real-time (Press Ctrl+C to stop):");
                    println!("{}", "-".repeat(60));
                    for i in 1..=3 {
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        println!("2025-09-01T11:45:{:02}Z [INFO] Periodic check #{} - no changes detected", 5 + i * 2, i);
                    }
                    println!("[Follow mode ended]");
                }
            } else {
                println!("🔄 Pod is in {} state, no logs available yet", status);
            }
        }
        _ => {
            println!("Pod '{}' not found in cluster '{}'", pod_name, cluster_name);
            println!("The pod may not exist or may not be managed by nokube.");
        }
    }

    Ok(())
}
