// KubeController - 负责监控初始组件是否正常运行
use crate::k8s::objects::{DaemonSetObject, DeploymentObject};
use crate::k8s::the_proxy::ActorAliveRequest;
use crate::k8s::{
    ActorOrphanCleanup, AsyncTaskObject, ComponentStatus, GlobalAttributionPath, K8sObjectType,
};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, Duration};

/// KubeController - 管理和监控所有k8s对象
pub struct KubeController {
    pub attribution_path: GlobalAttributionPath,
    pub workspace: String,

    // 管理的对象
    pub daemonsets: Arc<RwLock<HashMap<String, DaemonSetObject>>>,
    pub deployments: Arc<RwLock<HashMap<String, DeploymentObject>>>,

    // TheProxy 管理（替代心跳管理）
    pub proxy_tx: mpsc::Sender<ActorAliveRequest>,

    // 运行状态
    pub status: ComponentStatus,
}

impl KubeController {
    pub fn new(workspace: String) -> Self {
        let attribution_path = GlobalAttributionPath::new("kubecontroller".to_string());
        let (proxy_tx, _proxy_rx) = mpsc::channel(1000); // 创建 TheProxy 通道

        Self {
            attribution_path,
            workspace,
            daemonsets: Arc::new(RwLock::new(HashMap::new())),
            deployments: Arc::new(RwLock::new(HashMap::new())),
            proxy_tx,
            status: ComponentStatus::Starting,
        }
    }

    /// 启动KubeController
    pub async fn start(&mut self) -> Result<()> {
        tracing::info!("Starting KubeController");

        self.status = ComponentStatus::Starting;

        // 启动组件监控协程
        self.start_component_monitor().await?;

        // 启动配置更新监控协程
        self.start_config_monitor().await?;

        self.status = ComponentStatus::Running;

        tracing::info!("KubeController started successfully");
        Ok(())
    }

    /// 停止KubeController
    pub async fn stop(&mut self) -> Result<()> {
        tracing::info!("Stopping KubeController");

        self.status = ComponentStatus::Stopping;

        // 停止所有DaemonSet
        let mut daemonsets = self.daemonsets.write().await;
        for (_, mut daemonset) in std::mem::take(&mut *daemonsets) {
            daemonset.stop().await?;
        }

        // 停止所有Deployment
        let mut deployments = self.deployments.write().await;
        for (_, mut deployment) in std::mem::take(&mut *deployments) {
            deployment.stop().await?;
        }

        tracing::info!("KubeController stopped successfully");
        Ok(())
    }

    /// 添加DaemonSet
    pub async fn add_daemonset(&self, mut daemonset: DaemonSetObject) -> Result<()> {
        tracing::info!("Adding DaemonSet: {}", daemonset.name);

        daemonset.start().await?;

        let mut daemonsets = self.daemonsets.write().await;
        daemonsets.insert(daemonset.name.clone(), daemonset);

        tracing::info!("DaemonSet added successfully");
        Ok(())
    }

    /// 添加Deployment
    pub async fn add_deployment(&self, mut deployment: DeploymentObject) -> Result<()> {
        tracing::info!("Adding Deployment: {}", deployment.name);

        deployment.start().await?;

        let mut deployments = self.deployments.write().await;
        deployments.insert(deployment.name.clone(), deployment);

        tracing::info!("Deployment added successfully");
        Ok(())
    }

    /// 移除DaemonSet
    pub async fn remove_daemonset(&self, name: &str) -> Result<()> {
        tracing::info!("Removing DaemonSet: {}", name);

        let mut daemonsets = self.daemonsets.write().await;
        if let Some(mut daemonset) = daemonsets.remove(name) {
            daemonset.stop().await?;
            tracing::info!("DaemonSet {} removed successfully", name);
        } else {
            tracing::warn!("DaemonSet {} not found", name);
        }

        Ok(())
    }

    /// 移除Deployment
    pub async fn remove_deployment(&self, name: &str) -> Result<()> {
        tracing::info!("Removing Deployment: {}", name);

        let mut deployments = self.deployments.write().await;
        if let Some(mut deployment) = deployments.remove(name) {
            deployment.stop().await?;
            tracing::info!("Deployment {} removed successfully", name);
        } else {
            tracing::warn!("Deployment {} not found", name);
        }

        Ok(())
    }

    /// 启动组件监控协程
    async fn start_component_monitor(&self) -> Result<()> {
        let daemonsets = self.daemonsets.clone();
        let deployments = self.deployments.clone();

        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(15)); // 更快收敛孤儿

            loop {
                interval.tick().await;

                // 检查DaemonSet健康状况
                {
                    let daemonsets_guard = daemonsets.read().await;
                    for (name, daemonset) in daemonsets_guard.iter() {
                        match daemonset.health_check().await {
                            Ok(is_healthy) => {
                                if !is_healthy {
                                    tracing::warn!("DaemonSet {} is unhealthy", name);
                                    // 可以在这里实现重启逻辑
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to check health for DaemonSet {}: {}",
                                    name,
                                    e
                                );
                            }
                        }
                        // 中心调度：指派各 pod 执行必要的孤儿清理
                        for (_node, pod) in daemonset.pods.iter() {
                            let _ = pod.cleanup_if_orphaned().await;
                        }
                    }
                }

                // 检查Deployment健康状况
                {
                    let deployments_guard = deployments.read().await;
                    for (name, deployment) in deployments_guard.iter() {
                        match deployment.health_check().await {
                            Ok(is_healthy) => {
                                if !is_healthy {
                                    tracing::warn!("Deployment {} is unhealthy", name);
                                    // 可以在这里实现重启逻辑
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to check health for Deployment {}: {}",
                                    name,
                                    e
                                );
                            }
                        }
                        // 中心调度：指派各 pod 执行必要的孤儿清理
                        for (_pname, pod) in deployment.pods.iter() {
                            let _ = pod.cleanup_if_orphaned().await;
                        }
                    }
                }
            }
        });

        Ok(())
    }

    /// 启动配置更新监控协程
    async fn start_config_monitor(&self) -> Result<()> {
        let daemonsets = self.daemonsets.clone();
        let deployments = self.deployments.clone();

        tokio::spawn(async move {
            // 这里可以监听etcd的配置变化事件
            // 当配置更新时，调用相应对象的update_config方法

            // 示例：定期检查配置更新（实际应该使用etcd watch）
            let mut interval = interval(Duration::from_secs(300)); // 5分钟检查一次

            loop {
                interval.tick().await;

                // 检查是否有配置更新
                // 如果有，更新相应的对象

                tracing::debug!("Checking for configuration updates");
            }
        });

        Ok(())
    }

    /// 重启组件（在关闭和配置更新时重新调度启动）
    pub async fn restart_component(&self, component_type: K8sObjectType, name: &str) -> Result<()> {
        tracing::info!("Restarting component: {:?} {}", component_type, name);

        match component_type {
            K8sObjectType::DaemonSet => {
                let daemonsets = self.daemonsets.read().await;
                if let Some(daemonset) = daemonsets.get(name) {
                    // 重启DaemonSet（先停止再启动）
                    // 这里需要克隆daemonset，因为我们需要修改它
                    // 实际实现中可能需要更复杂的处理
                    tracing::info!("Restarting DaemonSet: {}", name);
                }
            }
            K8sObjectType::Deployment => {
                let deployments = self.deployments.read().await;
                if let Some(deployment) = deployments.get(name) {
                    // 重启Deployment
                    tracing::info!("Restarting Deployment: {}", name);
                }
            }
            _ => {
                tracing::warn!(
                    "Unsupported component type for restart: {:?}",
                    component_type
                );
            }
        }

        Ok(())
    }

    /// 获取所有组件状态
    pub async fn get_component_status(&self) -> HashMap<String, ComponentStatus> {
        let mut status_map = HashMap::new();

        // 获取DaemonSet状态
        {
            let daemonsets = self.daemonsets.read().await;
            for (name, daemonset) in daemonsets.iter() {
                status_map.insert(format!("daemonset/{}", name), daemonset.status.clone());
            }
        }

        // 获取Deployment状态
        {
            let deployments = self.deployments.read().await;
            for (name, deployment) in deployments.iter() {
                status_map.insert(format!("deployment/{}", name), deployment.status.clone());
            }
        }

        status_map
    }
}

#[async_trait::async_trait]
impl AsyncTaskObject for KubeController {
    fn object_type(&self) -> K8sObjectType {
        K8sObjectType::DaemonSet // 暂时使用DaemonSet，实际可能需要新的类型
    }

    fn attribution_path(&self) -> &GlobalAttributionPath {
        &self.attribution_path
    }

    async fn start(&mut self) -> Result<()> {
        // 委托给上面的start方法
        KubeController::start(self).await
    }

    async fn stop(&mut self) -> Result<()> {
        // 委托给上面的stop方法
        KubeController::stop(self).await
    }

    async fn update_config(&mut self) -> Result<()> {
        tracing::info!("Updating config for KubeController");

        // 更新所有管理的组件
        {
            let mut daemonsets = self.daemonsets.write().await;
            for (_, daemonset) in daemonsets.iter_mut() {
                daemonset.update_config().await?;
            }
        }

        {
            let mut deployments = self.deployments.write().await;
            for (_, deployment) in deployments.iter_mut() {
                deployment.update_config().await?;
            }
        }

        Ok(())
    }

    async fn health_check(&self) -> Result<bool> {
        // 检查所有管理的组件是否健康
        {
            let daemonsets = self.daemonsets.read().await;
            for (_, daemonset) in daemonsets.iter() {
                if !daemonset.health_check().await? {
                    return Ok(false);
                }
            }
        }

        {
            let deployments = self.deployments.read().await;
            for (_, deployment) in deployments.iter() {
                if !deployment.health_check().await? {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }
}
