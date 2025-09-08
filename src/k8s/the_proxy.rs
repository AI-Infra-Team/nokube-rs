// the_proxy - 特殊actor协程，一个agent一个，用于聚合所有 k8s actor 的请求
// 负责校验从属以及管控的actor是否alive，以及keepalive机制
use crate::k8s::{GlobalAttributionPath, ComponentStatus};
use crate::config::config_manager::ConfigManager;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, Duration};
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

/// Actor存活请求
#[derive(Debug)]
pub struct ActorAliveRequest {
    pub actor_path: GlobalAttributionPath,
    pub actor_type: String, // daemonset, deployment, pod
    pub cluster_name: String,
    pub status: ComponentStatus,
    pub lease_ttl: u64, // lease过期时间(秒)
}

/// Actor存活响应
#[derive(Debug)]
pub struct ActorAliveResponse {
    pub success: bool,
    pub lease_id: Option<u64>,
    pub message: String,
}

/// Actor状态信息
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActorStatus {
    pub actor_path: GlobalAttributionPath,
    pub actor_type: String,
    pub cluster_name: String,
    pub status: ComponentStatus,
    pub last_alive: DateTime<Utc>,
    pub lease_id: Option<u64>,
    pub lease_ttl: u64,
}

/// the_proxy - 聚合所有k8s actor的特殊协程
pub struct TheProxy {
    /// agent标识
    pub agent_id: String,
    pub cluster_name: String,
    
    /// 接收actor alive请求
    alive_rx: mpsc::Receiver<ActorAliveRequest>,
    alive_tx: mpsc::Sender<ActorAliveRequest>,
    
    /// 管理所有actor状态
    actor_states: Arc<RwLock<HashMap<GlobalAttributionPath, ActorStatus>>>,
    
    /// etcd配置管理器
    config_manager: Arc<ConfigManager>,
    
    /// keepalive间隔
    keepalive_interval: Duration,
}

impl TheProxy {
    pub fn new(
        agent_id: String,
        cluster_name: String,
        config_manager: Arc<ConfigManager>,
        keepalive_interval_secs: u64,
    ) -> Self {
        let (alive_tx, alive_rx) = mpsc::channel(1000);
        
        Self {
            agent_id,
            cluster_name,
            alive_rx,
            alive_tx,
            actor_states: Arc::new(RwLock::new(HashMap::new())),
            config_manager,
            keepalive_interval: Duration::from_secs(keepalive_interval_secs),
        }
    }
    
    /// 获取alive请求发送器（供k8s actor使用）
    pub fn get_alive_sender(&self) -> mpsc::Sender<ActorAliveRequest> {
        self.alive_tx.clone()
    }
    
    /// 启动the_proxy协程
    pub async fn start(&mut self) -> Result<()> {
        tracing::info!("Starting the_proxy for agent: {}", self.agent_id);
        
        // 启动alive请求处理协程
        let states = self.actor_states.clone();
        let config_manager = self.config_manager.clone();
        let cluster_name = self.cluster_name.clone();
        let alive_rx = std::mem::replace(&mut self.alive_rx, mpsc::channel(1).1);
        
        tokio::spawn(async move {
            Self::handle_alive_requests(alive_rx, states, config_manager, cluster_name).await;
        });
        
        // 启动定期keepalive协程
        let states = self.actor_states.clone();
        let config_manager = self.config_manager.clone();
        let cluster_name = self.cluster_name.clone();
        let interval = self.keepalive_interval;
        
        tokio::spawn(async move {
            Self::periodic_keepalive(states, config_manager, cluster_name, interval).await;
        });
        
        tracing::info!("the_proxy started for agent: {}", self.agent_id);
        Ok(())
    }
    
    /// 处理actor alive请求的协程
    async fn handle_alive_requests(
        mut alive_rx: mpsc::Receiver<ActorAliveRequest>,
        states: Arc<RwLock<HashMap<GlobalAttributionPath, ActorStatus>>>,
        config_manager: Arc<ConfigManager>,
        cluster_name: String,
    ) {
        while let Some(request) = alive_rx.recv().await {
            if let Err(e) = Self::process_alive_request(
                request,
                &states,
                &config_manager,
                &cluster_name,
            ).await {
                tracing::error!("Failed to process alive request: {}", e);
            }
        }
    }
    
    /// 处理单个alive请求
    async fn process_alive_request(
        request: ActorAliveRequest,
        states: &Arc<RwLock<HashMap<GlobalAttributionPath, ActorStatus>>>,
        config_manager: &Arc<ConfigManager>,
        cluster_name: &str,
    ) -> Result<()> {
        let actor_path = request.actor_path.clone();
        let actor_type = request.actor_type.clone();
        let cluster_name_req = request.cluster_name.clone();
        let status = request.status.clone();
        let lease_ttl = request.lease_ttl;
        
        // 更新actor状态
        {
            let mut states_guard = states.write().await;
            let actor_status = ActorStatus {
                actor_path: actor_path.clone(),
                actor_type: actor_type.clone(),
                cluster_name: cluster_name_req,
                status: status.clone(),
                last_alive: Utc::now(),
                lease_id: None, // TODO: 实现etcd lease
                lease_ttl,
            };
            states_guard.insert(actor_path.clone(), actor_status);
        }
        
        // 写入etcd alive冗余key
        let alive_key = format!(
            "/nokube/{}/actors/{}/alive",
            cluster_name,
            actor_path.to_string().replace("/", "-")
        );
        
        let alive_data = serde_json::json!({
            "actor_path": actor_path.to_string(),
            "actor_type": actor_type,
            "status": status.to_string(),
            "last_alive": chrono::Utc::now().to_rfc3339(),
            "lease_ttl": lease_ttl,
        });
        
        let etcd_manager = config_manager.get_etcd_manager();
        etcd_manager.put(alive_key, alive_data.to_string()).await?;
        
        tracing::debug!("Updated alive status for actor: {}", actor_path.to_string());
        Ok(())
    }
    
    /// 定期keepalive协程
    async fn periodic_keepalive(
        states: Arc<RwLock<HashMap<GlobalAttributionPath, ActorStatus>>>,
        config_manager: Arc<ConfigManager>,
        cluster_name: String,
        keepalive_interval: Duration,
    ) {
        let mut interval = interval(keepalive_interval);
        
        loop {
            interval.tick().await;
            
            if let Err(e) = Self::perform_keepalive(&states, &config_manager, &cluster_name).await {
                tracing::error!("Keepalive failed: {}", e);
            }
        }
    }
    
    /// 执行keepalive操作
    async fn perform_keepalive(
        states: &Arc<RwLock<HashMap<GlobalAttributionPath, ActorStatus>>>,
        config_manager: &Arc<ConfigManager>,
        cluster_name: &str,
    ) -> Result<()> {
        let states_guard = states.read().await;
        let now = Utc::now();
        
        for (actor_path, actor_status) in states_guard.iter() {
            // 检查actor是否超时
            let elapsed = now.signed_duration_since(actor_status.last_alive);
            if elapsed > chrono::Duration::seconds(actor_status.lease_ttl as i64 * 2) {
                tracing::warn!(
                    "Actor {} appears to be dead (no alive signal for {:?})",
                    actor_path.to_string(),
                    elapsed
                );
                
                // 标记为dead
                let dead_key = format!(
                    "/nokube/{}/actors/{}/alive",
                    cluster_name,
                    actor_path.to_string().replace("/", "-")
                );
                
                let dead_data = serde_json::json!({
                    "actor_path": actor_path.to_string(),
                    "actor_type": actor_status.actor_type,
                    "status": "Dead",
                    "last_alive": chrono::Utc::now().to_rfc3339(),
                    "lease_expired": true,
                });
                
                let etcd_manager = config_manager.get_etcd_manager();
                etcd_manager.put(dead_key, dead_data.to_string()).await?;
            }
        }
        
        tracing::debug!("Keepalive check completed for {} actors", states_guard.len());
        Ok(())
    }
    
    /// 获取所有actor状态
    pub async fn get_actor_states(&self) -> HashMap<GlobalAttributionPath, ActorStatus> {
        self.actor_states.read().await.clone()
    }
    
    /// 检查特定actor是否alive
    pub async fn is_actor_alive(&self, actor_path: &GlobalAttributionPath) -> bool {
        let states_guard = self.actor_states.read().await;
        if let Some(status) = states_guard.get(actor_path) {
            let elapsed = Utc::now().signed_duration_since(status.last_alive);
            elapsed < chrono::Duration::seconds(status.lease_ttl as i64 * 2)
        } else {
            false
        }
    }
}

impl ComponentStatus {
    pub fn to_string(&self) -> String {
        match self {
            ComponentStatus::Starting => "Starting".to_string(),
            ComponentStatus::Running => "Running".to_string(),
            ComponentStatus::Stopping => "Stopping".to_string(),
            ComponentStatus::Stopped => "Stopped".to_string(),
            ComponentStatus::Failed => "Failed".to_string(),
        }
    }
}