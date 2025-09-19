use std::collections::HashMap;

use chrono::Utc;
use serde::{Deserialize, Serialize};

pub const SERVICE_AGENT_MODULES: &[&str] = &[
    "bootstrap",
    "the_proxy",
    "kube_controller",
    "startup_reconcile",
    "load_actors",
    "exporter",
    "log_collector",
    "grafana",
    "metrics_collector",
    "bound_services",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModuleState {
    Pending,
    InProgress,
    Success,
    Failure,
}

impl ModuleState {
    pub fn is_success(&self) -> bool {
        matches!(self, ModuleState::Success)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleStatus {
    pub state: ModuleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub updated_at: String,
}

impl ModuleStatus {
    pub fn new(state: ModuleState) -> Self {
        Self {
            state,
            message: None,
            updated_at: Utc::now().to_rfc3339(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDeploymentStatus {
    pub deployment_version: String,
    pub cluster_name: String,
    pub node_id: String,
    #[serde(default)]
    pub modules: HashMap<String, ModuleStatus>,
    #[serde(default = "default_overall_state")]
    pub overall_state: ModuleState,
    pub updated_at: String,
}

fn default_overall_state() -> ModuleState {
    ModuleState::Pending
}

impl ServiceDeploymentStatus {
    pub fn new(cluster_name: String, node_id: String, deployment_version: String) -> Self {
        let mut modules = HashMap::new();
        for module in SERVICE_AGENT_MODULES {
            modules.insert(
                (*module).to_string(),
                ModuleStatus::new(ModuleState::Pending),
            );
        }

        Self {
            deployment_version,
            cluster_name,
            node_id,
            modules,
            overall_state: ModuleState::Pending,
            updated_at: Utc::now().to_rfc3339(),
        }
    }

    pub fn all_modules_success(&self) -> bool {
        SERVICE_AGENT_MODULES.iter().all(|module| {
            self.modules
                .get(*module)
                .map_or(false, |m| m.state.is_success())
        })
    }
}
