use crate::config::cluster_config::{ClusterConfig, NodeRole};

/// 解析 Head 节点的 IP（如果存在）
pub fn head_node_ip(cfg: &ClusterConfig) -> Option<String> {
    cfg.nodes
        .iter()
        .find(|n| matches!(n.role, NodeRole::Head))
        .and_then(|n| n.get_ip().ok().map(|s| s.to_string()))
}

/// GreptimeDB HTTP 基础地址，例如: http://<head-ip>:<port>
pub fn greptime_http_base(cfg: &ClusterConfig) -> String {
    if let Some(ip) = head_node_ip(cfg) {
        format!("http://{}:{}", ip, cfg.task_spec.monitoring.greptimedb.port)
    } else {
        format!(
            "http://localhost:{}",
            cfg.task_spec.monitoring.greptimedb.port
        )
    }
}

/// OTLP Logs 完整地址
/// 优先读取环境变量 NOKUBE_OTLP_LOGS_ENDPOINT
/// 否则基于 greptime_http_base 拼接 /v1/otlp/v1/logs
pub fn otlp_logs_endpoint(cfg: &ClusterConfig) -> String {
    if let Ok(v) = std::env::var("NOKUBE_OTLP_LOGS_ENDPOINT") {
        return v;
    }
    format!("{}/v1/otlp/v1/logs", greptime_http_base(cfg))
}

