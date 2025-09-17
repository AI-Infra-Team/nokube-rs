// Actor 实现 - DaemonSet、Deployment、Pod 等模拟单元
use crate::config::config_manager::ConfigManager;
use crate::k8s::the_proxy::ActorAliveRequest;
use crate::k8s::{
    ActorActionPlan, ActorKind, ActorOrphanCleanup, ActorState, AsyncActor, ContainerAction,
    GlobalAttributionPath,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Pod描述信息结构体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodDescription {
    pub name: String,
    pub namespace: String,
    pub priority: i32,
    pub node: String,
    pub node_ip: Option<String>,
    pub start_time: Option<DateTime<Utc>>,
    pub labels: HashMap<String, String>,
    pub status: PodStatus,
    pub ip: Option<String>,
    pub containers: Vec<ContainerStatus>,
    pub events: Vec<PodEvent>,
    // 新增 alive 状态信息
    pub alive_status: AliveStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AliveStatus {
    Alive,
    Dead,
    Unknown,
}

impl std::fmt::Display for AliveStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AliveStatus::Alive => write!(f, "Alive"),
            AliveStatus::Dead => write!(f, "Dead"),
            AliveStatus::Unknown => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)] // 添加 PartialEq
pub enum PodStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
    ContainerCreating,
}

impl std::fmt::Display for PodStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PodStatus::Pending => write!(f, "Pending"),
            PodStatus::Running => write!(f, "Running"),
            PodStatus::Succeeded => write!(f, "Succeeded"),
            PodStatus::Failed => write!(f, "Failed"),
            PodStatus::Unknown => write!(f, "Unknown"),
            PodStatus::ContainerCreating => write!(f, "ContainerCreating"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerStatus {
    pub name: String,
    pub container_id: Option<String>,
    pub image: String,
    pub state: ContainerState,
    pub ready: bool,
    pub restart_count: i32,
    pub ports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContainerState {
    Waiting { reason: String },
    Running { started_at: Option<DateTime<Utc>> },
    Terminated { reason: String, exit_code: i32 },
}

impl std::fmt::Display for ContainerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContainerState::Waiting { reason } => {
                write!(f, "Waiting\n      Reason:       {}", reason)
            }
            ContainerState::Running { started_at } => {
                if let Some(time) = started_at {
                    write!(
                        f,
                        "Running\n    Started:        {}",
                        time.format("%a, %d %b %Y %H:%M:%S +0000")
                    )
                } else {
                    write!(f, "Running")
                }
            }
            ContainerState::Terminated { reason, exit_code } => {
                write!(
                    f,
                    "Terminated\n      Reason:       {}\n      Exit Code:    {}",
                    reason, exit_code
                )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodEvent {
    pub event_type: String, // Normal, Warning
    pub reason: String,
    pub age: String,
    pub message: String,
    pub timestamp: Option<DateTime<Utc>>,
}

impl PodDescription {
    /// 从etcd数据构造PodDescription
    pub async fn from_etcd(
        config_manager: &ConfigManager,
        cluster_name: &str,
        pod_name: &str,
    ) -> Result<Option<Self>> {
        let etcd_manager = config_manager.get_etcd_manager();

        // 从etcd获取pod信息
        let pod_key = format!("/nokube/{}/pods/{}", cluster_name, pod_name);
        let pod_data = etcd_manager.get(pod_key).await?;

        if let Some(kv) = pod_data.first() {
            let pod_json = kv.value_str();
            // 解析基本pod信息
            let pod_info: serde_json::Value = serde_json::from_str(pod_json)?;

            // 获取节点信息
            let node_name = pod_info
                .get("node")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            // 获取节点IP
            let node_ip = if node_name != "unknown" {
                let node_key = format!("/nokube/{}/nodes/{}", cluster_name, node_name);
                let node_data = etcd_manager.get(node_key).await?;
                if let Some(node_kv) = node_data.first() {
                    let node_info: serde_json::Value = serde_json::from_str(node_kv.value_str())?;
                    node_info
                        .get("ip")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            } else {
                None
            };

            // 构造容器状态
            let containers = vec![ContainerStatus {
                name: pod_name.to_string(),
                container_id: pod_info
                    .get("container_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                image: pod_info
                    .get("image")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                state: match pod_info.get("status").and_then(|v| v.as_str()) {
                    Some("Running") => ContainerState::Running {
                        started_at: pod_info
                            .get("started_at")
                            .and_then(|v| v.as_str())
                            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                            .map(|dt| dt.with_timezone(&Utc)),
                    },
                    Some("ContainerCreating") => ContainerState::Waiting {
                        reason: "ContainerCreating".to_string(),
                    },
                    Some("Failed") => ContainerState::Terminated {
                        reason: "Error".to_string(),
                        exit_code: 1,
                    },
                    _ => ContainerState::Waiting {
                        reason: "Unknown".to_string(),
                    },
                },
                ready: pod_info
                    .get("ready")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                restart_count: pod_info
                    .get("restart_count")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32,
                ports: pod_info
                    .get("ports")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .map(|s| s.to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
            }];

            // 获取事件信息
            let events_key = format!("/nokube/{}/events/pod/{}", cluster_name, pod_name);
            let mut events = Vec::new();

            let events_data = etcd_manager.get(events_key).await?;
            if let Some(events_kv) = events_data.first() {
                let events_json: serde_json::Value = serde_json::from_str(events_kv.value_str())?;
                if let Some(events_array) = events_json.as_array() {
                    for event in events_array {
                        if let (Some(event_type), Some(reason), Some(message)) = (
                            event.get("type").and_then(|v| v.as_str()),
                            event.get("reason").and_then(|v| v.as_str()),
                            event.get("message").and_then(|v| v.as_str()),
                        ) {
                            events.push(PodEvent {
                                event_type: event_type.to_string(),
                                reason: reason.to_string(),
                                age: event
                                    .get("age")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                                    .to_string(),
                                message: message.to_string(),
                                timestamp: event
                                    .get("timestamp")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                                    .map(|dt| dt.with_timezone(&Utc)),
                            });
                        }
                    }
                }
            }

            // 解析状态
            let status = match pod_info.get("status").and_then(|v| v.as_str()) {
                Some("Running") => PodStatus::Running,
                Some("Pending") => PodStatus::Pending,
                Some("ContainerCreating") => PodStatus::ContainerCreating,
                Some("Failed") => PodStatus::Failed,
                Some("Succeeded") => PodStatus::Succeeded,
                _ => PodStatus::Unknown,
            };

            // 构造labels
            let mut labels = HashMap::new();
            if let Some(labels_map) = pod_info.get("labels").and_then(|v| v.as_object()) {
                for (key, value) in labels_map {
                    if let Some(value_str) = value.as_str() {
                        labels.insert(key.clone(), value_str.to_string());
                    }
                }
            }

            // 检查 actor alive 状态
            let actor_path_key = format!(
                "/nokube/{}/actors/{}/alive",
                cluster_name,
                pod_name.replace("/", "-")
            );

            let alive_status = match etcd_manager.get(actor_path_key).await {
                Ok(alive_data) if !alive_data.is_empty() => {
                    if let Some(alive_kv) = alive_data.first() {
                        let alive_json: Result<serde_json::Value, _> =
                            serde_json::from_str(alive_kv.value_str());
                        match alive_json {
                            Ok(alive_info) => {
                                // 检查是否已标记为 Dead
                                if let Some(status) =
                                    alive_info.get("status").and_then(|v| v.as_str())
                                {
                                    if status == "Dead" {
                                        AliveStatus::Dead
                                    } else {
                                        // 检查 last_alive 时间戳并结合租约计算新鲜度
                                        if let Some(last_alive_str) =
                                            alive_info.get("last_alive").and_then(|v| v.as_str())
                                        {
                                            if let Ok(last_alive) =
                                                DateTime::parse_from_rfc3339(last_alive_str)
                                            {
                                                let now = Utc::now();
                                                let diff = now.signed_duration_since(
                                                    last_alive.with_timezone(&Utc),
                                                );
                                                let lease_ttl = alive_info
                                                    .get("lease_ttl")
                                                    .and_then(|v| v.as_i64())
                                                    .unwrap_or(15)
                                                    .max(1);
                                                let freshness_cutoff = lease_ttl.saturating_mul(2);

                                                if diff.num_seconds() > freshness_cutoff {
                                                    AliveStatus::Dead
                                                } else {
                                                    AliveStatus::Alive
                                                }
                                            } else {
                                                AliveStatus::Unknown
                                            }
                                        } else {
                                            AliveStatus::Unknown
                                        }
                                    }
                                } else {
                                    AliveStatus::Unknown
                                }
                            }
                            Err(_) => AliveStatus::Unknown,
                        }
                    } else {
                        AliveStatus::Unknown
                    }
                }
                _ => AliveStatus::Unknown, // 没有 alive key 或查询失败
            };

            Ok(Some(PodDescription {
                name: pod_name.to_string(),
                namespace: pod_info
                    .get("namespace")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default")
                    .to_string(),
                priority: pod_info
                    .get("priority")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32,
                node: format!("{}/{}", node_name, node_ip.as_deref().unwrap_or("unknown")),
                node_ip,
                start_time: pod_info
                    .get("start_time")
                    .and_then(|v| v.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&Utc)),
                labels,
                status,
                ip: pod_info
                    .get("pod_ip")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                containers,
                events,
                alive_status,
            }))
        } else {
            Ok(None)
        }
    }

    /// 格式化显示pod描述信息
    pub fn display(&self) -> String {
        let mut output = String::new();

        output.push_str(&format!("Name:         {}\n", self.name));
        output.push_str(&format!("Namespace:    {}\n", self.namespace));
        output.push_str(&format!("Priority:     {}\n", self.priority));
        output.push_str(&format!("Node:         {}\n", self.node));

        if let Some(start_time) = &self.start_time {
            output.push_str(&format!(
                "Start Time:   {}\n",
                start_time.format("%a, %d %b %Y %H:%M:%S +0000")
            ));
        }

        // 显示labels
        if !self.labels.is_empty() {
            let labels_str: Vec<String> = self
                .labels
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect();
            output.push_str(&format!("Labels:       {}\n", labels_str.join(",")));
        }

        output.push_str(&format!("Status:       {}\n", self.status));
        output.push_str(&format!("Alive:        {}\n", self.alive_status));

        if let Some(ip) = &self.ip {
            output.push_str(&format!("IP:           {}\n", ip));
        } else {
            output.push_str("IP:           <none>\n");
        }

        // 显示容器信息
        output.push_str("Containers:\n");
        for container in &self.containers {
            output.push_str(&format!("  {}:\n", container.name));

            if let Some(container_id) = &container.container_id {
                output.push_str(&format!("    Container ID:   {}\n", container_id));
            } else {
                output.push_str("    Container ID:   <none>\n");
            }

            output.push_str(&format!("    Image:          {}\n", container.image));
            output.push_str(&format!("    State:          {}\n", container.state));
            output.push_str(&format!("    Ready:          {}\n", container.ready));
            output.push_str(&format!(
                "    Restart Count:  {}\n",
                container.restart_count
            ));

            if !container.ports.is_empty() {
                output.push_str(&format!(
                    "    Ports:          {}\n",
                    container.ports.join(",")
                ));
            }
        }

        // 显示事件
        if !self.events.is_empty() {
            output.push_str("Events:\n");
            for event in &self.events {
                output.push_str(&format!(
                    "  {}  {}           {}   {}\n",
                    event.event_type, event.reason, event.age, event.message
                ));
            }
        }

        output
    }
}

/// NodeAffinity配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeAffinity {
    pub required: Vec<NodeSelector>,
    pub preferred: Vec<PreferredNodeSelector>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSelector {
    pub match_expressions: Vec<NodeSelectorRequirement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSelectorRequirement {
    pub key: String,
    pub operator: String, // In, NotIn, Exists, DoesNotExist
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreferredNodeSelector {
    pub weight: i32,
    pub preference: NodeSelector,
}

/// 容器规格
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub command: Option<Vec<String>>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub volume_mounts: Option<Vec<VolumeMount>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeMount {
    pub name: String,
    pub mount_path: String,
    pub read_only: bool,
}

/// DaemonSetActor - 在nodeaffinity允许的所有节点上都部署一个实例
#[derive(Debug)]
pub struct DaemonSetActor {
    pub name: String,
    pub namespace: String,
    pub attribution_path: GlobalAttributionPath,
    pub node_affinity: NodeAffinity,
    pub container_spec: ContainerSpec,
    pub workspace: String,
    pub cluster_name: String, // 添加集群名称

    // 运行时状态
    pub pods: HashMap<String, PodActor>, // node_name -> pod
    // the_proxy连接
    pub proxy_tx: mpsc::Sender<ActorAliveRequest>,
    pub status: ActorState,
    pub config_manager: Arc<ConfigManager>, // 添加 ConfigManager
}

impl DaemonSetActor {
    pub fn new(
        name: String,
        namespace: String,
        attribution_path: GlobalAttributionPath,
        node_affinity: NodeAffinity,
        container_spec: ContainerSpec,
        workspace: String,
        cluster_name: String, // 添加集群名称参数
        proxy_tx: mpsc::Sender<ActorAliveRequest>,
        config_manager: Arc<ConfigManager>, // 添加 ConfigManager 参数
    ) -> Self {
        Self {
            name,
            namespace,
            attribution_path,
            node_affinity,
            container_spec,
            workspace,
            cluster_name,
            pods: HashMap::new(),
            proxy_tx,
            status: ActorState::Starting,
            config_manager,
        }
    }

    /// 获取匹配nodeaffinity的节点列表
    async fn get_matching_nodes(&self) -> Result<Vec<String>> {
        // 这里应该实现节点选择逻辑
        // 暂时返回模拟数据
        Ok(vec![
            "node1".to_string(),
            "node2".to_string(),
            "node3".to_string(),
        ])
    }

    /// 为每个匹配的节点创建Pod
    async fn ensure_pods(&mut self) -> Result<()> {
        let matching_nodes = self.get_matching_nodes().await?;

        for node_name in &matching_nodes {
            if !self.pods.contains_key(node_name) {
                // 动态附加节点名
                let pod_name = format!("{}-{}", self.name, node_name);
                let pod_attribution_path = self.attribution_path.child(&pod_name);

                let mut pod = PodActor::new(
                    pod_name,
                    self.namespace.clone(),
                    pod_attribution_path,
                    node_name.clone(),
                    self.container_spec.clone(),
                    self.workspace.clone(),
                    self.cluster_name.clone(), // 添加 cluster_name
                    self.proxy_tx.clone(),
                    self.config_manager.clone(), // 添加 config_manager
                );

                pod.start().await?;
                self.pods.insert(node_name.clone(), pod);
            }
        }

        // 清理不再需要的Pod
        let current_nodes: Vec<String> = self.pods.keys().cloned().collect();
        for node_name in current_nodes {
            if !matching_nodes.contains(&node_name) {
                if let Some(mut pod) = self.pods.remove(&node_name) {
                    pod.stop().await?;
                }
            }
        }

        Ok(())
    }

    /// 定期监控Pod状态
    async fn monitor_pods(&mut self) -> Result<()> {
        for (node_name, pod) in &self.pods {
            if !pod.health_check().await? {
                tracing::warn!("Pod {} on node {} is unhealthy", pod.name, node_name);
                // 可以在这里实现重启逻辑
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl AsyncActor for DaemonSetActor {
    fn actor_kind(&self) -> ActorKind {
        ActorKind::DaemonSet
    }

    fn attribution_path(&self) -> &GlobalAttributionPath {
        &self.attribution_path
    }

    async fn start(&mut self) -> Result<()> {
        tracing::info!("Starting DaemonSet: {}", self.name);

        self.status = ActorState::Starting;
        self.send_alive_signal().await?;

        // 确保Pod在所有匹配的节点上运行
        self.ensure_pods().await?;

        // 启动监控协程
        let attribution_path = self.attribution_path.clone();
        let proxy_tx = self.proxy_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                // 定期监控逻辑
            }
        });

        self.status = ActorState::Running;
        self.send_alive_signal().await?;

        tracing::info!("DaemonSet {} started successfully", self.name);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        tracing::info!("Stopping DaemonSet: {}", self.name);

        self.status = ActorState::Stopping;
        self.send_alive_signal().await?;

        // 停止所有Pod
        for (_, mut pod) in std::mem::take(&mut self.pods) {
            pod.stop().await?;
        }

        tracing::info!("DaemonSet {} stopped successfully", self.name);
        Ok(())
    }

    async fn update_config(&mut self) -> Result<()> {
        tracing::info!("Updating config for DaemonSet: {}", self.name);

        // 重新创建Pod以应用新配置
        for (_, pod) in &mut self.pods {
            pod.update_config().await?;
        }

        Ok(())
    }

    async fn health_check(&self) -> Result<bool> {
        // 检查所有Pod是否健康
        for (_, pod) in &self.pods {
            if !pod.health_check().await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn check(&self) -> Result<ActorActionPlan> {
        let mut plan = ActorActionPlan::default();

        for pod in self.pods.values() {
            plan.merge(pod.check().await?);
        }

        Ok(plan)
    }
}

impl DaemonSetActor {
    async fn send_alive_signal(&self) -> Result<()> {
        let request = ActorAliveRequest {
            actor_path: self.attribution_path.clone(),
            actor_type: "daemonset".to_string(),
            cluster_name: self.cluster_name.clone(),
            status: self.status.clone(),
            lease_ttl: 15,
        };

        if let Err(_) = self.proxy_tx.send(request).await {
            tracing::warn!("Failed to send alive signal for DaemonSet: {}", self.name);
        }

        Ok(())
    }
}

/// DeploymentActor - 根据nodeaffinity随机分发实例到若干节点
#[derive(Debug)]
pub struct DeploymentActor {
    pub name: String,
    pub namespace: String,
    pub attribution_path: GlobalAttributionPath,
    pub node_affinity: NodeAffinity,
    pub container_spec: ContainerSpec,
    pub workspace: String,
    pub cluster_name: String, // 添加集群名称
    pub replicas: i32,

    // 运行时状态
    pub pods: HashMap<String, PodActor>, // pod_name -> pod
    // the_proxy连接
    pub proxy_tx: mpsc::Sender<ActorAliveRequest>,
    pub status: ActorState,
    pub config_manager: Arc<ConfigManager>, // 添加 ConfigManager
}

impl DeploymentActor {
    pub fn new(
        name: String,
        namespace: String,
        attribution_path: GlobalAttributionPath,
        node_affinity: NodeAffinity,
        container_spec: ContainerSpec,
        workspace: String,
        cluster_name: String, // 添加集群名称参数
        replicas: i32,
        proxy_tx: mpsc::Sender<ActorAliveRequest>,
        config_manager: Arc<ConfigManager>, // 添加 ConfigManager 参数
    ) -> Self {
        Self {
            name,
            namespace,
            attribution_path,
            node_affinity,
            container_spec,
            workspace,
            cluster_name,
            replicas,
            pods: HashMap::new(),
            proxy_tx,
            status: ActorState::Starting,
            config_manager,
        }
    }

    /// 根据replica数量和nodeaffinity随机分发Pod
    async fn ensure_pods(&mut self) -> Result<()> {
        let matching_nodes = self.get_matching_nodes().await?;
        let current_pod_count = self.pods.len() as i32;

        // 如果Pod数量不足，创建新的Pod
        if current_pod_count < self.replicas {
            let needed = self.replicas - current_pod_count;

            for _i in 0..needed {
                // 随机选择节点
                use rand::seq::SliceRandom;
                use rand::SeedableRng;
                let mut rng = rand::rngs::SmallRng::from_entropy();
                if let Some(node_name) = matching_nodes.choose(&mut rng) {
                    // 动态附加名字后缀
                    let pod_name = format!(
                        "{}-{}",
                        self.name,
                        uuid::Uuid::new_v4().to_string()[..8].to_string()
                    );
                    let pod_attribution_path = self.attribution_path.child(&pod_name);

                    let mut pod = PodActor::new(
                        pod_name.clone(),
                        self.namespace.clone(),
                        pod_attribution_path,
                        node_name.clone(),
                        self.container_spec.clone(),
                        self.workspace.clone(),
                        self.cluster_name.clone(), // 添加 cluster_name
                        self.proxy_tx.clone(),
                        self.config_manager.clone(), // 添加 config_manager
                    );

                    pod.start().await?;
                    self.pods.insert(pod_name, pod);
                }
            }
        }

        // 如果Pod数量过多，删除多余的Pod
        if current_pod_count > self.replicas {
            let excess = current_pod_count - self.replicas;
            let mut pods_to_remove = Vec::new();

            for (pod_name, _) in self.pods.iter().take(excess as usize) {
                pods_to_remove.push(pod_name.clone());
            }

            for pod_name in pods_to_remove {
                if let Some(mut pod) = self.pods.remove(&pod_name) {
                    pod.stop().await?;
                }
            }
        }

        Ok(())
    }

    async fn get_matching_nodes(&self) -> Result<Vec<String>> {
        // 实现节点选择逻辑
        Ok(vec![
            "node1".to_string(),
            "node2".to_string(),
            "node3".to_string(),
        ])
    }
}

#[async_trait::async_trait]
impl AsyncActor for DeploymentActor {
    fn actor_kind(&self) -> ActorKind {
        ActorKind::Deployment
    }

    fn attribution_path(&self) -> &GlobalAttributionPath {
        &self.attribution_path
    }

    async fn start(&mut self) -> Result<()> {
        tracing::info!("Starting Deployment: {}", self.name);

        self.status = ActorState::Starting;
        self.send_alive_signal().await?;

        self.ensure_pods().await?;

        self.status = ActorState::Running;
        self.send_alive_signal().await?;

        tracing::info!("Deployment {} started successfully", self.name);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        tracing::info!("Stopping Deployment: {}", self.name);

        self.status = ActorState::Stopping;
        self.send_alive_signal().await?;

        for (_, mut pod) in std::mem::take(&mut self.pods) {
            pod.stop().await?;
        }

        tracing::info!("Deployment {} stopped successfully", self.name);
        Ok(())
    }

    async fn update_config(&mut self) -> Result<()> {
        tracing::info!("Updating config for Deployment: {}", self.name);

        for (_, pod) in &mut self.pods {
            pod.update_config().await?;
        }

        Ok(())
    }

    async fn health_check(&self) -> Result<bool> {
        for (_, pod) in &self.pods {
            if !pod.health_check().await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn check(&self) -> Result<ActorActionPlan> {
        let mut plan = ActorActionPlan::default();

        for pod in self.pods.values() {
            plan.merge(pod.check().await?);
        }

        Ok(plan)
    }
}

impl DeploymentActor {
    async fn send_alive_signal(&self) -> Result<()> {
        let request = ActorAliveRequest {
            actor_path: self.attribution_path.clone(),
            actor_type: "deployment".to_string(),
            cluster_name: self.cluster_name.clone(),
            status: self.status.clone(),
            lease_ttl: 15,
        };

        if let Err(_) = self.proxy_tx.send(request).await {
            tracing::warn!("Failed to send alive signal for Deployment: {}", self.name);
        }

        Ok(())
    }
}

/// PodActor - 循环运行其关联的docker容器
#[derive(Debug)]
pub struct PodActor {
    pub name: String,
    pub namespace: String,
    pub attribution_path: GlobalAttributionPath,
    pub node_name: String,
    pub container_spec: ContainerSpec,
    pub workspace: String,
    pub cluster_name: String, // 添加集群名称

    // 运行时状态
    pub container_id: Option<String>,
    // the_proxy连接
    pub proxy_tx: mpsc::Sender<ActorAliveRequest>,
    pub status: ActorState,

    // 添加 ConfigManager 引用以写入 etcd
    pub config_manager: Arc<ConfigManager>,
}

impl PodActor {
    pub fn new(
        name: String,
        namespace: String,
        attribution_path: GlobalAttributionPath,
        node_name: String,
        container_spec: ContainerSpec,
        workspace: String,
        cluster_name: String, // 添加集群名称参数
        proxy_tx: mpsc::Sender<ActorAliveRequest>,
        config_manager: Arc<ConfigManager>,
    ) -> Self {
        Self {
            name,
            namespace,
            attribution_path,
            node_name,
            container_spec,
            workspace,
            cluster_name,
            container_id: None,
            proxy_tx,
            status: ActorState::Starting,
            config_manager,
        }
    }

    /// 将 pod 状态保存到 etcd
    async fn save_to_etcd(&self) -> Result<()> {
        let etcd_manager = self.config_manager.get_etcd_manager();
        let pod_key = format!("/nokube/{}/pods/{}", self.cluster_name, self.name);

        // 构建 pod 信息 JSON
        let pod_info = serde_json::json!({
            "name": self.name,
            "namespace": self.namespace,
            "node": self.node_name,
            "image": self.container_spec.image,
            "container_id": self.container_id,
            "status": match self.status {
                ActorState::Starting => "Pending",
                ActorState::Running => "Running",
                ActorState::Stopping => "Terminating",
                ActorState::Failed => "Failed",
                ActorState::Stopped => "Failed",
            },
            "ready": matches!(self.status, ActorState::Running),
            "restart_count": 0,
            "start_time": chrono::Utc::now().to_rfc3339(),
            "pod_ip": match &self.container_id {
                Some(_) => serde_json::Value::String("172.17.0.5".to_string()), // 模拟 IP
                None => serde_json::Value::Null,
            },
            "labels": {
                "app": self.name,
                "role": "pod"
            },
            "ports": self.container_spec.command.as_ref()
                .map(|cmd| if cmd.iter().any(|c| c.contains("port")) {
                    vec!["8080/TCP".to_string()]
                } else {
                    Vec::<String>::new()
                })
                .unwrap_or_default(),
            "priority": 0
        });

        etcd_manager.put(pod_key, pod_info.to_string()).await?;
        tracing::info!(
            "Saved pod {} to etcd for cluster {}",
            self.name,
            self.cluster_name
        );

        // 同时保存事件信息
        self.save_events_to_etcd().await?;

        Ok(())
    }

    /// 将 pod 事件保存到 etcd
    async fn save_events_to_etcd(&self) -> Result<()> {
        let etcd_manager = self.config_manager.get_etcd_manager();
        let events_key = format!("/nokube/{}/events/pod/{}", self.cluster_name, self.name);

        let events = match self.status {
            ActorState::Starting => vec![
                serde_json::json!({
                    "type": "Normal",
                    "reason": "Scheduled",
                    "message": format!("Successfully assigned {}/{} to {}", self.namespace, self.name, self.node_name),
                    "age": "1m",
                    "timestamp": chrono::Utc::now().to_rfc3339()
                }),
                serde_json::json!({
                    "type": "Normal",
                    "reason": "Pulling",
                    "message": format!("Pulling image \"{}\"", self.container_spec.image),
                    "age": "30s",
                    "timestamp": chrono::Utc::now().to_rfc3339()
                }),
            ],
            ActorState::Running => vec![
                serde_json::json!({
                    "type": "Normal",
                    "reason": "Scheduled",
                    "message": format!("Successfully assigned {}/{} to {}", self.namespace, self.name, self.node_name),
                    "age": "2m",
                    "timestamp": chrono::Utc::now().to_rfc3339()
                }),
                serde_json::json!({
                    "type": "Normal",
                    "reason": "Pulled",
                    "message": format!("Successfully pulled image \"{}\"", self.container_spec.image),
                    "age": "1m",
                    "timestamp": chrono::Utc::now().to_rfc3339()
                }),
                serde_json::json!({
                    "type": "Normal",
                    "reason": "Created",
                    "message": format!("Created container {}", self.name),
                    "age": "1m",
                    "timestamp": chrono::Utc::now().to_rfc3339()
                }),
                serde_json::json!({
                    "type": "Normal",
                    "reason": "Started",
                    "message": format!("Started container {}", self.name),
                    "age": "1m",
                    "timestamp": chrono::Utc::now().to_rfc3339()
                }),
            ],
            _ => vec![serde_json::json!({
                "type": "Warning",
                "reason": "FailedMount",
                "message": "Failed to mount volumes",
                "age": "1m",
                "timestamp": chrono::Utc::now().to_rfc3339()
            })],
        };

        etcd_manager
            .put(events_key, serde_json::Value::Array(events).to_string())
            .await?;
        tracing::info!(
            "Saved pod {} events to etcd for cluster {}",
            self.name,
            self.cluster_name
        );

        Ok(())
    }

    /// 准备挂载的config和secret
    async fn prepare_mounts(&self) -> Result<()> {
        let pod_workspace = self.attribution_path.workspace_path(&self.workspace);
        std::fs::create_dir_all(&pod_workspace)?;

        // 创建config目录
        let config_dir = format!("{}/config", pod_workspace);
        std::fs::create_dir_all(&config_dir)?;

        // 创建secret目录
        let secret_dir = format!("{}/secret", pod_workspace);
        std::fs::create_dir_all(&secret_dir)?;

        tracing::info!("Prepared mount directories for Pod: {}", self.name);
        Ok(())
    }

    /// 启动Docker容器（模拟 - 实际容器管理由ServiceModeAgent的ProcessManager处理）
    async fn start_container(&mut self) -> Result<()> {
        self.prepare_mounts().await?;

        // 模拟容器ID，实际容器由ServiceModeAgent管理
        let container_name = format!("nokube-pod-{}", self.name);
        self.container_id = Some(format!(
            "simulated-{}-{}",
            container_name,
            uuid::Uuid::new_v4().to_string()[..8].to_string()
        ));

        tracing::info!(
            "Pod {} container management delegated to ServiceModeAgent",
            self.name
        );
        tracing::info!("Expected container name: {}", container_name);
        tracing::info!(
            "Container {} logs will be collected by ServiceModeAgent LogCollector",
            container_name
        );

        Ok(())
    }
}

#[async_trait::async_trait]
impl ActorOrphanCleanup for PodActor {
    async fn cleanup_if_orphaned(&self) -> anyhow::Result<()> {
        let plan = AsyncActor::check(self).await?;

        if plan.is_empty() {
            return Ok(());
        }

        let etcd = self.config_manager.get_etcd_manager();

        for action in plan.containers_to_stop {
            tracing::info!(
                "KubeController orphan cleanup: stopping container '{}' for pod {} (reason: {})",
                action.name,
                self.name,
                action
                    .reason
                    .as_deref()
                    .unwrap_or("plan generated without reason")
            );
            let _ = crate::agent::general::DockerRunner::stop(&action.name);
            let _ = crate::agent::general::DockerRunner::remove_container(&action.name);
        }

        for key in plan.etcd_keys_to_delete {
            if let Err(e) = etcd.delete(key.clone()).await {
                tracing::warn!(
                    "KubeController orphan cleanup: failed to delete etcd key {}: {}",
                    key,
                    e
                );
            }
        }

        for signal in plan.signals_to_emit {
            tracing::info!(
                "KubeController orphan cleanup: pending signal '{}' for {} (not yet implemented)",
                signal.signal,
                signal.target_path
            );
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl AsyncActor for PodActor {
    fn actor_kind(&self) -> ActorKind {
        ActorKind::Pod
    }

    fn attribution_path(&self) -> &GlobalAttributionPath {
        &self.attribution_path
    }

    async fn start(&mut self) -> Result<()> {
        tracing::info!("Starting Pod: {}", self.name);

        self.status = ActorState::Starting;
        self.send_alive_signal().await?;

        // 保存 Starting 状态到 etcd
        self.save_to_etcd().await?;

        self.start_container().await?;

        self.status = ActorState::Running;
        self.send_alive_signal().await?;

        // 保存 Running 状态到 etcd
        self.save_to_etcd().await?;

        tracing::info!("Pod {} started successfully", self.name);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        tracing::info!("Stopping Pod: {}", self.name);

        self.status = ActorState::Stopping;
        self.send_alive_signal().await?;

        if let Some(container_id) = &self.container_id {
            tracing::info!(
                "Pod {} container stopping delegated to ServiceModeAgent",
                self.name
            );
            tracing::info!("Container ID: {}", container_id);
        }

        self.container_id = None;
        tracing::info!("Pod {} stopped successfully", self.name);
        Ok(())
    }

    async fn update_config(&mut self) -> Result<()> {
        tracing::info!("Updating config for Pod: {}", self.name);

        // 重启容器以应用新配置
        self.stop().await?;
        self.start().await?;

        Ok(())
    }

    async fn health_check(&self) -> Result<bool> {
        // 简化的健康检查 - 如果有container_id就认为是运行中
        // 实际的健康检查应该由ServiceModeAgent的ProcessManager处理
        if let Some(_container_id) = &self.container_id {
            // 模拟健康检查 - 实际应该检查ServiceModeAgent管理的容器状态
            return Ok(true);
        }

        Ok(false)
    }

    async fn check(&self) -> Result<ActorActionPlan> {
        let mut plan = ActorActionPlan::default();

        if let Some(parent) = self.attribution_path.parent() {
            let parts: Vec<&str> = parent.path.split('/').collect();
            if parts.len() >= 2 {
                let actor_type = parts[0];
                let actor_name = parts[1];
                let etcd = self.config_manager.get_etcd_manager();

                let etcd_key = match actor_type {
                    "deployment" => Some(format!(
                        "/nokube/{}/deployments/{}",
                        self.cluster_name, actor_name
                    )),
                    "daemonset" => Some(format!(
                        "/nokube/{}/daemonsets/{}",
                        self.cluster_name, actor_name
                    )),
                    _ => None,
                };

                if let Some(key) = etcd_key {
                    match etcd.get(key.clone()).await {
                        Ok(kvs) if kvs.is_empty() => {
                            let container_name = format!("nokube-pod-{}", self.name);
                            plan.containers_to_stop.push(ContainerAction {
                                name: container_name,
                                reason: Some(format!(
                                    "parent actor {} missing (etcd key: {})",
                                    parent.path, key
                                )),
                            });
                            plan.etcd_keys_to_delete
                                .push(format!("/nokube/{}/pods/{}", self.cluster_name, self.name));
                            plan.etcd_keys_to_delete.push(format!(
                                "/nokube/{}/events/pod/{}",
                                self.cluster_name, self.name
                            ));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                "Pod {} check: failed to read parent key {}: {}",
                                self.name,
                                key,
                                e
                            );
                        }
                    }
                }
            }
        }

        Ok(plan)
    }
}

impl PodActor {
    async fn send_alive_signal(&self) -> Result<()> {
        let request = ActorAliveRequest {
            actor_path: self.attribution_path.clone(),
            actor_type: "pod".to_string(),
            cluster_name: self.cluster_name.clone(),
            status: self.status.clone(),
            lease_ttl: 15, // 与设计的 30s Freshness 对齐
        };

        if let Err(_) = self.proxy_tx.send(request).await {
            tracing::warn!("Failed to send alive signal for Pod: {}", self.name);
        }

        Ok(())
    }
}
