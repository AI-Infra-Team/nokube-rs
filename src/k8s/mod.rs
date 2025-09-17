// NoKube Kubernetes抽象层
// 支持往k8s集群apply k8s yaml，通过service agent内的协程运行

pub mod actors;
pub mod controllers;
pub mod storage;
pub mod the_proxy;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// 全局归属路径 - 用于唯一标识组件层级关系
/// 格式: daemonset-aaa/deployment-node1/container-aaa
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GlobalAttributionPath {
    pub path: String,
}

impl std::fmt::Display for GlobalAttributionPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path)
    }
}

impl GlobalAttributionPath {
    pub fn new(path: String) -> Self {
        Self { path }
    }

    pub fn parent(&self) -> Option<GlobalAttributionPath> {
        let parts: Vec<&str> = self.path.rsplitn(2, '/').collect();
        if parts.len() > 1 {
            Some(GlobalAttributionPath::new(parts[1].to_string()))
        } else {
            None
        }
    }

    pub fn child(&self, name: &str) -> GlobalAttributionPath {
        GlobalAttributionPath::new(format!("{}/{}", self.path, name))
    }

    pub fn workspace_path(&self, base_workspace: &str) -> String {
        format!("{}/{}", base_workspace, self.path)
    }
}

/// Actor 类型（模拟 K8s 角色）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActorKind {
    ConfigMap,
    Secret,
    DaemonSet,
    Deployment,
    Pod,
    Controller,
}

/// 异步 Actor 基础 trait
#[async_trait::async_trait]
pub trait AsyncActor: Send + Sync {
    fn actor_kind(&self) -> ActorKind;
    fn attribution_path(&self) -> &GlobalAttributionPath;
    async fn start(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
    async fn update_config(&mut self) -> Result<()>;
    async fn health_check(&self) -> Result<bool>;
}

/// Actor 状态 - 用于 TheProxy 系统
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActorState {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

/// Actor 孤儿清理接口——由 KubeController 调度执行
#[async_trait::async_trait]
pub trait ActorOrphanCleanup: Send + Sync {
    /// 由 KubeController 定期调用，执行父级引用校验并在需要时完成清理
    async fn cleanup_if_orphaned(&self) -> Result<()>;
}
