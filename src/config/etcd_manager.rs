use crate::config::cluster_config::ClusterConfig;
use anyhow::Result;
use etcd_rs::{
    Client, ClientConfig, DeleteRequest, Endpoint, KeyRange, KeyValueOp, PutRequest, RangeRequest,
};
use serde_json;
use tokio::time::{interval, Duration};

pub struct EtcdManager {
    client: Client,
}

impl EtcdManager {
    pub async fn store_cluster_meta(
        &self,
        meta: &crate::config::cluster_config::ClusterMeta,
    ) -> anyhow::Result<()> {
        let key = format!("cluster/{}", meta.cluster_name);
        let value = serde_json::to_string(meta)?;
        let req = etcd_rs::PutRequest::new(key, value);
        self.client.put(req).await?;
        tracing::info!("Stored cluster meta for {}", meta.cluster_name);
        Ok(())
    }

    pub async fn get_cluster_meta(
        &self,
        cluster_name: &str,
    ) -> anyhow::Result<Option<crate::config::cluster_config::ClusterMeta>> {
        let key = format!("cluster/{}", cluster_name);
        let req = RangeRequest::new(KeyRange::key(key.clone()));
        let resp = self.client.get(req).await?;
        if resp.kvs.is_empty() {
            return Ok(None);
        }
        let value = resp.kvs[0].value_str();
        let meta: crate::config::cluster_config::ClusterMeta = serde_json::from_str(&value)?;
        Ok(Some(meta))
    }

    pub async fn list_cluster_metas(
        &self,
    ) -> anyhow::Result<Vec<crate::config::cluster_config::ClusterMeta>> {
        let req = RangeRequest::new(KeyRange::prefix("cluster/"));
        let resp = self.client.get(req).await?;
        let mut metas = Vec::new();
        for kv in resp.kvs {
            if let Ok(meta) =
                serde_json::from_str::<crate::config::cluster_config::ClusterMeta>(kv.value_str())
            {
                metas.push(meta);
            }
        }
        Ok(metas)
    }
    pub async fn new(endpoints: Vec<String>) -> Result<Self> {
        tracing::info!("Initializing EtcdManager with endpoints: {:?}", endpoints);
        
        for e in &endpoints {
            if !(e.starts_with("http://") || e.starts_with("https://")) {
                anyhow::bail!("etcd endpoint 必须包含 http:// 或 https:// 前缀: {}", e);
            }
        }
        
        tracing::info!("Creating etcd client configuration...");
        let endpoints: Vec<Endpoint> = endpoints.into_iter().map(|e| Endpoint::new(e)).collect();
        let config = ClientConfig::new(endpoints);
        
        tracing::info!("Attempting to connect to etcd...");
        
        // 使用超时避免无限期等待
        let connect_future = Client::connect(config);
        let timeout_duration = Duration::from_secs(10);
        
        let client = tokio::time::timeout(timeout_duration, connect_future)
            .await
            .map_err(|_| anyhow::anyhow!("Etcd connection timed out after 10 seconds"))?
            .map_err(|e| anyhow::anyhow!("Failed to connect to etcd: {}", e))?;
        
        tracing::info!("Successfully connected to etcd!");
        Ok(Self { client })
    }

    pub async fn store_cluster_config(&self, config: &ClusterConfig) -> Result<()> {
        let key = format!("{}", config.cluster_name);
        let value = serde_json::to_string(config)?;

        let req = PutRequest::new(key, value);
        self.client.put(req).await?;
        tracing::info!("Stored cluster config for {}", config.cluster_name);
        Ok(())
    }

    pub async fn get_cluster_config(&self, cluster_name: &str) -> Result<Option<ClusterConfig>> {
        let key = cluster_name.to_string();

        let req = RangeRequest::new(KeyRange::key(key.clone()));
        let resp = self.client.get(req).await?;

        if resp.kvs.is_empty() {
            return Ok(None);
        }

        let value = resp.kvs[0].value_str();
        let config: ClusterConfig = serde_json::from_str(&value)?;
        Ok(Some(config))
    }

    pub async fn watch_cluster_config<F>(
        &self,
        cluster_name: &str,
        poll_interval: u64,
        mut callback: F,
    ) -> Result<()>
    where
        F: FnMut(ClusterConfig) + Send + 'static,
    {
        let mut ticker = interval(Duration::from_secs(poll_interval));
        let key = cluster_name.to_string();
        let mut last_revision = 0i64;

        loop {
            ticker.tick().await;

            let req = RangeRequest::new(KeyRange::key(key.clone()));
            match self.client.get(req).await {
                Ok(resp) => {
                    if !resp.kvs.is_empty() {
                        let kv = &resp.kvs[0];
                        if kv.mod_revision > last_revision {
                            last_revision = kv.mod_revision;

                            match serde_json::from_str::<ClusterConfig>(kv.value_str()) {
                                Ok(config) => {
                                    tracing::info!(
                                        "Detected config change for cluster: {}",
                                        cluster_name
                                    );
                                    callback(config);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to parse config: {}", e);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to poll etcd: {}", e);
                }
            }
        }
    }

    pub async fn delete_cluster_config(&self, cluster_name: &str) -> Result<()> {
        let key = cluster_name.to_string();
        let req = DeleteRequest::new(key);
        self.client.delete(req).await?;
        tracing::info!("Deleted cluster config for {}", cluster_name);
        Ok(())
    }
}
