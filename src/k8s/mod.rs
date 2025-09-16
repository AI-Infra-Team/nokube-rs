// NoKube Kubernetes抽象层
// 支持往k8s集群apply k8s yaml，通过service agent内的协程运行

pub mod objects;
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

/// k8s对象类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum K8sObjectType {
    ConfigMap,
    Secret,
    DaemonSet,
    Deployment,
    Pod,
}

/// 异步任务对象模板基础trait
#[async_trait::async_trait]
pub trait AsyncTaskObject: Send + Sync {
    fn object_type(&self) -> K8sObjectType;
    fn attribution_path(&self) -> &GlobalAttributionPath;
    async fn start(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
    async fn update_config(&mut self) -> Result<()>;
    async fn health_check(&self) -> Result<bool>;
}

/// 组件状态 - 用于 TheProxy 系统
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ComponentStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

/// Actor 监督接口：统一“子随父灭”和“孤儿回收”逻辑
#[async_trait::async_trait]
pub trait ActorSupervise: Send + Sync {
    /// 定期调用：由 ServiceAgent/KubeController 调度
    /// 实现需检查父/上级是否存在（etcd key 或 alive 租约），若不存在应自清理（停止容器并清理 etcd pod/events）
    async fn supervise_parent(&self) -> Result<()>;
}
