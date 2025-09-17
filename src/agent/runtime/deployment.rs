use anyhow::Result;
use base64::Engine as _;
use serde_yaml::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::agent::general::{DockerRunConfig, DockerRunner};
use crate::config::etcd_manager::EtcdManager;

pub const POD_CONTAINER_SEPARATOR: &str = "__";

fn extract_deployment_spec<'a>(deployment_yaml: &'a Value) -> Result<&'a Value> {
    let spec = deployment_yaml
        .get("spec")
        .ok_or_else(|| anyhow::anyhow!("Missing spec in deployment"))?;

    if spec.get("template").is_some() {
        Ok(spec)
    } else if let Some(deploy) = spec.get("deployment") {
        Ok(deploy)
    } else {
        Err(anyhow::anyhow!("Missing template or deployment in spec"))
    }
}

#[derive(Clone, Debug)]
struct ContainerTemplate {
    k8s_name: String,
    docker_name: String,
    spec: Value,
}

#[derive(Clone, Debug)]
struct PodTemplate {
    pod_name: String,
    containers: Vec<ContainerTemplate>,
}

#[derive(Clone, Debug)]
struct PreparedVolume {
    host_path: String,
    read_only: bool,
}

#[derive(Clone, Debug)]
pub struct PodContainerRecord {
    pub name: String,
    pub docker_name: String,
    pub status: String,
    pub container_id: Option<String>,
}

async fn prepare_named_volume(
    volume: &Value,
    workspace: &str,
    cluster_name: &str,
    etcd_manager: &Arc<EtcdManager>,
) -> Result<Option<(String, PreparedVolume)>> {
    let Some(volume_name) = volume.get("name").and_then(|v| v.as_str()) else {
        warn!("Encountered volume definition without name, skipping");
        return Ok(None);
    };

    if let Some(config_map) = volume.get("configMap") {
        if let Some(config_name) = config_map.get("name").and_then(|v| v.as_str()) {
            let sanitized = sanitize_name_component(config_name, "configmap");
            let volume_dir = format!("{}/configmaps/{}", workspace, sanitized);
            std::fs::create_dir_all(&volume_dir)?;
            match load_configmap_from_etcd(etcd_manager, cluster_name, config_name).await? {
                Some(data) => {
                    if let Some(map) = data.as_mapping() {
                        for (filename, content) in map {
                            if let (Some(name), Some(value)) = (filename.as_str(), content.as_str())
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

            return Ok(Some((
                volume_name.to_string(),
                PreparedVolume {
                    host_path: volume_dir,
                    read_only: false,
                },
            )));
        }
    }

    if let Some(secret) = volume.get("secret") {
        if let Some(secret_name) = secret.get("secretName").and_then(|v| v.as_str()) {
            let sanitized = sanitize_name_component(secret_name, "secret");
            let volume_dir = format!("{}/secrets/{}", workspace, sanitized);
            std::fs::create_dir_all(&volume_dir)?;
            match load_secret_from_etcd(etcd_manager, cluster_name, secret_name).await? {
                Some(secret_data) => {
                    if let Some(map) = secret_data.as_mapping() {
                        for (filename, content) in map {
                            if let (Some(name), Some(value)) = (filename.as_str(), content.as_str())
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

            return Ok(Some((
                volume_name.to_string(),
                PreparedVolume {
                    host_path: volume_dir,
                    read_only: true,
                },
            )));
        }
    }

    if let Some(host_path) = volume
        .get("hostPath")
        .and_then(|hp| hp.get("path"))
        .and_then(|v| v.as_str())
    {
        return Ok(Some((
            volume_name.to_string(),
            PreparedVolume {
                host_path: host_path.to_string(),
                read_only: volume
                    .get("readOnly")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            },
        )));
    }

    warn!(
        "Unsupported or empty volume definition for '{}', skipping",
        volume_name
    );
    Ok(None)
}

fn sanitize_name_component(name: &str, fallback: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    let mut sanitized: String = lower
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    while sanitized.starts_with('-') {
        sanitized.remove(0);
    }
    while sanitized.ends_with('-') {
        sanitized.pop();
    }

    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

fn build_docker_container_name(pod_name: &str, container_name: &str) -> String {
    format!(
        "nokube-pod-{}{}{}",
        sanitize_name_component(pod_name, "pod"),
        POD_CONTAINER_SEPARATOR,
        sanitize_name_component(container_name, "main")
    )
}

fn normalize_container_specs(container_spec: &Value) -> Result<Vec<Value>> {
    match container_spec {
        Value::Sequence(seq) => Ok(seq.iter().cloned().collect()),
        Value::Mapping(_) => Ok(vec![container_spec.clone()]),
        _ => Err(anyhow::anyhow!("containerSpec must be mapping or sequence")),
    }
}

fn prepare_pod_template(deployment_yaml: &Value, deployment_name: &str) -> Result<PodTemplate> {
    let deployment_spec = extract_deployment_spec(deployment_yaml)?;
    let container_values = if let Some(template) = deployment_spec.get("template") {
        if let Some(template_spec) = template.get("spec") {
            if let Some(containers) = template_spec
                .get("containers")
                .and_then(|c| c.as_sequence())
            {
                containers.iter().cloned().collect()
            } else if let Some(container_spec) = template.get("containerSpec") {
                normalize_container_specs(container_spec)?
            } else {
                return Err(anyhow::anyhow!(
                    "Missing containers or containerSpec in template spec"
                ));
            }
        } else if let Some(container_spec) = template.get("containerSpec") {
            normalize_container_specs(container_spec)?
        } else {
            return Err(anyhow::anyhow!(
                "Missing template.spec or template.containerSpec"
            ));
        }
    } else if let Some(container_spec) = deployment_spec.get("containerSpec") {
        normalize_container_specs(container_spec)?
    } else if let Some(containers) = deployment_spec
        .get("containers")
        .and_then(|c| c.as_sequence())
    {
        containers.iter().cloned().collect()
    } else {
        return Err(anyhow::anyhow!(
            "Missing container definition in deployment"
        ));
    };

    let mut containers = Vec::new();
    for (idx, container_value) in container_values.into_iter().enumerate() {
        let raw_name = container_value
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("container{}", idx));
        let docker_name = build_docker_container_name(deployment_name, &raw_name);
        containers.push(ContainerTemplate {
            k8s_name: raw_name,
            docker_name,
            spec: container_value,
        });
    }

    Ok(PodTemplate {
        pod_name: deployment_name.to_string(),
        containers,
    })
}

pub fn enumerate_actor_container_names(
    deployment_yaml: &Value,
    deployment_name: &str,
) -> Result<Vec<String>> {
    let template = prepare_pod_template(deployment_yaml, deployment_name)?;
    Ok(template
        .containers
        .into_iter()
        .map(|c| c.docker_name)
        .collect())
}

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

    let deployment_spec = extract_deployment_spec(deployment_yaml)?;
    let pod_template = prepare_pod_template(deployment_yaml, deployment_name)?;
    let template_spec = deployment_spec.get("template").and_then(|t| t.get("spec"));

    let mut prepared_volumes: HashMap<String, PreparedVolume> = HashMap::new();

    if let Some(template_spec) = template_spec {
        if let Some(volumes) = template_spec.get("volumes").and_then(|v| v.as_sequence()) {
            for volume in volumes {
                if let Some((name, prepared)) =
                    prepare_named_volume(volume, workspace, cluster_name, etcd_manager).await?
                {
                    prepared_volumes.insert(name, prepared);
                }
            }
        }
    }

    if let Some(volumes) = deployment_spec.get("volumes").and_then(|v| v.as_sequence()) {
        for volume in volumes {
            if let Some((name, prepared)) =
                prepare_named_volume(volume, workspace, cluster_name, etcd_manager).await?
            {
                prepared_volumes.insert(name, prepared);
            }
        }
    }

    let yaml_text = serde_yaml::to_string(deployment_yaml).unwrap_or_default();
    let checksum = calc_hash_u64(&yaml_text);

    let mut container_records: Vec<PodContainerRecord> = Vec::new();
    let mut last_error: Option<String> = None;

    for container in pod_template.containers {
        let container_spec = &container.spec;
        let image = container_spec
            .get("image")
            .and_then(|v| v.as_str())
            .unwrap_or("python:3.10-slim");

        let mut config = DockerRunConfig::new(container.docker_name.clone(), image.to_string())
            .restart_policy("unless-stopped".to_string());

        if let Some(env_obj) = container_spec.get("env") {
            if let Some(env_map) = env_obj.as_mapping() {
                for (k, v) in env_map {
                    if let (Some(key), Some(value)) = (k.as_str(), v.as_str()) {
                        config = config.add_env(key.to_string(), value.to_string());
                    }
                }
            } else if let Some(env_array) = env_obj.as_sequence() {
                for env_item in env_array {
                    if let (Some(name), Some(value)) = (
                        env_item.get("name").and_then(|v| v.as_str()),
                        env_item.get("value").and_then(|v| v.as_str()),
                    ) {
                        config = config.add_env(name.to_string(), value.to_string());
                    }
                }
            }
        }

        for key in [
            "http_proxy",
            "https_proxy",
            "no_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "NO_PROXY",
        ] {
            if !config.environment.contains_key(key) {
                if let Ok(val) = std::env::var(key) {
                    if !val.is_empty() {
                        config = config.add_env(key.to_string(), val);
                    }
                }
            }
        }

        if let Some(configmap_data) = spec.get("configMap").and_then(|cm| cm.get("data")) {
            if let Some(map) = configmap_data.as_mapping() {
                let config_dir = format!(
                    "{}/configmaps/{}",
                    workspace,
                    sanitize_name_component(deployment_name, "deployment")
                );
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

        if let Some(volume_mounts) = container_spec
            .get("volumeMounts")
            .and_then(|vm| vm.as_sequence())
        {
            for mount in volume_mounts {
                let Some(mount_name) = mount.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                let Some(mount_path) = mount.get("mountPath").and_then(|v| v.as_str()) else {
                    continue;
                };
                if let Some(prepared) = prepared_volumes.get(mount_name) {
                    let read_only = mount
                        .get("readOnly")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(prepared.read_only);
                    config = config.add_volume(
                        prepared.host_path.clone(),
                        mount_path.to_string(),
                        read_only,
                    );
                } else {
                    warn!(
                        "Volume mount '{}' requested by container '{}' but no volume prepared",
                        mount_name, container.k8s_name
                    );
                }
            }
        }

        let mut container_command = Vec::new();
        if let Some(cmd_seq) = container_spec.get("command").and_then(|v| v.as_sequence()) {
            container_command.extend(
                cmd_seq
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string())),
            );
        }
        if let Some(args_seq) = container_spec.get("args").and_then(|v| v.as_sequence()) {
            container_command.extend(
                args_seq
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string())),
            );
        }
        if !container_command.is_empty() {
            config = config.command(container_command);
        }

        config.extra_args.push("--label".to_string());
        config
            .extra_args
            .push(format!("nokube.actor.checksum={}", checksum));

        match DockerRunner::run(&config) {
            Ok(container_id) => {
                info!(
                    "Created Docker container (actor={}): {} => {}",
                    deployment_name, container.docker_name, container_id
                );
                container_records.push(PodContainerRecord {
                    name: container.k8s_name.clone(),
                    docker_name: container.docker_name.clone(),
                    status: "Running".to_string(),
                    container_id: Some(container_id),
                });
            }
            Err(e) => {
                let err_str = format!("{}", e);
                error!(
                    "Failed to create container {} for deployment {}: {}",
                    container.k8s_name, deployment_name, err_str
                );
                last_error = Some(err_str.clone());
                container_records.push(PodContainerRecord {
                    name: container.k8s_name.clone(),
                    docker_name: container.docker_name.clone(),
                    status: "Failed".to_string(),
                    container_id: None,
                });
            }
        }
    }

    let overall_status = if container_records.iter().all(|c| c.status == "Running") {
        "Running"
    } else {
        "Failed"
    };

    store_pod_status(
        etcd_manager,
        cluster_name,
        deployment_name,
        overall_status,
        &container_records,
        last_error.as_deref(),
    )
    .await?;

    Ok(())
}

pub async fn store_pod_status(
    etcd_manager: &Arc<EtcdManager>,
    cluster_name: &str,
    pod_name: &str,
    status: &str,
    containers: &[PodContainerRecord],
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

    if let Some(first_running) = containers
        .iter()
        .find(|c| c.status == "Running")
        .and_then(|c| c.container_id.clone())
    {
        pod_info["container_id"] = serde_json::Value::String(first_running);
    }

    pod_info["containers"] = serde_json::Value::Array(
        containers
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "docker_name": c.docker_name,
                    "status": c.status,
                    "container_id": c.container_id,
                })
            })
            .collect(),
    );

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
