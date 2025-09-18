use super::docker_runner::DockerRunner;
use crate::agent::runtime::deployment::POD_CONTAINER_SEPARATOR;
use crate::config::{cluster_config::ClusterConfig, etcd_manager::EtcdManager};
use crate::k8s::controllers::ContainerEvent;
use anyhow::Result;
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::process::Command as StdCommand;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::Duration;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub timestamp: u64,
    pub cpu_usage: f64,
    pub cpu_cores: u64,
    pub memory_usage: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
    pub node_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ActorLevel {
    Root,
    Pod,
}

impl ActorLevel {
    fn as_str(&self) -> &'static str {
        match self {
            ActorLevel::Root => "root",
            ActorLevel::Pod => "pod",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ActorIdentifier {
    name: String,
    level: ActorLevel,
}

impl ActorIdentifier {
    fn root(name: String) -> Self {
        Self {
            name,
            level: ActorLevel::Root,
        }
    }

    fn pod(name: String) -> Self {
        Self {
            name,
            level: ActorLevel::Pod,
        }
    }
}

#[derive(Debug, Clone)]
struct ActorMetadata {
    root_actor: String,
    owner_type: String,
    parent_actor: Option<String>,
    pod_name: Option<String>,
    container_name: Option<String>,
    container_path: Option<String>,
}

impl ActorMetadata {
    fn for_root(root_actor: String, owner_type: String) -> Self {
        Self {
            root_actor: root_actor.clone(),
            owner_type,
            parent_actor: None,
            pod_name: None,
            container_name: None,
            container_path: None,
        }
    }

    fn for_pod(
        root_actor: String,
        owner_type: String,
        pod_name: String,
        container_name: String,
        container_path: String,
    ) -> Self {
        Self {
            root_actor: root_actor.clone(),
            owner_type,
            parent_actor: Some(root_actor),
            pod_name: Some(pod_name),
            container_name: Some(container_name),
            container_path: Some(container_path),
        }
    }

    fn fallback(id: &ActorIdentifier) -> Self {
        Self {
            root_actor: id.name.clone(),
            owner_type: "unknown".to_string(),
            parent_actor: None,
            pod_name: None,
            container_name: None,
            container_path: None,
        }
    }
}

#[derive(Debug, Clone)]
struct ActorLifecycleEvent {
    id: ActorIdentifier,
    metadata: ActorMetadata,
}

#[derive(Debug, Default, Clone)]
struct ActorLifecycleBatch {
    starts: Vec<ActorLifecycleEvent>,
    stops: Vec<ActorLifecycleEvent>,
}

pub struct Exporter {
    client: Client,
    node_id: String,
    cluster_name: String,
    etcd_manager: Arc<EtcdManager>,
    current_config: Arc<RwLock<Option<ClusterConfig>>>,
    // Tracks last-seen timestamps per container so we can emit a single tombstone when it disappears.
    container_tracker: Arc<RwLock<HashMap<String, u64>>>,
    pending_tombstones: Arc<Mutex<HashSet<String>>>,
    actor_tracker: Arc<RwLock<HashMap<ActorIdentifier, u64>>>,
    pending_actor_tombstones: Arc<Mutex<HashSet<ActorIdentifier>>>,
    actor_metadata: Arc<RwLock<HashMap<ActorIdentifier, ActorMetadata>>>,
}

impl Exporter {
    pub fn new(node_id: String, cluster_name: String, etcd_manager: Arc<EtcdManager>) -> Self {
        Self {
            client: Client::new(),
            node_id,
            cluster_name,
            etcd_manager,
            current_config: Arc::new(RwLock::new(None)),
            container_tracker: Arc::new(RwLock::new(HashMap::new())),
            pending_tombstones: Arc::new(Mutex::new(HashSet::new())),
            actor_tracker: Arc::new(RwLock::new(HashMap::new())),
            pending_actor_tombstones: Arc::new(Mutex::new(HashSet::new())),
            actor_metadata: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn start_with_etcd_polling(
        &self,
        container_events: Option<mpsc::Receiver<ContainerEvent>>,
    ) -> Result<()> {
        let config_poller = self.start_config_polling().await?;
        let metrics_collector = self.start_metrics_collection().await?;
        if let Some(events) = container_events {
            let event_listener = self.start_event_listener(events).await?;
            tokio::try_join!(config_poller, metrics_collector, event_listener)?;
        } else {
            tokio::try_join!(config_poller, metrics_collector)?;
        }
        Ok(())
    }

    async fn start_event_listener(
        &self,
        mut container_events: mpsc::Receiver<ContainerEvent>,
    ) -> Result<tokio::task::JoinHandle<Result<()>>> {
        let tracker = Arc::clone(&self.container_tracker);
        let pending = Arc::clone(&self.pending_tombstones);
        let actor_pending = Arc::clone(&self.pending_actor_tombstones);
        let node_id = self.node_id.clone();
        let cluster_name = self.cluster_name.clone();

        let handle = tokio::spawn(async move {
            while let Some(event) = container_events.recv().await {
                match event {
                    ContainerEvent::Started { name } => {
                        let (pod_name, _container_segment, core_actor, _owner_type) =
                            Exporter::derive_actor_core(&name, &node_id, &cluster_name);
                        let canonical_root = format!("{}-{}", core_actor, cluster_name);
                        let root_id = ActorIdentifier::root(canonical_root.clone());
                        let pod_id = ActorIdentifier::pod(pod_name.clone());
                        {
                            let mut pending_guard = pending.lock().await;
                            pending_guard.remove(&name);
                        }
                        {
                            let mut actor_pending_guard = actor_pending.lock().await;
                            actor_pending_guard.remove(&root_id);
                            actor_pending_guard.remove(&pod_id);
                        }
                        let ts = Utc::now().timestamp().max(0) as u64;
                        let mut tracker_guard = tracker.write().await;
                        tracker_guard.insert(name, ts);
                    }
                    ContainerEvent::Stopped { name } => {
                        let (pod_name, _container_segment, core_actor, _owner_type) =
                            Exporter::derive_actor_core(&name, &node_id, &cluster_name);
                        let canonical_root = format!("{}-{}", core_actor, cluster_name);
                        let root_id = ActorIdentifier::root(canonical_root.clone());
                        let pod_id = ActorIdentifier::pod(pod_name.clone());
                        {
                            let mut pending_guard = pending.lock().await;
                            pending_guard.insert(name.clone());
                        }
                        {
                            let mut actor_pending_guard = actor_pending.lock().await;
                            actor_pending_guard.insert(root_id);
                            actor_pending_guard.insert(pod_id);
                        }
                        let mut tracker_guard = tracker.write().await;
                        tracker_guard.remove(&name);
                    }
                }
            }

            Ok(())
        });

        Ok(handle)
    }

    async fn start_config_polling(&self) -> Result<tokio::task::JoinHandle<Result<()>>> {
        let etcd_manager = Arc::clone(&self.etcd_manager);
        let cluster_name = self.cluster_name.clone();
        let current_config = Arc::clone(&self.current_config);

        let handle = tokio::spawn(async move {
            loop {
                match etcd_manager.get_cluster_config(&cluster_name).await {
                    Ok(Some(config)) => {
                        let mut config_guard = current_config.write().await;
                        *config_guard = Some(config.clone());

                        let poll_interval = config.nokube_config.config_poll_interval.unwrap_or(10);
                        drop(config_guard);

                        tokio::time::sleep(Duration::from_secs(poll_interval)).await;
                    }
                    Ok(None) => {
                        tracing::warn!("No config found for cluster: {}", cluster_name);
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                    Err(e) => {
                        tracing::error!("Failed to get cluster config: {}", e);
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                }
            }
        });

        Ok(handle)
    }

    async fn start_metrics_collection(&self) -> Result<tokio::task::JoinHandle<Result<()>>> {
        let current_config = Arc::clone(&self.current_config);
        let client = self.client.clone();
        let node_id = self.node_id.clone();
        let container_tracker = Arc::clone(&self.container_tracker);
        let pending_tombstones = Arc::clone(&self.pending_tombstones);
        let actor_tracker = Arc::clone(&self.actor_tracker);
        let pending_actor_tombstones = Arc::clone(&self.pending_actor_tombstones);
        let actor_metadata = Arc::clone(&self.actor_metadata);

        let handle = tokio::spawn(async move {
            loop {
                let config_guard = current_config.read().await;
                if let Some(config) = config_guard.as_ref() {
                    if config.task_spec.monitoring.enabled {
                        let interval_seconds = config.nokube_config.metrics_interval.unwrap_or(30);
                        // 直接使用节点列表中的地址进行推送，而不需要额外的配置

                        // 查找启用了greptimedb的节点作为推送目标
                        if let Some(head_node) = config.nodes.iter().find(|n| {
                            matches!(n.role, crate::config::cluster_config::NodeRole::Head)
                        }) {
                            let greptimedb_url = format!(
                                "http://{}:{}",
                                head_node
                                    .get_ip()
                                    .map_err(|e| anyhow::anyhow!("Failed to get node IP: {}", e))?,
                                config.task_spec.monitoring.greptimedb.port
                            );

                            match Self::collect_system_metrics(&node_id).await {
                                Ok(metrics) => {
                                    info!("📊 Metrics collected for node '{}': CPU: {:.1}%, Memory: {:.1}%, RX: {} bytes, TX: {} bytes", 
                                          node_id, metrics.cpu_usage, metrics.memory_usage, metrics.network_rx_bytes, metrics.network_tx_bytes);

                                    // 收集 actor（容器）级别指标
                                    let containers = Self::list_actor_containers();
                                    let mut container_metrics = Vec::new();
                                    let mut alive_containers = HashSet::new();
                                    let mut alive_actors: HashSet<ActorIdentifier> = HashSet::new();
                                    let mut actor_infos: HashMap<ActorIdentifier, ActorMetadata> =
                                        HashMap::new();
                                    for name in containers {
                                        alive_containers.insert(name.clone());
                                        let (pod_name, container_segment, core_actor, owner_type) =
                                            Self::derive_actor_core(
                                                &name,
                                                &node_id,
                                                &config.cluster_name,
                                            );
                                        let canonical_root =
                                            format!("{}-{}", core_actor, config.cluster_name);
                                        let root_id = ActorIdentifier::root(canonical_root.clone());
                                        alive_actors.insert(root_id.clone());
                                        actor_infos.entry(root_id).or_insert_with(|| {
                                            ActorMetadata::for_root(
                                                canonical_root.clone(),
                                                owner_type.clone(),
                                            )
                                        });

                                        let pod_id = ActorIdentifier::pod(pod_name.clone());
                                        alive_actors.insert(pod_id.clone());
                                        let container_path = format!(
                                            "{}/{}/{}/{}",
                                            config.cluster_name,
                                            core_actor,
                                            pod_name,
                                            container_segment
                                        );
                                        actor_infos.entry(pod_id).or_insert_with(|| {
                                            ActorMetadata::for_pod(
                                                canonical_root.clone(),
                                                owner_type.clone(),
                                                pod_name.clone(),
                                                name.clone(),
                                                container_path.clone(),
                                            )
                                        });

                                        match Self::collect_container_metrics(&name) {
                                            Ok(metrics) => container_metrics.push((name, metrics)),
                                            Err(e) => warn!(
                                                "Failed to collect metrics for container {}: {}",
                                                name, e
                                            ),
                                        }
                                    }

                                    {
                                        let mut meta_guard = actor_metadata.write().await;
                                        for (id, meta) in &actor_infos {
                                            meta_guard.insert(id.clone(), meta.clone());
                                        }
                                    }

                                    let event_tombstones = {
                                        let mut pending = pending_tombstones.lock().await;
                                        for name in &alive_containers {
                                            pending.remove(name);
                                        }
                                        pending.drain().collect::<Vec<_>>()
                                    };

                                    let grace_secs = interval_seconds.saturating_mul(2).max(30);
                                    let time_based_exits = {
                                        let mut tracker = container_tracker.write().await;
                                        let now = metrics.timestamp;
                                        for name in &alive_containers {
                                            tracker.insert(name.clone(), now);
                                        }
                                        let mut exited = Vec::new();
                                        tracker.retain(|name, last_seen| {
                                            if alive_containers.contains(name) {
                                                true
                                            } else if now.saturating_sub(*last_seen) >= grace_secs {
                                                exited.push(name.clone());
                                                false
                                            } else {
                                                true
                                            }
                                        });
                                        exited
                                    };

                                    let mut exited_set: HashSet<String> =
                                        event_tombstones.into_iter().collect();
                                    for name in time_based_exits {
                                        exited_set.insert(name);
                                    }
                                    let exited_containers =
                                        exited_set.into_iter().collect::<Vec<_>>();

                                    let actor_events = {
                                        let actor_tombstone_set: HashSet<ActorIdentifier> = {
                                            let mut pending = pending_actor_tombstones.lock().await;
                                            for actor in &alive_actors {
                                                pending.remove(actor);
                                            }
                                            pending.drain().collect()
                                        };

                                        let mut start_ids = Vec::new();
                                        let mut stop_ids = Vec::new();
                                        {
                                            let mut tracker = actor_tracker.write().await;
                                            let now = metrics.timestamp;
                                            for actor in &alive_actors {
                                                if !tracker.contains_key(actor) {
                                                    start_ids.push(actor.clone());
                                                }
                                                tracker.insert(actor.clone(), now);
                                            }

                                            let snapshot: Vec<(ActorIdentifier, u64)> = tracker
                                                .iter()
                                                .map(|(id, ts)| (id.clone(), *ts))
                                                .collect();
                                            for (actor_id, last_seen) in snapshot {
                                                if !alive_actors.contains(&actor_id) {
                                                    if actor_tombstone_set.contains(&actor_id)
                                                        || now.saturating_sub(last_seen)
                                                            >= grace_secs
                                                    {
                                                        stop_ids.push(actor_id.clone());
                                                        tracker.remove(&actor_id);
                                                    }
                                                }
                                            }
                                        }

                                        let mut batch = ActorLifecycleBatch::default();
                                        {
                                            let meta_guard = actor_metadata.read().await;
                                            for actor_id in start_ids {
                                                let metadata = actor_infos
                                                    .get(&actor_id)
                                                    .cloned()
                                                    .or_else(|| meta_guard.get(&actor_id).cloned())
                                                    .unwrap_or_else(|| {
                                                        ActorMetadata::fallback(&actor_id)
                                                    });
                                                batch.starts.push(ActorLifecycleEvent {
                                                    id: actor_id,
                                                    metadata,
                                                });
                                            }
                                            for actor_id in stop_ids {
                                                let metadata = meta_guard
                                                    .get(&actor_id)
                                                    .cloned()
                                                    .unwrap_or_else(|| {
                                                        ActorMetadata::fallback(&actor_id)
                                                    });
                                                batch.stops.push(ActorLifecycleEvent {
                                                    id: actor_id,
                                                    metadata,
                                                });
                                            }
                                        }

                                        batch
                                    };

                                    let actor_statuses: Vec<ActorLifecycleEvent> = {
                                        let meta_guard = actor_metadata.read().await;
                                        alive_actors
                                            .iter()
                                            .map(|actor_id| {
                                                let metadata = actor_infos
                                                    .get(actor_id)
                                                    .cloned()
                                                    .or_else(|| meta_guard.get(actor_id).cloned())
                                                    .unwrap_or_else(|| {
                                                        ActorMetadata::fallback(actor_id)
                                                    });
                                                ActorLifecycleEvent {
                                                    id: actor_id.clone(),
                                                    metadata,
                                                }
                                            })
                                            .collect()
                                    };

                                    if let Err(e) = Self::push_to_greptimedb_all(
                                        &client,
                                        &greptimedb_url,
                                        &metrics,
                                        &node_id,
                                        &container_metrics,
                                        &config.cluster_name,
                                        &exited_containers,
                                        &actor_statuses,
                                        &actor_events,
                                    )
                                    .await
                                    {
                                        tracing::error!(
                                            "Failed to push metrics to GreptimeDB URL {}: {}",
                                            greptimedb_url,
                                            e
                                        );
                                    } else {
                                        info!("✅ Successfully pushed metrics to GreptimeDB for node '{}'", node_id);
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Failed to collect metrics: {}", e);
                                }
                            }

                            tokio::time::sleep(Duration::from_secs(interval_seconds)).await;
                        } else {
                            tracing::warn!("No head node found for metrics collection");
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                    } else {
                        drop(config_guard);
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                } else {
                    drop(config_guard);
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        });

        Ok(handle)
    }

    /// 列出由 NoKube 创建的 actor 容器名称
    fn list_actor_containers() -> Vec<String> {
        let docker_path = DockerRunner::get_runtime_path().unwrap_or_else(|_| "docker".to_string());
        let output = StdCommand::new(docker_path)
            .args(["ps", "--format", "{{.Names}}"])
            .output();
        if let Ok(out) = output {
            let text = String::from_utf8_lossy(&out.stdout);
            return text
                .lines()
                .map(|s| s.trim())
                .filter(|n| !n.is_empty())
                // 支持 actor 容器（nokube-pod-*）以及平台内置服务容器（nokube-*, 如 grafana/greptimedb/httpserver），以及含 gitops 的容器
                .filter(|n| {
                    n.starts_with("nokube-pod-") || n.starts_with("nokube-") || n.contains("gitops")
                })
                .map(|s| s.to_string())
                .collect();
        }
        Vec::new()
    }

    /// 收集单个容器的 CPU/内存 指标
    fn collect_container_metrics(container: &str) -> Result<(f64, u64, f64)> {
        let docker_path = DockerRunner::get_runtime_path().unwrap_or_else(|_| "docker".to_string());
        let out = StdCommand::new(docker_path)
            .args([
                "stats",
                "--no-stream",
                "--format",
                "{{.CPUPerc}}\t{{.MemUsage}}\t{{.MemPerc}}",
                container,
            ])
            .output()?;
        if !out.status.success() {
            anyhow::bail!("docker stats failed for {}", container);
        }
        let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if line.is_empty() {
            anyhow::bail!("empty stats for {}", container);
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            anyhow::bail!("unexpected stats format: {}", line);
        }
        let cpu = parts[0]
            .trim()
            .trim_end_matches('%')
            .replace(',', ".")
            .parse::<f64>()
            .unwrap_or(0.0);
        let mem_usage_str = parts[1]
            .trim()
            .split('/')
            .next()
            .unwrap_or(parts[1].trim())
            .trim();
        let mem_bytes = Self::parse_size_to_bytes(mem_usage_str);
        let mem_pct = parts[2]
            .trim()
            .trim_end_matches('%')
            .replace(',', ".")
            .parse::<f64>()
            .unwrap_or(0.0);
        Ok((cpu, mem_bytes, mem_pct))
    }

    /// 从容器名推断 pod 名、顶层 actor 名和所有者类型（deployment/daemonset）
    pub fn derive_actor_core(
        container: &str,
        node: &str,
        cluster: &str,
    ) -> (String, String, String, String) {
        let stripped = container.strip_prefix("nokube-pod-").unwrap_or(container);
        let (pod_segment, container_segment) = if let Some((pod_part, container_part)) =
            stripped.split_once(POD_CONTAINER_SEPARATOR)
        {
            (pod_part.to_string(), container_part.to_string())
        } else {
            (stripped.to_string(), "main".to_string())
        };

        let mut core = pod_segment.clone();
        let ds_suffix = format!("-{}", node);
        if core.ends_with(&ds_suffix) {
            core = core
                .trim_end_matches(&ds_suffix)
                .trim_end_matches('-')
                .to_string();
            let cluster_suffix = format!("-{}", cluster);
            if core.ends_with(&cluster_suffix) {
                core = core
                    .trim_end_matches(&cluster_suffix)
                    .trim_end_matches('-')
                    .to_string();
            }
            return (
                pod_segment,
                container_segment,
                core,
                "daemonset".to_string(),
            );
        }

        let cluster_suffix = format!("-{}", cluster);
        if core.ends_with(&cluster_suffix) {
            core = core
                .trim_end_matches(&cluster_suffix)
                .trim_end_matches('-')
                .to_string();
        }

        if let Some((b, last)) = core.rsplit_once('-') {
            if last.len() == 8 && last.chars().all(|c| c.is_ascii_hexdigit()) {
                return (
                    pod_segment,
                    container_segment,
                    b.to_string(),
                    "deployment".to_string(),
                );
            }
        }

        (
            pod_segment,
            container_segment,
            core,
            "deployment".to_string(),
        )
    }

    /// 转义 Influx 标签值（空格、逗号）
    fn esc_tag(v: &str) -> String {
        v.replace(' ', "\\ ").replace(',', "\\,")
    }

    fn format_actor_tags(
        cluster_name: &str,
        node_tag: &str,
        event: &ActorLifecycleEvent,
    ) -> String {
        let cluster_tag = Self::esc_tag(cluster_name);
        let actor_tag = Self::esc_tag(&event.id.name);
        let root_tag = Self::esc_tag(&event.metadata.root_actor);
        let owner_tag = Self::esc_tag(&event.metadata.owner_type);
        let level_tag = event.id.level.as_str();
        let mut tags = format!(
            ",cluster_name={},node={},instance={},actor={},actor_level={},root_actor={},top_actor={},upper_actor={},owner_type={},canonical=1",
            cluster_tag,
            node_tag,
            node_tag,
            actor_tag,
            level_tag,
            root_tag,
            root_tag,
            root_tag,
            owner_tag
        );
        if let Some(parent) = &event.metadata.parent_actor {
            tags.push_str(&format!(",parent_actor={}", Self::esc_tag(parent)));
        }
        if matches!(event.id.level, ActorLevel::Pod) {
            let pod_value = event.metadata.pod_name.as_deref().unwrap_or(&event.id.name);
            let pod_tag = Self::esc_tag(pod_value);
            tags.push_str(&format!(",pod={},pod_name={}", pod_tag, pod_tag));
            if let Some(container_name) = &event.metadata.container_name {
                let container_tag = Self::esc_tag(container_name);
                tags.push_str(&format!(
                    ",container={},container_name={}",
                    container_tag, container_tag
                ));
            }
            if let Some(container_path) = &event.metadata.container_path {
                let container_path_tag = Self::esc_tag(container_path);
                tags.push_str(&format!(",container_path={}", container_path_tag));
            }
        }
        tags
    }

    fn parse_size_to_bytes(s: &str) -> u64 {
        // Supports: B, KiB, MiB, GiB, KB, MB, GB
        let s = s.trim();
        let (num, unit) = s.split_at(s.trim_end_matches(|c: char| c.is_alphabetic()).len());
        let val = num.trim().replace(',', ".").parse::<f64>().unwrap_or(0.0);
        let unit = unit.trim().to_ascii_lowercase();
        let mult = match unit.as_str() {
            "b" => 1.0,
            "kib" => 1024.0,
            "mib" => 1024.0 * 1024.0,
            "gib" => 1024.0 * 1024.0 * 1024.0,
            "kb" => 1000.0,
            "mb" => 1000.0 * 1000.0,
            "gb" => 1000.0 * 1000.0 * 1000.0,
            _ => 1.0,
        };
        (val * mult) as u64
    }

    async fn collect_system_metrics(node_id: &str) -> Result<SystemMetrics> {
        let cpu_usage = Self::get_cpu_usage().await?;
        let cpu_cores = Self::get_cpu_cores().await?;
        let (memory_usage, memory_used_bytes, memory_total_bytes) =
            Self::get_memory_stats().await?;
        let (network_rx_bytes, network_tx_bytes) = Self::get_network_stats().await?;

        Ok(SystemMetrics {
            timestamp: chrono::Utc::now().timestamp() as u64,
            cpu_usage,
            cpu_cores,
            memory_usage,
            memory_used_bytes,
            memory_total_bytes,
            network_rx_bytes,
            network_tx_bytes,
            node_id: node_id.to_string(),
        })
    }

    async fn get_cpu_usage() -> Result<f64> {
        // 读取 /proc/stat 获取CPU使用率
        let stat_content = tokio::fs::read_to_string("/proc/stat").await?;
        let line = stat_content.lines().next().unwrap_or("");

        // 解析第一行: cpu user nice system idle iowait irq softirq steal guest guest_nice
        let values: Vec<u64> = line
            .split_whitespace()
            .skip(1) // 跳过 "cpu" 标签
            .take(10)
            .filter_map(|s| s.parse().ok())
            .collect();

        if values.len() >= 4 {
            let idle = values[3];
            let total: u64 = values.iter().sum();
            if total > 0 {
                let cpu_usage = 100.0 - (idle as f64 / total as f64 * 100.0);
                return Ok(cpu_usage.max(0.0).min(100.0));
            }
        }

        // 如果解析失败，返回默认值
        Ok(0.0)
    }

    async fn get_memory_stats() -> Result<(f64, u64, u64)> {
        // 读取 /proc/meminfo 获取内存使用率
        let meminfo_content = tokio::fs::read_to_string("/proc/meminfo").await?;

        let mut mem_total = 0u64;
        let mut mem_available = 0u64;

        for line in meminfo_content.lines() {
            if line.starts_with("MemTotal:") {
                if let Some(value) = line.split_whitespace().nth(1) {
                    mem_total = value.parse().unwrap_or(0);
                }
            } else if line.starts_with("MemAvailable:") {
                if let Some(value) = line.split_whitespace().nth(1) {
                    mem_available = value.parse().unwrap_or(0);
                }
            }
        }

        if mem_total > 0 {
            let mem_used_kb = mem_total.saturating_sub(mem_available);
            let memory_usage = (mem_used_kb as f64 / mem_total as f64) * 100.0;
            let mem_used_bytes = mem_used_kb.saturating_mul(1024);
            let mem_total_bytes = mem_total.saturating_mul(1024);
            return Ok((
                memory_usage.max(0.0).min(100.0),
                mem_used_bytes,
                mem_total_bytes,
            ));
        }

        Ok((0.0, 0, 0))
    }

    async fn get_network_stats() -> Result<(u64, u64)> {
        // 读取 /proc/net/dev 获取网络统计信息
        let netdev_content = tokio::fs::read_to_string("/proc/net/dev").await?;

        let mut total_rx_bytes = 0u64;
        let mut total_tx_bytes = 0u64;

        for line in netdev_content.lines().skip(2) {
            // 跳过头部两行
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 10 {
                let interface = parts[0].trim_end_matches(':');
                // 忽略 loopback 接口
                if interface != "lo" {
                    if let (Ok(rx), Ok(tx)) = (parts[1].parse::<u64>(), parts[9].parse::<u64>()) {
                        total_rx_bytes += rx;
                        total_tx_bytes += tx;
                    }
                }
            }
        }

        Ok((total_rx_bytes, total_tx_bytes))
    }

    async fn get_cpu_cores() -> Result<u64> {
        // Prefer /proc/cpuinfo logical processor count
        let content = tokio::fs::read_to_string("/proc/cpuinfo")
            .await
            .unwrap_or_default();
        let mut count = 0u64;
        for line in content.lines() {
            // Each logical processor in linux shows a 'processor\t: N' entry
            if line.to_ascii_lowercase().starts_with("processor") && line.contains(':') {
                count += 1;
            }
        }
        if count == 0 {
            // Fallback: try /sys/devices/system/cpu/present like 0-3 -> 4 cores
            if let Ok(present) = tokio::fs::read_to_string("/sys/devices/system/cpu/present").await
            {
                let s = present.trim();
                if let Some((start, end)) = s.split_once('-') {
                    if let (Ok(a), Ok(b)) = (start.parse::<u64>(), end.parse::<u64>()) {
                        count = b.saturating_sub(a) + 1;
                    }
                }
            }
        }
        if count == 0 {
            count = 1;
        } // sensible fallback
        Ok(count)
    }

    async fn push_to_greptimedb_all(
        client: &Client,
        greptimedb_url: &str,
        metrics: &SystemMetrics,
        node_id: &str,
        container_metrics: &[(String, (f64, u64, f64))],
        cluster_name: &str,
        exited_containers: &[String],
        actor_statuses: &[ActorLifecycleEvent],
        actor_events: &ActorLifecycleBatch,
    ) -> Result<()> {
        let instance = &metrics.node_id;
        // 为了方便格式化，准备一个闭包构建公共标签（系统级）
        let node_tag = Self::esc_tag(instance);

        // 使用 InfluxDB 行协议格式，这是 GreptimeDB 推荐的写入方式
        let mut influxdb_metrics = String::new();
        let ts = metrics.timestamp * 1_000_000_000;
        // 系统级指标附带 cluster_name 与 node 标签
        let sys_tags = format!(
            ",cluster_name={},node={},instance={}",
            Self::esc_tag(cluster_name),
            node_tag,
            node_tag
        );
        influxdb_metrics.push_str(&format!(
            "nokube_cpu_usage{} value={} {}\n",
            sys_tags, metrics.cpu_usage, ts
        ));
        influxdb_metrics.push_str(&format!(
            "nokube_cpu_cores{} value={} {}\n",
            sys_tags, metrics.cpu_cores, ts
        ));
        influxdb_metrics.push_str(&format!(
            "nokube_memory_usage{} value={} {}\n",
            sys_tags, metrics.memory_usage, ts
        ));
        influxdb_metrics.push_str(&format!(
            "nokube_memory_used_bytes{} value={} {}\n",
            sys_tags, metrics.memory_used_bytes, ts
        ));
        influxdb_metrics.push_str(&format!(
            "nokube_memory_total_bytes{} value={} {}\n",
            sys_tags, metrics.memory_total_bytes, ts
        ));
        influxdb_metrics.push_str(&format!(
            "nokube_network_rx_bytes{} value={} {}\n",
            sys_tags, metrics.network_rx_bytes, ts
        ));
        influxdb_metrics.push_str(&format!(
            "nokube_network_tx_bytes{} value={} {}\n",
            sys_tags, metrics.network_tx_bytes, ts
        ));

        // 计算节点内存派生量（与容器同步采样时间戳对齐）
        let sum_container_mem_bytes: u64 = container_metrics
            .iter()
            .map(|(_, (_cpu, mem_bytes, _pct))| *mem_bytes)
            .sum();
        let other_used_bytes = metrics
            .memory_used_bytes
            .saturating_sub(sum_container_mem_bytes);
        let free_bytes = metrics
            .memory_total_bytes
            .saturating_sub(metrics.memory_used_bytes);

        influxdb_metrics.push_str(&format!(
            "nokube_node_mem_other_bytes{} value={} {}\n",
            sys_tags, other_used_bytes, ts
        ));
        influxdb_metrics.push_str(&format!(
            "nokube_node_mem_free_bytes{} value={} {}\n",
            sys_tags, free_bytes, ts
        ));

        for event in actor_statuses {
            let tags = Self::format_actor_tags(cluster_name, &node_tag, event);
            influxdb_metrics.push_str(&format!("nokube_actor_status{} value=1 {}\n", tags, ts));
        }

        for event in &actor_events.starts {
            let tags = Self::format_actor_tags(cluster_name, &node_tag, event);
            influxdb_metrics.push_str(&format!("nokube_actor_lifecycle{} value=1 {}\n", tags, ts));
        }
        for event in &actor_events.stops {
            let tags = Self::format_actor_tags(cluster_name, &node_tag, event);
            influxdb_metrics.push_str(&format!("nokube_actor_lifecycle{} value=-1 {}\n", tags, ts));
            influxdb_metrics.push_str(&format!("nokube_actor_status{} value=-1 {}\n", tags, ts));
        }

        // 追加容器级别指标
        for (name, (cpu_pct, mem_bytes, mem_pct)) in container_metrics {
            let container_tag = Self::esc_tag(name);
            let (pod, container_segment, core_actor, owner_type) =
                Self::derive_actor_core(name, instance, cluster_name);
            // 规范化顶层名：始终为 <core>-<cluster>
            let canonical_upper = format!("{}-{}", core_actor, cluster_name);
            let pod_tag = Self::esc_tag(&pod);
            let upper_tag = Self::esc_tag(&canonical_upper);
            let actor_tag = upper_tag.clone();
            let owner_tag = Self::esc_tag(&owner_type);
            let parent_tag = pod_tag.clone();
            // absolute cpu in cores
            let cpu_cores_val = (cpu_pct / 100.0) * (metrics.cpu_cores as f64);
            // richer label set for easier Grafana queries
            let container_path = format!(
                "{}/{}/{}/{}",
                cluster_name, core_actor, pod, container_segment
            );
            let container_path_tag = Self::esc_tag(&container_path);
            let tags = format!(
                ",cluster_name={},node={},instance={},container={},container_name={},pod={},pod_name={},parent_actor={},root_actor={},top_actor={},upper_actor={},owner_type={},container_path={},canonical=1",
                Self::esc_tag(cluster_name),
                node_tag,
                node_tag,
                container_tag,
                container_tag,
                pod_tag,
                pod_tag,
                parent_tag,
                upper_tag,
                actor_tag,
                upper_tag,
                owner_tag,
                container_path_tag
            );
            influxdb_metrics.push_str(&format!("nokube_container_status{} value=1 {}\n", tags, ts));
            influxdb_metrics.push_str(&format!(
                "nokube_container_cpu{} value={} {}\n",
                tags, cpu_pct, ts
            ));
            influxdb_metrics.push_str(&format!(
                "nokube_container_cpu_cores{} value={} {}\n",
                tags, cpu_cores_val, ts
            ));
            influxdb_metrics.push_str(&format!(
                "nokube_container_mem_bytes{} value={} {}\n",
                tags, mem_bytes, ts
            ));
            influxdb_metrics.push_str(&format!(
                "nokube_container_mem_percent{} value={} {}\n",
                tags, mem_pct, ts
            ));
        }

        for name in exited_containers {
            let container_tag = Self::esc_tag(name);
            let (pod, container_segment, core_actor, owner_type) =
                Self::derive_actor_core(name, instance, cluster_name);
            let canonical_upper = format!("{}-{}", core_actor, cluster_name);
            let pod_tag = Self::esc_tag(&pod);
            let upper_tag = Self::esc_tag(&canonical_upper);
            let actor_tag = upper_tag.clone();
            let owner_tag = Self::esc_tag(&owner_type);
            let parent_tag = pod_tag.clone();
            let container_path = format!(
                "{}/{}/{}/{}",
                cluster_name, core_actor, pod, container_segment
            );
            let container_path_tag = Self::esc_tag(&container_path);
            let tags = format!(
                ",cluster_name={},node={},instance={},container={},container_name={},pod={},pod_name={},parent_actor={},root_actor={},top_actor={},upper_actor={},owner_type={},container_path={},canonical=1",
                Self::esc_tag(cluster_name),
                node_tag,
                node_tag,
                container_tag,
                container_tag,
                pod_tag,
                pod_tag,
                parent_tag,
                upper_tag,
                actor_tag,
                upper_tag,
                owner_tag,
                container_path_tag
            );
            influxdb_metrics.push_str(&format!(
                "nokube_container_status{} value=-1 {}\n",
                tags, ts
            ));
            influxdb_metrics.push_str(&format!("nokube_container_cpu{} value=0 {}\n", tags, ts));
            influxdb_metrics.push_str(&format!(
                "nokube_container_cpu_cores{} value=0 {}\n",
                tags, ts
            ));
            influxdb_metrics.push_str(&format!(
                "nokube_container_mem_bytes{} value=0 {}\n",
                tags, ts
            ));
            influxdb_metrics.push_str(&format!(
                "nokube_container_mem_percent{} value=0 {}\n",
                tags, ts
            ));
        }

        // 使用 InfluxDB 写入端点而不是 Prometheus remote write
        let url = format!("{}/v1/influxdb/write", greptimedb_url);

        tracing::debug!("Pushing metrics to GreptimeDB URL: {}", url);
        tracing::debug!("Request body (InfluxDB format):\n{}", influxdb_metrics);

        let response = client
            .post(&url)
            .header("Content-Type", "text/plain")
            .body(influxdb_metrics.clone())
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error response".to_string());
            anyhow::bail!(
                "Failed to push metrics to GreptimeDB URL {}: {} {}\nRequest body (InfluxDB format):\n{}\nResponse body:\n{}",
                url,
                status.as_u16(),
                status.canonical_reason().unwrap_or("Unknown"),
                influxdb_metrics,
                error_body
            );
        }

        tracing::debug!(
            "Successfully pushed metrics (node+containers) to GreptimeDB for node: {}",
            instance
        );
        Ok(())
    }
}
