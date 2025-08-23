use thiserror::Error;

#[derive(Error, Debug)]
pub enum NokubeError {
    #[error("Configuration error: {0}")]
    Config(String),

    // #[error("Etcd error: {0}")]
    // Etcd(#[from] etcd_rs::Error),
    #[error("SSH connection error: {0}")]
    Ssh(#[from] ssh2::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Deployment error: {0}")]
    Deployment(String),

    #[error("Agent error: {0}")]
    Agent(String),

    #[error("Monitoring error: {0}")]
    Monitoring(String),

    #[error("Cluster not found: {cluster}. Current clusters: {cluster_list}")]
    ClusterNotFound {
        cluster: String,
        cluster_list: String,
    },

    #[error("Node not found: {0}")]
    NodeNotFound(String),

    #[error("Service deployment failed: {service} on node {node}: {reason}")]
    ServiceDeploymentFailed {
        service: String,
        node: String,
        reason: String,
    },

    #[error("Dependency installation failed: {dependency}: {reason}")]
    DependencyInstallationFailed { dependency: String, reason: String },

    #[error("File operation failed on {file_path}: {reason}")]
    FileOperation { file_path: String, reason: String },
}

pub type Result<T> = std::result::Result<T, NokubeError>;

impl From<anyhow::Error> for NokubeError {
    fn from(err: anyhow::Error) -> Self {
        NokubeError::Config(err.to_string())
    }
}