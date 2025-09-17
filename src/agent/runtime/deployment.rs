use anyhow::Result;
use base64::Engine as _;
use serde_yaml::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::agent::general::{DockerRunConfig, DockerRunner};
use crate::config::etcd_manager::EtcdManager;

pub fn calc_hash_u64(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

pub async fn create_deployment_container(
    deployment_yaml: &Value,
    deployment_name: &str,
    cluster_name: &str,
    etcd_manager: &Arc<EtcdManager>,
    workspace: &str,
) -> Result<()> {
    info!(
        "Creating deployment container (unified): {}",
        deployment_name
    );

    let spec = deployment_yaml
        .get("spec")
        .ok_or_else(|| anyhow::anyhow!("Missing spec in deployment"))?;

    let deployment_spec = if spec.get("template").is_some() {
        info!("🎯 Processing standard K8s Deployment format");
        spec
    } else if let Some(deploy) = spec.get("deployment") {
        info!("🎯 Processing GitOpsCluster deployment format");
        deploy
    } else {
        return Err(anyhow::anyhow!("Missing template or deployment in spec"));
    };

    let container_spec = if let Some(template) = deployment_spec.get("template") {
        if let Some(template_spec) = template.get("spec") {
            template_spec
                .get("containers")
                .and_then(|c| c.as_sequence())
                .and_then(|seq| seq.first())
                .ok_or_else(|| anyhow::anyhow!("Missing containers in template.spec"))?
        } else {
            template
                .get("containerSpec")
                .ok_or_else(|| anyhow::anyhow!("Missing containerSpec in template"))?
        }
    } else {
        deployment_spec
            .get("containerSpec")
            .ok_or_else(|| anyhow::anyhow!("Missing containerSpec in deployment"))?
    };

    let image = container_spec
        .get("image")
        .and_then(|v| v.as_str())
        .unwrap_or("python:3.10-slim");

    let command: Option<Vec<String>> = container_spec
        .get("command")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        });
    let args: Option<Vec<String>> = container_spec
        .get("args")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        });

    let mut env = HashMap::new();
    if let Some(env_obj) = container_spec.get("env") {
        if let Some(env_map) = env_obj.as_mapping() {
            for (k, v) in env_map {
                if let (Some(key), Some(value)) = (k.as_str(), v.as_str()) {
                    env.insert(key.to_string(), value.to_string());
                }
            }
        } else if let Some(env_array) = env_obj.as_sequence() {
            for env_item in env_array {
                if let (Some(name), Some(value)) = (
                    env_item.get("name").and_then(|v| v.as_str()),
                    env_item.get("value").and_then(|v| v.as_str()),
                ) {
                    env.insert(name.to_string(), value.to_string());
                }
            }
        }
    }

    let container_name = format!("nokube-pod-{}", deployment_name);
    let mut config = DockerRunConfig::new(container_name.clone(), image.to_string())
        .restart_policy("unless-stopped".to_string());

    if let Some(cmd) = command.as_ref() {
        config = config.command(cmd.clone());
    }

    for (key, value) in env.iter() {
        config = config.add_env(key.clone(), value.clone());
    }

    for key in [
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
    ] {
        if !env.contains_key(key) {
            if let Ok(val) = std::env::var(key) {
                if !val.is_empty() {
                    config = config.add_env(key.to_string(), val);
                }
            }
        }
    }

    if let Some(configmap_data) = spec.get("configMap").and_then(|cm| cm.get("data")) {
        if let Some(map) = configmap_data.as_mapping() {
            let config_dir = format!("{}/configmaps/{}", workspace, deployment_name);
            std::fs::create_dir_all(&config_dir)?;
            for (k, v) in map {
                if let (Some(name), Some(value)) = (k.as_str(), v.as_str()) {
                    let file_path = format!("{}/{}", config_dir, name);
                    std::fs::write(&file_path, value)?;
                    info!("📝 Created config file: {}", file_path);
                }
            }
            config = config.add_volume(config_dir, "/etc/config".to_string(), false);
        }
    }

    if let Some(volumes) = deployment_spec.get("volumes").and_then(|v| v.as_sequence()) {
        for volume in volumes {
            if let Some(volume_name) = volume.get("name").and_then(|v| v.as_str()) {
                if let Some(config_map) = volume.get("configMap") {
                    if let Some(config_name) = config_map.get("name").and_then(|v| v.as_str()) {
                        let volume_dir = format!("{}/configmaps/{}", workspace, config_name);
                        std::fs::create_dir_all(&volume_dir)?;
                        match load_configmap_from_etcd(etcd_manager, cluster_name, config_name)
                            .await?
                        {
                            Some(data) => {
                                if let Some(map) = data.as_mapping() {
                                    for (filename, content) in map {
                                        if let (Some(name), Some(value)) =
                                            (filename.as_str(), content.as_str())
                                        {
                                            let file_path = format!("{}/{}", volume_dir, name);
                                            std::fs::write(&file_path, value)?;
                                            info!("✅ Created ConfigMap file: {}", file_path);
                                        }
                                    }
                                }
                            }
                            None => warn!("⚠️  ConfigMap '{}' not found in etcd", config_name),
                        }

                        if let Some(volume_mounts) = container_spec
                            .get("volumeMounts")
                            .and_then(|vm| vm.as_sequence())
                        {
                            for mount in volume_mounts {
                                if let (Some(mount_name), Some(mount_path)) = (
                                    mount.get("name").and_then(|v| v.as_str()),
                                    mount.get("mountPath").and_then(|v| v.as_str()),
                                ) {
                                    if mount_name == volume_name {
                                        config = config.add_volume(
                                            volume_dir.clone(),
                                            mount_path.to_string(),
                                            false,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                if let Some(secret) = volume.get("secret") {
                    if let Some(secret_name) = secret.get("secretName").and_then(|v| v.as_str()) {
                        let volume_dir = format!("{}/secrets/{}", workspace, secret_name);
                        std::fs::create_dir_all(&volume_dir)?;
                        match load_secret_from_etcd(etcd_manager, cluster_name, secret_name).await?
                        {
                            Some(secret_data) => {
                                if let Some(map) = secret_data.as_mapping() {
                                    for (filename, content) in map {
                                        if let (Some(name), Some(value)) =
                                            (filename.as_str(), content.as_str())
                                        {
                                            let decoded = base64::engine::general_purpose::STANDARD
                                                .decode(value.as_bytes())
                                                .ok()
                                                .and_then(|bytes| String::from_utf8(bytes).ok())
                                                .unwrap_or_else(|| value.to_string());
                                            let file_path = format!("{}/{}", volume_dir, name);
                                            std::fs::write(&file_path, decoded)?;
                                            info!("✅ Created Secret volume file: {}", file_path);
                                        }
                                    }
                                }
                            }
                            None => warn!("⚠️  Secret '{}' not found in etcd", secret_name),
                        }

                        if let Some(volume_mounts) = container_spec
                            .get("volumeMounts")
                            .and_then(|vm| vm.as_sequence())
                        {
                            for mount in volume_mounts {
                                if let (Some(mount_name), Some(mount_path)) = (
                                    mount.get("name").and_then(|v| v.as_str()),
                                    mount.get("mountPath").and_then(|v| v.as_str()),
                                ) {
                                    if mount_name == volume_name {
                                        config = config.add_volume(
                                            volume_dir.clone(),
                                            mount_path.to_string(),
                                            true,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut container_command = Vec::new();
    if let Some(cmd) = command.as_ref() {
        container_command.extend(cmd.iter().cloned());
    }
    if let Some(args) = args.as_ref() {
        container_command.extend(args.iter().cloned());
    }
    if !container_command.is_empty() {
        config = config.command(container_command);
    }

    let yaml_text = serde_yaml::to_string(deployment_yaml).unwrap_or_default();
    let checksum = calc_hash_u64(&yaml_text);
    config.extra_args.push("--label".to_string());
    config
        .extra_args
        .push(format!("nokube.actor.checksum={}", checksum));

    match DockerRunner::run(&config) {
        Ok(container_id) => {
            info!(
                "Created Docker container: {} with ID: {}",
                container_name, container_id
            );
            store_pod_status(etcd_manager, cluster_name, deployment_name, "Running", None).await?;
        }
        Err(e) => {
            error!("Failed to create deployment {}: {}", deployment_name, e);
            store_pod_status(
                etcd_manager,
                cluster_name,
                deployment_name,
                "Failed",
                Some(&format!("{}", e)),
            )
            .await?;
        }
    }

    Ok(())
}

pub async fn store_pod_status(
    etcd_manager: &Arc<EtcdManager>,
    cluster_name: &str,
    pod_name: &str,
    status: &str,
    error_message: Option<&str>,
) -> anyhow::Result<()> {
    let pod_key = format!("/nokube/{}/pods/{}", cluster_name, pod_name);

    let mut pod_info = serde_json::json!({
        "name": pod_name,
        "namespace": "default",
        "node": "agent-node",
        "image": "python:3.10-slim",
        "container_id": serde_json::Value::Null,
        "status": status,
        "ready": status == "Running",
        "restart_count": 0,
        "start_time": chrono::Utc::now().to_rfc3339(),
        "pod_ip": if status == "Running" { serde_json::Value::String("172.17.0.5".to_string()) } else { serde_json::Value::Null },
        "labels": {
            "app": pod_name,
            "role": "pod"
        },
        "ports": ["8080/TCP"],
        "priority": 0
    });

    if let Some(error) = error_message {
        pod_info["error_message"] = serde_json::Value::String(error.to_string());
    }

    etcd_manager.put(pod_key, pod_info.to_string()).await?;
    Ok(())
}

pub async fn load_configmap_from_etcd(
    etcd_manager: &Arc<EtcdManager>,
    cluster_name: &str,
    configmap_name: &str,
) -> Result<Option<Value>> {
    let configmap_key = format!("/nokube/{}/configmaps/{}", cluster_name, configmap_name);

    match etcd_manager.get(configmap_key.clone()).await {
        Ok(kvs) if !kvs.is_empty() => {
            let configmap_yaml = String::from_utf8_lossy(&kvs[0].value);
            info!(
                "🔍 Loading ConfigMap '{}' from etcd: {} bytes",
                configmap_name,
                configmap_yaml.len()
            );

            match serde_yaml::from_str::<Value>(&configmap_yaml) {
                Ok(configmap_obj) => Ok(configmap_obj.get("data").cloned()),
                Err(e) => {
                    error!(
                        "❌ Failed to parse ConfigMap '{}' YAML: {}",
                        configmap_name, e
                    );
                    Err(anyhow::anyhow!("Failed to parse ConfigMap YAML: {}", e))
                }
            }
        }
        Ok(_) => {
            info!(
                "🔍 ConfigMap '{}' not found in etcd (key: {})",
                configmap_name, configmap_key
            );
            Ok(None)
        }
        Err(e) => {
            error!(
                "❌ Failed to query etcd for ConfigMap '{}': {}",
                configmap_name, e
            );
            Err(anyhow::anyhow!("Failed to query etcd for ConfigMap: {}", e))
        }
    }
}

pub async fn load_secret_from_etcd(
    etcd_manager: &Arc<EtcdManager>,
    cluster_name: &str,
    secret_name: &str,
) -> Result<Option<Value>> {
    let secret_key = format!("/nokube/{}/secrets/{}", cluster_name, secret_name);

    match etcd_manager.get(secret_key.clone()).await {
        Ok(kvs) if !kvs.is_empty() => {
            let secret_yaml = String::from_utf8_lossy(&kvs[0].value);
            info!(
                "🔍 Loading Secret '{}' from etcd: {} bytes",
                secret_name,
                secret_yaml.len()
            );

            match serde_yaml::from_str::<Value>(&secret_yaml) {
                Ok(secret_obj) => Ok(secret_obj.get("data").cloned()),
                Err(e) => {
                    error!("❌ Failed to parse Secret '{}' YAML: {}", secret_name, e);
                    Err(anyhow::anyhow!("Failed to parse Secret YAML: {}", e))
                }
            }
        }
        Ok(_) => {
            info!(
                "🔍 Secret '{}' not found in etcd (key: {})",
                secret_name, secret_key
            );
            Ok(None)
        }
        Err(e) => {
            error!(
                "❌ Failed to query etcd for Secret '{}': {}",
                secret_name, e
            );
            Err(anyhow::anyhow!("Failed to query etcd for Secret: {}", e))
        }
    }
}
