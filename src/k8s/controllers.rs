// KubeController - 负责监控初始Actor是否正常运行
use crate::agent::general::log_collector::LogCollectorCommand;
use crate::agent::runtime::deployment::{
    calc_hash_u64, create_deployment_container, enumerate_actor_container_names,
};
use crate::config::etcd_manager::EtcdManager;
use crate::k8s::actors::{DaemonSetActor, DeploymentActor};
use crate::k8s::the_proxy::ActorAliveRequest;
use crate::k8s::{ActorActionPlan, ActorKind, ActorState, AsyncActor, GlobalAttributionPath};
use anyhow::Result;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

/// KubeController - 管理和监控所有k8sActor
pub struct KubeController {
    pub attribution_path: GlobalAttributionPath,
    pub workspace: String,
    pub cluster_name: String,
    pub _node_id: String,

    // 管理的Actor
    pub daemonsets: Arc<RwLock<HashMap<String, DaemonSetActor>>>,
    pub deployments: Arc<RwLock<HashMap<String, DeploymentActor>>>,

    // TheProxy 管理（替代心跳管理）
    pub proxy_tx: mpsc::Sender<ActorAliveRequest>,

    etcd_manager: Arc<EtcdManager>,
    log_notifier: Arc<Mutex<Option<mpsc::Sender<LogCollectorCommand>>>>,

    // 运行状态
    pub status: ActorState,
}

impl KubeController {
    pub fn new(
        workspace: String,
        cluster_name: String,
        node_id: String,
        etcd_manager: Arc<EtcdManager>,
        proxy_tx: mpsc::Sender<ActorAliveRequest>,
    ) -> Self {
        let attribution_path = GlobalAttributionPath::new("kubecontroller".to_string());

        Self {
            attribution_path,
            workspace,
            cluster_name,
            _node_id: node_id,
            daemonsets: Arc::new(RwLock::new(HashMap::new())),
            deployments: Arc::new(RwLock::new(HashMap::new())),
            proxy_tx,
            etcd_manager,
            log_notifier: Arc::new(Mutex::new(None)),
            status: ActorState::Starting,
        }
    }

    pub async fn register_log_collector(
        &self,
        sender: mpsc::Sender<LogCollectorCommand>,
    ) -> Result<()> {
        {
            let mut guard = self.log_notifier.lock().await;
            *guard = Some(sender.clone());
        }

        self.send_existing_container_notifications(&sender).await;
        Ok(())
    }

    async fn send_existing_container_notifications(
        &self,
        sender: &mpsc::Sender<LogCollectorCommand>,
    ) {
        let runtime_path = crate::agent::general::DockerRunner::get_runtime_path()
            .unwrap_or_else(|_| "docker".to_string());
        match Command::new(&runtime_path)
            .args(["ps", "--format", "{{.Names}}"])
            .output()
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for name in stdout.lines() {
                    if name.starts_with("nokube-pod-") {
                        if let Err(e) = sender
                            .send(LogCollectorCommand::FollowContainer {
                                name: name.to_string(),
                            })
                            .await
                        {
                            warn!(
                                "Failed to enqueue existing container follow command ({}): {}",
                                name, e
                            );
                        }
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Failed to enumerate existing containers for log collector: {}",
                    e
                );
            }
        }
    }

    async fn publish_follow_commands(
        notifier: &Arc<Mutex<Option<mpsc::Sender<LogCollectorCommand>>>>,
        containers: &[String],
    ) {
        if containers.is_empty() {
            return;
        }

        let maybe_tx = {
            let guard = notifier.lock().await;
            guard.clone()
        };

        if let Some(tx) = maybe_tx {
            for name in containers {
                if let Err(e) = tx
                    .send(LogCollectorCommand::FollowContainer { name: name.clone() })
                    .await
                {
                    warn!("Failed to notify log collector to follow {}: {}", name, e);
                }
            }
        }
    }

    async fn publish_stop_commands(
        notifier: &Arc<Mutex<Option<mpsc::Sender<LogCollectorCommand>>>>,
        containers: &[String],
    ) {
        if containers.is_empty() {
            return;
        }

        let maybe_tx = {
            let guard = notifier.lock().await;
            guard.clone()
        };

        if let Some(tx) = maybe_tx {
            for name in containers {
                if let Err(e) = tx
                    .send(LogCollectorCommand::StopContainer { name: name.clone() })
                    .await
                {
                    warn!("Failed to notify log collector to stop {}: {}", name, e);
                }
            }
        }
    }

    /// 启动KubeController
    pub async fn start(&mut self) -> Result<()> {
        tracing::info!("Starting KubeController");

        self.status = ActorState::Starting;

        // 启动Actor监控协程
        self.start_actor_health_monitor().await?;

        // 启动Actor扫描协程
        self.start_actor_monitor().await?;

        // 启动配置更新监控协程
        self.start_config_monitor().await?;

        self.status = ActorState::Running;

        tracing::info!("KubeController started successfully");
        Ok(())
    }

    /// 停止KubeController
    pub async fn stop(&mut self) -> Result<()> {
        tracing::info!("Stopping KubeController");

        self.status = ActorState::Stopping;

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
    pub async fn add_daemonset(&self, mut daemonset: DaemonSetActor) -> Result<()> {
        tracing::info!("Adding DaemonSet: {}", daemonset.name);

        daemonset.start().await?;

        let mut daemonsets = self.daemonsets.write().await;
        daemonsets.insert(daemonset.name.clone(), daemonset);

        tracing::info!("DaemonSet added successfully");
        Ok(())
    }

    /// 添加Deployment
    pub async fn add_deployment(&self, mut deployment: DeploymentActor) -> Result<()> {
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

    /// 启动Actor监控协程
    async fn start_actor_health_monitor(&self) -> Result<()> {
        let daemonsets = self.daemonsets.clone();
        let deployments = self.deployments.clone();
        let etcd_manager = Arc::clone(&self.etcd_manager);
        let log_notifier = Arc::clone(&self.log_notifier);

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
                        match daemonset.check().await {
                            Ok(plan) => {
                                if !plan.is_empty() {
                                    KubeController::apply_action_plan(
                                        &etcd_manager,
                                        &log_notifier,
                                        plan,
                                    )
                                    .await;
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to run check() for DaemonSet {}: {}",
                                    name,
                                    e
                                );
                            }
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
                        match deployment.check().await {
                            Ok(plan) => {
                                if !plan.is_empty() {
                                    KubeController::apply_action_plan(
                                        &etcd_manager,
                                        &log_notifier,
                                        plan,
                                    )
                                    .await;
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to run check() for Deployment {}: {}",
                                    name,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    async fn apply_action_plan(
        etcd_manager: &Arc<EtcdManager>,
        log_notifier: &Arc<Mutex<Option<mpsc::Sender<LogCollectorCommand>>>>,
        plan: ActorActionPlan,
    ) {
        if plan.is_empty() {
            return;
        }

        let ActorActionPlan {
            containers_to_stop,
            etcd_keys_to_delete,
            signals_to_emit,
        } = plan;

        for action in containers_to_stop {
            info!(
                "Applying actor plan: stopping container '{}' (reason: {:?})",
                action.name, action.reason
            );
            let notify = vec![action.name.clone()];
            Self::publish_stop_commands(log_notifier, &notify).await;
            if let Err(e) = crate::agent::general::DockerRunner::stop(&action.name) {
                warn!(
                    "Failed to stop container {} during plan execution: {}",
                    action.name, e
                );
            }
            if let Err(e) = crate::agent::general::DockerRunner::remove_container(&action.name) {
                warn!(
                    "Failed to remove container {} during plan execution: {}",
                    action.name, e
                );
            }
        }

        for key in etcd_keys_to_delete {
            match etcd_manager.delete(key.clone()).await {
                Ok(_) => info!("Applying actor plan: deleted etcd key {}", key),
                Err(e) => warn!(
                    "Applying actor plan: failed to delete etcd key {}: {}",
                    key, e
                ),
            }
        }

        for signal in signals_to_emit {
            info!(
                "Applying actor plan: signal '{}' for {} (dispatch not yet implemented)",
                signal.signal, signal.target_path
            );
        }
    }

    async fn start_actor_monitor(&self) -> Result<()> {
        let etcd_manager = Arc::clone(&self.etcd_manager);
        let cluster_name = self.cluster_name.clone();
        let workspace = self.workspace.clone();
        let log_notifier = Arc::clone(&self.log_notifier);

        tokio::spawn(async move {
            use std::collections::HashMap;

            let mut interval = interval(Duration::from_secs(15));
            let mut seen_deploy_checksums: HashMap<String, (u64, Vec<String>)> = HashMap::new();
            let mut seen_daemon_checksums: HashMap<String, u64> = HashMap::new();

            loop {
                interval.tick().await;
                info!("Scanning actor definitions in cluster: {}", cluster_name);

                let deployment_prefix = format!("/nokube/{}/deployments/", cluster_name);
                match etcd_manager.get_prefix(deployment_prefix.clone()).await {
                    Ok(deployment_kvs) => {
                        info!(
                            "Deployment monitor: total keys={} (seen={})",
                            deployment_kvs.len(),
                            seen_deploy_checksums.len()
                        );
                        let mut current_map: HashMap<String, u64> = HashMap::new();
                        for kv in &deployment_kvs {
                            let key = String::from_utf8_lossy(&kv.key).to_string();
                            let val = String::from_utf8_lossy(&kv.value);
                            let csum = calc_hash_u64(&val);
                            current_map.insert(key, csum);
                        }

                        for kv in deployment_kvs {
                            let key_str = String::from_utf8_lossy(&kv.key).to_string();
                            let value_str = String::from_utf8_lossy(&kv.value).into_owned();
                            let checksum = calc_hash_u64(&value_str);
                            if !seen_deploy_checksums.contains_key(&key_str) {
                                let deployment_name =
                                    key_str.split('/').last().unwrap_or("unknown");
                                match serde_yaml::from_str::<serde_yaml::Value>(&value_str) {
                                    Ok(deployment_yaml) => {
                                        info!(
                                            "Processing new deployment: {} (key={})",
                                            deployment_name, key_str
                                        );

                                        let workspace_path = workspace.clone();
                                        let _ = std::fs::create_dir_all(&workspace_path);

                                        if let Err(e) = create_deployment_container(
                                            &deployment_yaml,
                                            deployment_name,
                                            &cluster_name,
                                            &etcd_manager,
                                            &workspace_path,
                                        )
                                        .await
                                        {
                                            error!(
                                                "Failed to create deployment {}: {}",
                                                deployment_name, e
                                            );
                                        } else {
                                            let container_names = enumerate_actor_container_names(
                                                &deployment_yaml,
                                                deployment_name,
                                            )
                                            .unwrap_or_else(|err| {
                                                warn!(
                                                    "Failed to enumerate containers for {}: {}",
                                                    deployment_name, err
                                                );
                                                vec![format!("nokube-pod-{}", deployment_name)]
                                            });
                                            KubeController::publish_follow_commands(
                                                &log_notifier,
                                                &container_names,
                                            )
                                            .await;
                                            seen_deploy_checksums.insert(
                                                key_str.clone(),
                                                (checksum, container_names),
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        error!(
                                            "Failed to parse deployment YAML for {}: {}",
                                            deployment_name, e
                                        );
                                    }
                                }
                            } else if let Some((prev_checksum, prev_containers)) =
                                seen_deploy_checksums.get(&key_str)
                            {
                                if *prev_checksum != checksum {
                                    let previous_containers = prev_containers.clone();
                                    let deployment_name =
                                        key_str.split('/').last().unwrap_or("unknown");
                                    info!(
                                        "🔁 Detected deployment update: {} (key={}), restarting",
                                        deployment_name, key_str
                                    );
                                    KubeController::publish_stop_commands(
                                        &log_notifier,
                                        &previous_containers,
                                    )
                                    .await;
                                    for container_name in previous_containers {
                                        if let Err(e) = crate::agent::general::DockerRunner::stop(
                                            &container_name,
                                        ) {
                                            tracing::warn!(
                                                "Failed to stop container {}: {}",
                                                container_name,
                                                e
                                            );
                                        }
                                        if let Err(e) =
                                            crate::agent::general::DockerRunner::remove_container(
                                                &container_name,
                                            )
                                        {
                                            tracing::warn!(
                                                "Failed to remove container {}: {}",
                                                container_name,
                                                e
                                            );
                                        }
                                    }
                                    match serde_yaml::from_str::<serde_yaml::Value>(&value_str) {
                                        Ok(deployment_yaml) => {
                                            let workspace_path = workspace.clone();
                                            let _ = std::fs::create_dir_all(&workspace_path);
                                            if let Err(e) = create_deployment_container(
                                                &deployment_yaml,
                                                deployment_name,
                                                &cluster_name,
                                                &etcd_manager,
                                                &workspace_path,
                                            )
                                            .await
                                            {
                                                error!(
                                                    "Failed to recreate deployment {}: {}",
                                                    deployment_name, e
                                                );
                                            } else {
                                                let container_names =
                                                    enumerate_actor_container_names(
                                                        &deployment_yaml,
                                                        deployment_name,
                                                    )
                                                    .unwrap_or_else(|err| {
                                                        warn!(
                                                            "Failed to enumerate containers for {} after update: {}",
                                                            deployment_name, err
                                                        );
                                                        vec![format!(
                                                            "nokube-pod-{}",
                                                            deployment_name
                                                        )]
                                                    });
                                                KubeController::publish_follow_commands(
                                                    &log_notifier,
                                                    &container_names,
                                                )
                                                .await;
                                                seen_deploy_checksums.insert(
                                                    key_str.clone(),
                                                    (checksum, container_names),
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            error!(
                                                "Failed to parse deployment YAML for {}: {}",
                                                deployment_name, e
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        let removed_deployments: Vec<String> = seen_deploy_checksums
                            .keys()
                            .filter(|existing_key| !current_map.contains_key(*existing_key))
                            .cloned()
                            .collect();

                        for key in removed_deployments {
                            let deployment_name = key.split('/').last().unwrap_or("unknown");
                            info!(
                                "Detected deployment removal: {} (key={}), cleaning up",
                                deployment_name, key
                            );
                            if let Some((_, container_names)) = seen_deploy_checksums.remove(&key) {
                                KubeController::publish_stop_commands(
                                    &log_notifier,
                                    &container_names,
                                )
                                .await;
                                for container_name in container_names {
                                    if let Err(e) =
                                        crate::agent::general::DockerRunner::stop(&container_name)
                                    {
                                        warn!(
                                            "Failed to stop container {} during removal handling: {}",
                                            container_name, e
                                        );
                                    }
                                    if let Err(e) =
                                        crate::agent::general::DockerRunner::remove_container(
                                            &container_name,
                                        )
                                    {
                                        warn!(
                                            "Failed to remove container {} during removal handling: {}",
                                            container_name, e
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to check deployments: {}", e);
                    }
                }

                let daemonset_prefix = format!("/nokube/{}/daemonsets/", cluster_name);
                match etcd_manager.get_prefix(daemonset_prefix).await {
                    Ok(daemonset_kvs) => {
                        info!(
                            "DaemonSet monitor: total keys={} (seen={})",
                            daemonset_kvs.len(),
                            seen_daemon_checksums.len()
                        );
                        let mut current_map: HashMap<String, u64> = HashMap::new();
                        for kv in &daemonset_kvs {
                            let key = String::from_utf8_lossy(&kv.key).to_string();
                            let val = String::from_utf8_lossy(&kv.value);
                            current_map.insert(key, calc_hash_u64(&val));
                        }

                        for kv in daemonset_kvs {
                            let key_str = String::from_utf8_lossy(&kv.key).to_string();
                            let val = String::from_utf8_lossy(&kv.value);
                            let checksum = calc_hash_u64(&val);
                            if !seen_daemon_checksums.contains_key(&key_str) {
                                let daemonset_name = key_str.split('/').last().unwrap_or("unknown");
                                match serde_yaml::from_str::<serde_yaml::Value>(&val) {
                                    Ok(_) => {
                                        info!(
                                            "Processing new daemonset: {} (key={})",
                                            daemonset_name, key_str
                                        );
                                        seen_daemon_checksums.insert(key_str.clone(), checksum);
                                    }
                                    Err(e) => {
                                        error!(
                                            "Failed to parse daemonset YAML for {}: {}",
                                            daemonset_name, e
                                        );
                                    }
                                }
                            } else if let Some(prev) = seen_daemon_checksums.get(&key_str) {
                                if *prev != checksum {
                                    let daemonset_name =
                                        key_str.split('/').last().unwrap_or("unknown");
                                    info!(
                                        "🔁 Detected daemonset update: {} (key={}), will recreate local ds container",
                                        daemonset_name,
                                        key_str
                                    );
                                    seen_daemon_checksums.insert(key_str.clone(), checksum);
                                }
                            }
                        }

                        let removed: Vec<String> = seen_daemon_checksums
                            .keys()
                            .filter(|k| !current_map.contains_key(*k))
                            .cloned()
                            .collect();
                        for key in removed {
                            let daemonset_name = key.split('/').last().unwrap_or("unknown");
                            info!(
                                "Detected daemonset removal: {} (key={}), cleaning up",
                                daemonset_name, key
                            );
                            seen_daemon_checksums.remove(&key);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to check daemonsets: {}", e);
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
            // 当配置更新时，调用相应Actor的update_config方法

            // 示例：定期检查配置更新（实际应该使用etcd watch）
            let mut interval = interval(Duration::from_secs(300)); // 5分钟检查一次

            loop {
                interval.tick().await;

                // 检查是否有配置更新
                // 如果有，更新相应的Actor

                tracing::debug!("Checking for configuration updates");
            }
        });

        Ok(())
    }

    /// 重启Actor（在关闭和配置更新时重新调度启动）
    pub async fn restart_actor(&self, component_type: ActorKind, name: &str) -> Result<()> {
        tracing::info!("Restarting component: {:?} {}", component_type, name);

        match component_type {
            ActorKind::DaemonSet => {
                let daemonsets = self.daemonsets.read().await;
                if let Some(daemonset) = daemonsets.get(name) {
                    // 重启DaemonSet（先停止再启动）
                    // 这里需要克隆daemonset，因为我们需要修改它
                    // 实际实现中可能需要更复杂的处理
                    tracing::info!("Restarting DaemonSet: {}", name);
                }
            }
            ActorKind::Deployment => {
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

    /// 获取所有Actor状态
    pub async fn get_actor_states(&self) -> HashMap<String, ActorState> {
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
impl AsyncActor for KubeController {
    fn actor_kind(&self) -> ActorKind {
        ActorKind::Controller
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

        // 更新所有管理的Actor
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
        // 检查所有管理的Actor是否健康
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
