use base64::Engine;
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new cluster configuration
    Init {
        #[arg(short, long)]
        cluster_name: String,
    },
    /// Create or update cluster deployment
    NewOrUpdate {
        /// Path to cluster configuration YAML file
        config_file: String,
    },
    /// Start monitoring services
    Monitor {
        #[arg(short, long)]
        cluster_name: String,
    },
    /// Run agent in command mode (for deployment operations)
    AgentCommand {
        #[arg(short, long)]
        config_path: String,
        #[arg(long)]
        extra_params: Option<String>,
    },
    /// Run agent in service mode (persistent execution)
    AgentService {
        #[arg(short, long)]
        config_path: String,
    },
}
use clap::{Parser, Subcommand};
use tracing::{error, info, Level};
use tracing_subscriber;
use std::fs;

mod config;
mod agent;
mod remote_ctl;
mod error;

use config::{ConfigManager, cluster_config::ClusterConfig};
use remote_ctl::DeploymentController;
use error::{NokubeError, Result};
use agent::{CommandModeAgent, ServiceModeAgent};
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Init { cluster_name } => handle_init(cluster_name).await,
        Commands::NewOrUpdate { config_file } => handle_new_or_update(config_file).await,
        Commands::Monitor { cluster_name } => handle_monitor(cluster_name).await,
        Commands::AgentCommand { config_path, extra_params } => handle_agent_command(config_path, extra_params).await,
        Commands::AgentService { config_path } => handle_agent_service(config_path).await,
    };

    if let Err(e) = &result {
        error!("Application error: {}", e);
        std::process::exit(1);
    }
    return result;

// 逻辑拆分函数实现（移到 main 外部）
async fn handle_init(cluster_name: String) -> Result<()> {
    info!("Initializing cluster: {}", cluster_name);
    match ConfigManager::new().await {
        Ok(config_manager) => {
            match config_manager.init_cluster(&cluster_name).await {
                Ok(_) => {
                    info!("Cluster {} initialized successfully", cluster_name);
                    Ok(())
                }
                Err(e) => {
                    error!("Failed to initialize cluster {}: {}", cluster_name, e);
                    Err(NokubeError::Config(format!("Cluster initialization failed: {}", e)))
                }
            }
        }
        Err(e) => {
            error!("Failed to create config manager: {}", e);
            Err(NokubeError::Config(format!("Config manager creation failed: {}", e)))
        }
    }
}

async fn handle_new_or_update(config_file: String) -> Result<()> {
    info!("Deploying/updating cluster from config: {}", config_file);
    let cluster_config = match read_cluster_config_from_file(&config_file).await {
        Ok(config) => config,
        Err(e) => {
            error!("Failed to read config file {}: {}", config_file, e);
            return Err(NokubeError::Config(format!("Config file reading failed: {}", e)));
        }
    };
    let cluster_name = cluster_config.cluster_name.clone();
    match DeploymentController::new().await {
        Ok(mut deployment_controller) => {
            let config_manager_result = ConfigManager::new().await;
            match config_manager_result {
                Ok(config_manager) => {
                    if let Err(e) = config_manager.update_cluster_config(&cluster_config).await {
                        error!("Failed to store cluster config: {}", e);
                        return Err(NokubeError::Config(format!("Config storage failed: {}", e)));
                    }
                    if let Err(e) = config_manager.init_cluster(&cluster_name).await {
                        error!("Failed to store cluster meta: {}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to create config manager: {}", e);
                    return Err(NokubeError::Config(format!("Config manager creation failed: {}", e)));
                }
            }
            match deployment_controller.deploy_or_update(&cluster_name).await {
                Ok(_) => {
                    info!("Cluster {} deployed/updated successfully", cluster_name);
                    Ok(())
                }
                Err(e) => {
                    error!("Failed to deploy/update cluster {}: {}", cluster_name, e);
                    Err(NokubeError::Deployment(format!("Deployment failed: {}", e)))
                }
            }
        }
        Err(e) => {
            error!("Failed to create deployment controller: {}", e);
            Err(NokubeError::Deployment(format!("Controller creation failed: {}", e)))
        }
    }
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
                    error!("Failed to setup monitoring for cluster {}: {}", cluster_name, e);
                    Err(NokubeError::Monitoring(format!("Monitoring setup failed: {}", e)))
                }
            }
        }
        Err(e) => {
            error!("Failed to create deployment controller: {}", e);
            Err(NokubeError::Monitoring(format!("Controller creation failed: {}", e)))
        }
    }
}

async fn handle_agent_command(config_path: String, extra_params: Option<String>) -> Result<()> {
    info!("Running agent in command mode with config: {}", config_path);
    let cluster_name = extra_params.as_ref()
        .and_then(|params_b64| {
            base64::engine::general_purpose::STANDARD.decode(params_b64).ok()
                .and_then(|params_json| String::from_utf8(params_json).ok())
                .and_then(|params_str| serde_json::from_str::<serde_json::Value>(&params_str).ok())
                .and_then(|params| params.get("cluster_name").and_then(|v| v.as_str()).map(|s| s.to_string()))
        })
        .or_else(|| {
            std::fs::read_to_string(&config_path).ok()
                .and_then(|content| serde_yaml::from_str::<config::cluster_config::ClusterConfig>(&content).ok())
                .map(|cfg| cfg.cluster_name.clone())
        })
        .unwrap_or_else(|| "default".to_string());

    let config_manager = ConfigManager::new().await
        .map_err(|e| {
            error!("Failed to create config manager: {}", e);
            NokubeError::Config(format!("Config manager creation failed: {}", e))
        })?;

    let cluster_config = config_manager.get_cluster_config(&cluster_name).await
        .map_err(|e| {
            error!("Failed to load cluster config: {}", e);
            NokubeError::Config(format!("Config loading failed: {}", e))
        })?;

    let cluster_config = match cluster_config {
        Some(cfg) => cfg,
        None => {
            let cluster_metas = config_manager.list_clusters().await.unwrap_or_default();
            let cluster_names: Vec<String> = cluster_metas.iter().map(|m| m.config.cluster_name.clone()).collect();
            let cluster_list = cluster_names.join(", ");
            error!("Cluster config not found for: {}", cluster_name);
            return Err(NokubeError::ClusterNotFound {
                cluster: cluster_name.to_string(),
                cluster_list,
            });
        }
    };

    let command_agent = CommandModeAgent::new(cluster_config);
    command_agent.execute().await.map_err(|e| {
        error!("Command mode agent failed: {}", e);
        NokubeError::Agent(format!("Command mode execution failed: {}", e))
    })?;
    info!("Command mode agent completed successfully");
    Ok(())
}

async fn handle_agent_service(config_path: String) -> Result<()> {
    info!("Starting agent service with config: {}", config_path);
    let cluster_name = match std::fs::read_to_string(&config_path) {
        Ok(content) => match serde_yaml::from_str::<config::cluster_config::ClusterConfig>(&content) {
            Ok(cfg) => cfg.cluster_name.clone(),
            Err(_) => "default".to_string(),
        },
        Err(_) => "default".to_string(),
    };

    let config_manager = ConfigManager::new().await
        .map_err(|e| {
            error!("Failed to create config manager: {}", e);
            NokubeError::Config(format!("Config manager creation failed: {}", e))
        })?;

    let cluster_config = config_manager.get_cluster_config(&cluster_name).await
        .map_err(|e| {
            error!("Failed to load cluster config: {}", e);
            NokubeError::Config(format!("Config loading failed: {}", e))
        })?;

    let cluster_config = match cluster_config {
        Some(cfg) => cfg,
        None => {
            let cluster_metas = config_manager.list_clusters().await.unwrap_or_default();
            let cluster_names: Vec<String> = cluster_metas.iter().map(|m| m.config.cluster_name.clone()).collect();
            let cluster_list = cluster_names.join(", ");
            error!("Cluster config not found for: {}", cluster_name);
            return Err(NokubeError::ClusterNotFound {
                cluster: cluster_name.to_string(),
                cluster_list,
            });
        }
    };

    let node_id = std::env::var("NOKUBE_NODE_ID").unwrap_or_else(|_| "default-node".to_string());
    let etcd_endpoints = vec!["127.0.0.1:2379".to_string()];
    let mut service_agent = ServiceModeAgent::new(
        node_id,
        cluster_name.to_string(),
        etcd_endpoints,
        cluster_config
    ).await?;
    service_agent.run().await.map_err(|e| {
        error!("Service mode agent failed: {}", e);
        NokubeError::Agent(format!("Service mode execution failed: {}", e))
    })?;
    info!("Service mode agent completed successfully");
    Ok(())
}
}

async fn read_cluster_config_from_file(file_path: &str) -> Result<ClusterConfig> {
    info!("Reading cluster config from file: {}", file_path);
    
    let content = fs::read_to_string(file_path)
        .map_err(|e| NokubeError::Config(format!("Failed to read file {}: {}", file_path, e)))?;
    
    let cluster_config: ClusterConfig = serde_yaml::from_str(&content)
        .map_err(|e| NokubeError::Config(format!("Failed to parse YAML from {}: {}", file_path, e)))?;
    
    info!("Successfully loaded cluster config for: {}", cluster_config.cluster_name);
    Ok(cluster_config)
}