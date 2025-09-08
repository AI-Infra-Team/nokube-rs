// etcd存储模块 - 专门处理config和secret对象
use crate::config::etcd_manager::EtcdManager;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// ConfigMap对象
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigMapObject {
    pub name: String,
    pub namespace: String,
    pub data: HashMap<String, String>,
    pub binary_data: Option<HashMap<String, Vec<u8>>>,
}

/// Secret对象
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretObject {
    pub name: String,
    pub namespace: String,
    pub secret_type: String,
    pub data: HashMap<String, Vec<u8>>, // base64编码的数据
    pub string_data: Option<HashMap<String, String>>, // 原始字符串数据
}

/// etcd中k8s对象的存储管理器
pub struct K8sStorageManager {
    etcd_manager: EtcdManager,
}

impl K8sStorageManager {
    pub fn new(etcd_manager: EtcdManager) -> Self {
        Self { etcd_manager }
    }
    
    /// 存储ConfigMap到etcd
    /// Key格式: k8s/configmap/{namespace}/{name}
    pub async fn store_configmap(&self, configmap: &ConfigMapObject) -> Result<()> {
        let key = format!("k8s/configmap/{}/{}", configmap.namespace, configmap.name);
        let value = serde_json::to_string(configmap)?;
        
        self.etcd_manager.put(key, value).await?;
        
        tracing::info!("Stored ConfigMap {}/{} to etcd", configmap.namespace, configmap.name);
        Ok(())
    }
    
    /// 从etcd获取ConfigMap
    pub async fn get_configmap(&self, namespace: &str, name: &str) -> Result<Option<ConfigMapObject>> {
        let key = format!("k8s/configmap/{}/{}", namespace, name);
        let kvs = self.etcd_manager.get(key).await?;
        
        if kvs.is_empty() {
            return Ok(None);
        }
        
        let configmap: ConfigMapObject = serde_json::from_str(kvs[0].value_str())?;
        Ok(Some(configmap))
    }
    
    /// 存储Secret到etcd
    /// Key格式: k8s/secret/{namespace}/{name}
    pub async fn store_secret(&self, secret: &SecretObject) -> Result<()> {
        let key = format!("k8s/secret/{}/{}", secret.namespace, secret.name);
        let value = serde_json::to_string(secret)?;
        
        self.etcd_manager.put(key, value).await?;
        
        tracing::info!("Stored Secret {}/{} to etcd", secret.namespace, secret.name);
        Ok(())
    }
    
    /// 从etcd获取Secret
    pub async fn get_secret(&self, namespace: &str, name: &str) -> Result<Option<SecretObject>> {
        let key = format!("k8s/secret/{}/{}", namespace, name);
        let kvs = self.etcd_manager.get(key).await?;
        
        if kvs.is_empty() {
            return Ok(None);
        }
        
        let secret: SecretObject = serde_json::from_str(kvs[0].value_str())?;
        Ok(Some(secret))
    }
    
    /// 列出namespace下所有ConfigMap
    pub async fn list_configmaps(&self, namespace: &str) -> Result<Vec<ConfigMapObject>> {
        let prefix = format!("k8s/configmap/{}/", namespace);
        let kvs = self.etcd_manager.get_prefix(prefix).await?;
        
        let mut configmaps = Vec::new();
        for kv in kvs {
            if let Ok(configmap) = serde_json::from_str::<ConfigMapObject>(kv.value_str()) {
                configmaps.push(configmap);
            }
        }
        Ok(configmaps)
    }
    
    /// 列出namespace下所有Secret
    pub async fn list_secrets(&self, namespace: &str) -> Result<Vec<SecretObject>> {
        let prefix = format!("k8s/secret/{}/", namespace);
        let kvs = self.etcd_manager.get_prefix(prefix).await?;
        
        let mut secrets = Vec::new();
        for kv in kvs {
            if let Ok(secret) = serde_json::from_str::<SecretObject>(kv.value_str()) {
                secrets.push(secret);
            }
        }
        Ok(secrets)
    }
    
    /// 删除ConfigMap
    pub async fn delete_configmap(&self, namespace: &str, name: &str) -> Result<()> {
        let key = format!("k8s/configmap/{}/{}", namespace, name);
        self.etcd_manager.delete(key).await?;
        
        tracing::info!("Deleted ConfigMap {}/{} from etcd", namespace, name);
        Ok(())
    }
    
    /// 删除Secret
    pub async fn delete_secret(&self, namespace: &str, name: &str) -> Result<()> {
        let key = format!("k8s/secret/{}/{}", namespace, name);
        self.etcd_manager.delete(key).await?;
        
        tracing::info!("Deleted Secret {}/{} from etcd", namespace, name);
        Ok(())
    }
}