use crate::agent::general::docker_runner::DockerRunner;
use anyhow::Result;
use base64::Engine;
use chrono::{DateTime, Utc};
use opentelemetry::global;
use opentelemetry::logs::{Logger, LoggerProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_otlp::{ExportConfig, LogExporter};
use opentelemetry_sdk::logs::{Config, LoggerProvider};
use opentelemetry_sdk::Resource;
use reqwest;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

// Global cache to hold leaked &'static str for dynamic scope names (container_path)
static SCOPE_CACHE: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
fn get_or_leak_scope_name(s: &str) -> &'static str {
    let cache = SCOPE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap();
    if let Some(&v) = map.get(s) {
        return v;
    }
    let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
    map.insert(s.to_string(), leaked);
    leaked
}

/// 日志条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub level: String,
    pub source: String,    // service_agent, grafana, pod, etc.
    pub source_id: String, // 具体的进程ID或名称
    pub cluster_name: String,
    pub node_name: String,
    pub message: String,
    pub fields: HashMap<String, String>, // 额外字段
}

/// 日志收集器配置
#[derive(Debug, Clone)]
pub struct LogCollectorConfig {
    pub cluster_name: String,
    pub node_name: String,
    pub otlp_logs_endpoint: String,
    pub batch_size: usize,
    pub flush_interval_secs: u64,
    pub flush_timeout_secs: u64,
    pub auth_user: Option<String>,
    pub auth_password: Option<String>,
}

/// 日志收集器
pub struct LogCollector {
    config: LogCollectorConfig,
    log_buffer: Vec<LogEntry>,
    log_rx: mpsc::Receiver<LogEntry>,
    log_tx: mpsc::Sender<LogEntry>,
    client: reqwest::Client,
    logger_provider: LoggerProvider,
}

impl LogCollector {
    pub fn new(config: LogCollectorConfig) -> Result<Self> {
        let (log_tx, log_rx) = mpsc::channel(10000);
        let client = reqwest::Client::new();

        // 设置 OpenTelemetry 全局错误处理器
        global::set_error_handler(|err| {
            error!("OpenTelemetry error: {:?}", err);
        })
        .expect("Failed to set OpenTelemetry error handler");

        // 创建 OpenTelemetry LoggerProvider
        let resource = Resource::new(vec![
            KeyValue::new("service.name", "nokube-rs"),
            KeyValue::new("service.version", "0.1.0"),
        ]);

        // 构建OTLP导出器 - 使用HTTP协议发送protobuf
        // 注意：此处 endpoint 需要是完整的日志路径，例如：
        //   https://<host>/v1/otlp/v1/logs
        // 我们直接使用传入的 greptimedb_url 作为完整endpoint（不再追加路径）。
        let final_endpoint = config.otlp_logs_endpoint.trim_end_matches('/').to_string();
        info!("Using OTLP logs endpoint: {}", final_endpoint);
        let export_config = ExportConfig {
            endpoint: final_endpoint,
            ..Default::default()
        };

        // 使用环境变量为 OTLP 请求添加 Greptime 所需请求头（库对 headers 支持有限，采用内部设置 env 方式）
        let headers_str = "X-Greptime-DB-Name=public,\
X-Greptime-Log-Table-Name=opentelemetry_logs,\
X-Greptime-Log-Extract-Keys=cluster_name,node_name,source,source_id,container,pod,root_actor,container_path";
        // 若 ClusterConfig 提供了凭证（通过 LogCollectorConfig 传入），追加 Authorization 头
        // let mut headers_combined = headers_str.to_string();
        // if let Some(user) = config.auth_user.clone() {
        //     let pass = config.auth_password.clone().unwrap_or_default();
        //     if !user.is_empty() {
        //         let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", user, pass));
        //         headers_combined.push_str(&format!(",Authorization=Basic {}", token));
        //     }
        // }
        // std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", &headers_combined);
        // std::env::set_var("OTEL_EXPORTER_OTLP_LOGS_HEADERS", &headers_combined);
        // debug!(
        //     "OTLP headers set: db=public, table=opentelemetry_logs, extract_keys=cluster_name,node_name,source,source_id,container,pod,root_actor,container_path, auth_user_present={}, auth_password_set={}",
        //     config.auth_user.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
        //     config.auth_password.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
        // );

        // 创建导出器（使用默认 HTTP 配置；headers 由环境变量传入）
        let exporter = LogExporter::new_http(export_config, Default::default())?;

        // 创建 LoggerProvider 并配置为使用当前的 Tokio runtime
        let logger_provider = LoggerProvider::builder()
            .with_config(Config::default().with_resource(resource))
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .build();

        Ok(Self {
            config,
            log_buffer: Vec::new(),
            log_rx,
            log_tx,
            client,
            logger_provider,
        })
    }

    /// 获取日志发送器
    pub fn get_log_sender(&self) -> mpsc::Sender<LogEntry> {
        self.log_tx.clone()
    }

    /// 启动日志收集器
    pub async fn start(&mut self) -> Result<()> {
        info!(
            "Starting log collector for cluster: {}",
            self.config.cluster_name
        );

        // 启动日志处理协程
        let log_rx = std::mem::replace(&mut self.log_rx, mpsc::channel(1).1);
        let config = self.config.clone();
        let logger_provider = self.logger_provider.clone();

        tokio::spawn(async move {
            Self::run_log_processor(log_rx, config, logger_provider).await;
        });

        info!("Log collector started");
        Ok(())
    }

    /// 日志处理协程
    async fn run_log_processor(
        mut log_rx: mpsc::Receiver<LogEntry>,
        config: LogCollectorConfig,
        logger_provider: LoggerProvider,
    ) {
        let mut buffer = Vec::new();
        let mut flush_interval = interval(Duration::from_secs(config.flush_interval_secs));

        loop {
            tokio::select! {
                // 接收新日志
                log_entry = log_rx.recv() => {
                    match log_entry {
                        Some(entry) => {
                            buffer.push(entry);

                            // 如果缓冲区满了，立即刷新
                            if buffer.len() >= config.batch_size {
                                Self::flush_logs_otlp(&buffer, &logger_provider, config.flush_timeout_secs).await;
                                buffer.clear();
                            }
                        }
                        None => {
                            // 通道关闭，刷新剩余日志并退出
                            if !buffer.is_empty() {
                                Self::flush_logs_otlp(&buffer, &logger_provider, config.flush_timeout_secs).await;
                            }
                            break;
                        }
                    }
                }

                // 定期刷新
                _ = flush_interval.tick() => {
                    if !buffer.is_empty() {
                        Self::flush_logs_otlp(&buffer, &logger_provider, config.flush_timeout_secs).await;
                        buffer.clear();
                    }
                }
            }
        }
    }

    /// 将日志批量发送到 GreptimeDB 使用 OpenTelemetry OTLP protobuf 格式
    async fn flush_logs_otlp(
        logs: &[LogEntry],
        logger_provider: &LoggerProvider,
        timeout_secs: u64,
    ) {
        if logs.is_empty() {
            return;
        }

        info!(
            "Flushing3 {} log entries to GreptimeDB using OTLP",
            logs.len()
        );

        // 外层超时保护：防止卡住导致既无成功也无失败
        let timeout_duration = Duration::from_secs(timeout_secs);
        match tokio::time::timeout(
            timeout_duration,
            Self::try_flush_logs(logs, logger_provider),
        )
        .await
        {
            Ok(Ok(())) => {
                info!("Successfully flushed {} log entries", logs.len());
            }
            Ok(Err(e)) => {
                error!("Failed to flush logs: {}", e);
            }
            Err(_) => {
                error!("Flush logs timed out after {:?}", timeout_duration);
            }
        }
    }

    /// 尝试刷新日志的具体实现
    async fn try_flush_logs(logs: &[LogEntry], logger_provider: &LoggerProvider) -> Result<()> {
        for log_entry in logs {
            // 创建日志记录
            let mut log_record = opentelemetry::logs::LogRecord::default();

            // 设置基本属性
            log_record.timestamp = Some(
                std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_nanos(
                        log_entry.timestamp.timestamp_nanos_opt().unwrap_or(0) as u64,
                    ),
            );
            log_record.body = Some(log_entry.message.clone().into());
            log_record.severity_text = Some(log_entry.level.clone().into());
            log_record.severity_number = Some(Self::severity_to_otel_number(&log_entry.level));

            // 添加属性 - 使用正确的属性格式
            let mut attributes = Vec::new();
            attributes.push(("cluster_name".into(), log_entry.cluster_name.clone().into()));
            attributes.push(("node_name".into(), log_entry.node_name.clone().into()));
            attributes.push(("source".into(), log_entry.source.clone().into()));
            attributes.push(("source_id".into(), log_entry.source_id.clone().into()));

            // 统一容器字段命名，便于查询/链接
            attributes.push(("container".into(), log_entry.source_id.clone().into()));
            let mut scope_name: Option<&'static str> = None;
            if log_entry.source == "actor" {
                // 基于 exporter 的解析逻辑，推导 root_actor / pod / container_path
                let (pod_name, container_segment, root_actor, _owner_type) =
                    crate::agent::general::exporter::Exporter::derive_actor_core(
                        &log_entry.source_id,
                        &log_entry.node_name,
                        &log_entry.cluster_name,
                    );
                let container_path = format!(
                    "{}/{}/{}/{}",
                    log_entry.cluster_name, root_actor, pod_name, container_segment
                );
                attributes.push(("pod".into(), pod_name.into()));
                attributes.push(("container_name".into(), container_segment.into()));
                attributes.push(("root_actor".into(), root_actor.into()));
                attributes.push(("container_path".into(), container_path.clone().into()));
                scope_name = Some(get_or_leak_scope_name(&container_path));
            }

            // 添加额外字段
            for (key, value) in &log_entry.fields {
                attributes.push((key.clone().into(), value.clone().into()));
            }

            log_record.attributes = Some(attributes);

            // 发送日志记录：使用 per-entry scope_name（actor 用 container_path，其它用默认）
            let logger = if let Some(scope) = scope_name {
                logger_provider.logger(scope)
            } else {
                logger_provider.logger("nokube-rs")
            };
            logger.emit(log_record);
        }

        // 强制导出所有待发送的日志 - 捕获更详细的错误信息
        // 注意：force_flush 是阻塞性的，这里放到 spawn_blocking 里以便超时能够生效
        let provider = logger_provider.clone();
        let results = tokio::task::spawn_blocking(move || provider.force_flush())
            .await
            .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {}", e))?;

        // force_flush 返回 Vec<ExportResult>
        let result_count = results.len();
        let mut success_count = 0;
        let mut error_count = 0;
        let mut last_error: Option<Box<dyn std::error::Error + Send + Sync>> = None;

        for (index, result) in results.iter().enumerate() {
            match result {
                Ok(_) => {
                    success_count += 1;
                    debug!("Successfully flushed log batch {} to GreptimeDB", index + 1);
                }
                Err(e) => {
                    error_count += 1;
                    error!(
                        "Failed to flush log batch {} to GreptimeDB: {:?}",
                        index + 1,
                        e
                    );
                    // 尝试提取更多错误细节
                    let error_string = format!("{:?}", e);
                    if error_string.contains("SendError") {
                        warn!("OpenTelemetry SendError detected - this usually indicates channel/connection issues");
                    }
                    if error_string.contains("no reactor running") {
                        warn!("Tokio reactor issue detected - this may be a runtime configuration problem");
                    }

                    // 保存最后一个错误用于返回
                    last_error = Some(format!("OpenTelemetry LogError: {:?}", e).into());
                }
            }
        }

        if result_count > 0 {
            info!(
                "Flushed {} log entries: {} successful, {} failed",
                logs.len(),
                success_count,
                error_count
            );
        } else {
            // 未返回任何结果——对此进行显式告警，便于现场排查（常见于头部/鉴权/endpoint 配置问题）
            let hdr_all = std::env::var("OTEL_EXPORTER_OTLP_HEADERS")
                .unwrap_or_else(|_| "<unset>".to_string());
            let hdr_logs = std::env::var("OTEL_EXPORTER_OTLP_LOGS_HEADERS")
                .unwrap_or_else(|_| "<unset>".to_string());
            warn!(
                "OTLP force_flush returned 0 results; headers(all)={}, headers(logs)={}. This often indicates HTTP headers/authorization or endpoint issues.",
                hdr_all, hdr_logs
            );
            return Err(anyhow::anyhow!("OTLP force_flush returned 0 results"));
        }

        // 如果有错误，返回错误
        if error_count > 0 {
            if let Some(err) = last_error {
                return Err(anyhow::anyhow!("Log flush failed: {}", err));
            } else {
                return Err(anyhow::anyhow!(
                    "Log flush failed with {} errors",
                    error_count
                ));
            }
        }

        Ok(())
    }

    /// 将日志级别转换为 OpenTelemetry severity number
    fn severity_to_otel_number(level: &str) -> opentelemetry::logs::Severity {
        use opentelemetry::logs::Severity;
        match level.to_uppercase().as_str() {
            "TRACE" => Severity::Trace,
            "DEBUG" => Severity::Debug,
            "INFO" => Severity::Info,
            "WARN" | "WARNING" => Severity::Warn,
            "ERROR" => Severity::Error,
            "FATAL" | "CRITICAL" => Severity::Fatal,
            _ => Severity::Info, // 默认为Info级别
        }
    }

    /// 发送日志条目
    pub async fn log(&self, entry: LogEntry) {
        if let Err(e) = self.log_tx.send(entry).await {
            error!("Failed to send log entry to collector: {:?}", e);
            warn!("Log collector channel may be closed or full");
        }
    }

    /// 收集Docker容器日志
    pub async fn collect_docker_logs(&self, container_name: &str) -> Result<()> {
        info!(
            "Starting to collect Docker logs for container: {}",
            container_name
        );

        let sender = self.log_tx.clone();
        let cluster_name = self.config.cluster_name.clone();
        let node_name = self.config.node_name.clone();
        let container = container_name.to_string();

        tokio::spawn(async move {
            let runtime_path =
                DockerRunner::get_runtime_path().unwrap_or_else(|_| "docker".to_string());
            let mut cmd = Command::new(runtime_path);
            cmd.args(&["logs", "-f", &container]);

            match cmd.output() {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let is_actor = container.starts_with("nokube-pod-");
                    let source_label = if is_actor { "actor" } else { "docker" };

                    // 处理stdout
                    for line in stdout.lines() {
                        if !line.trim().is_empty() {
                            let msg = format!("[{}] {}", container, line);
                            let entry = LogEntry {
                                timestamp: Utc::now(),
                                level: "INFO".to_string(),
                                source: source_label.to_string(),
                                source_id: container.clone(),
                                cluster_name: cluster_name.clone(),
                                node_name: node_name.clone(),
                                message: msg,
                                fields: {
                                    let mut f = HashMap::new();
                                    f.insert("container".to_string(), container.clone());
                                    if is_actor {
                                        let (pod_name, container_segment, root_actor, _owner) = crate::agent::general::exporter::Exporter::derive_actor_core(
                                            &container, &node_name, &cluster_name);
                                        f.insert("pod".to_string(), pod_name.clone());
                                        f.insert(
                                            "container_name".to_string(),
                                            container_segment.clone(),
                                        );
                                        f.insert("root_actor".to_string(), root_actor.clone());
                                        let path = format!(
                                            "{}/{}/{}/{}",
                                            cluster_name, root_actor, pod_name, container_segment
                                        );
                                        f.insert("container_path".to_string(), path);
                                    }
                                    f
                                },
                            };

                            if let Err(e) = sender.send(entry).await {
                                error!("Failed to send Docker stdout log entry: {:?}", e);
                                break;
                            } else {
                                debug!(
                                    "Sent Docker stdout log entry: container={}, level=INFO",
                                    container
                                );
                            }
                        }
                    }

                    // 处理stderr
                    for line in stderr.lines() {
                        if !line.trim().is_empty() {
                            let msg = format!("[{}] {}", container, line);
                            let entry = LogEntry {
                                timestamp: Utc::now(),
                                level: "ERROR".to_string(),
                                source: source_label.to_string(),
                                source_id: container.clone(),
                                cluster_name: cluster_name.clone(),
                                node_name: node_name.clone(),
                                message: msg,
                                fields: {
                                    let mut f = HashMap::new();
                                    f.insert("container".to_string(), container.clone());
                                    if is_actor {
                                        let (pod_name, container_segment, root_actor, _owner) = crate::agent::general::exporter::Exporter::derive_actor_core(
                                            &container, &node_name, &cluster_name);
                                        f.insert("pod".to_string(), pod_name.clone());
                                        f.insert(
                                            "container_name".to_string(),
                                            container_segment.clone(),
                                        );
                                        f.insert("root_actor".to_string(), root_actor.clone());
                                        let path = format!(
                                            "{}/{}/{}/{}",
                                            cluster_name, root_actor, pod_name, container_segment
                                        );
                                        f.insert("container_path".to_string(), path);
                                    }
                                    f
                                },
                            };

                            if let Err(e) = sender.send(entry).await {
                                error!("Failed to send Docker stderr log entry: {:?}", e);
                                break;
                            } else {
                                debug!(
                                    "Sent Docker stderr log entry: container={}, level=ERROR",
                                    container
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to collect Docker logs for {}: {}", container, e);
                }
            }
        });

        Ok(())
    }

    /// 持续收集Docker容器日志 (follow模式)
    pub async fn follow_docker_logs(&self, container_name: &str) -> Result<()> {
        info!(
            "Starting to follow Docker logs for container: {}",
            container_name
        );

        let sender = self.log_tx.clone();
        let cluster_name = self.config.cluster_name.clone();
        let node_name = self.config.node_name.clone();
        let container = container_name.to_string();

        tokio::spawn(async move {
            loop {
                let runtime_path =
                    DockerRunner::get_runtime_path().unwrap_or_else(|_| "docker".to_string());
                let mut cmd = tokio::process::Command::new(runtime_path);
                cmd.args(&["logs", "-f", "--tail", "10", &container]);
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());

                match cmd.spawn() {
                    Ok(mut child) => {
                        if let Some(stdout) = child.stdout.take() {
                            use tokio::io::{AsyncBufReadExt, BufReader};
                            let reader = BufReader::new(stdout);
                            let mut lines = reader.lines();
                            let is_actor = container.starts_with("nokube-pod-");
                            let source_label = if is_actor { "actor" } else { "docker" };

                            while let Ok(Some(line)) = lines.next_line().await {
                                if !line.trim().is_empty() {
                                    let msg = format!("[{}] {}", container, line);
                                    let entry = LogEntry {
                                        timestamp: Utc::now(),
                                        level: "INFO".to_string(),
                                        source: source_label.to_string(),
                                        source_id: container.clone(),
                                        cluster_name: cluster_name.clone(),
                                        node_name: node_name.clone(),
                                        message: msg,
                                        fields: HashMap::new(),
                                    };

                                    if let Err(e) = sender.send(entry).await {
                                        error!("Failed to send follow Docker log entry: {:?}", e);
                                        break;
                                    } else {
                                        debug!("Sent follow Docker log entry: container={}, level=INFO", container);
                                    }
                                }
                            }
                        }
                        // 处理 stderr 流
                        if let Some(stderr) = child.stderr.take() {
                            let sender_err = sender.clone();
                            let container_err = container.clone();
                            let cluster_err = cluster_name.clone();
                            let node_err = node_name.clone();
                            let is_actor = container_err.starts_with("nokube-pod-");
                            let source_label = if is_actor { "actor" } else { "docker" };
                            tokio::spawn(async move {
                                use tokio::io::{AsyncBufReadExt, BufReader};
                                let reader = BufReader::new(stderr);
                                let mut lines = reader.lines();
                                while let Ok(Some(line)) = lines.next_line().await {
                                    if !line.trim().is_empty() {
                                        let msg = format!("[{}] {}", container_err, line);
                                        let entry = LogEntry {
                                            timestamp: Utc::now(),
                                            level: "ERROR".to_string(),
                                            source: source_label.to_string(),
                                            source_id: container_err.clone(),
                                            cluster_name: cluster_err.clone(),
                                            node_name: node_err.clone(),
                                            message: msg,
                                            fields: HashMap::new(),
                                        };
                                        if let Err(e) = sender_err.send(entry).await {
                                            error!("Failed to send follow Docker stderr log entry: {:?}", e);
                                            break;
                                        } else {
                                            debug!("Sent follow Docker stderr log entry: container={}, level=ERROR", container_err);
                                        }
                                    }
                                }
                            });
                        }

                        let _ = child.wait().await;
                    }
                    Err(e) => {
                        error!(
                            "Failed to spawn docker logs command for {}: {}",
                            container, e
                        );
                    }
                }

                // 重连前等待一段时间
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        Ok(())
    }
}

/// GreptimeDB日志写入器的trait
pub trait GreptimeDBLogger {
    fn log_info(&self, source: &str, source_id: &str, message: &str);
    fn log_warn(&self, source: &str, source_id: &str, message: &str);
    fn log_error(&self, source: &str, source_id: &str, message: &str);
}

/// GreptimeDB日志写入器
pub struct GreptimeDBLogWriter {
    sender: mpsc::Sender<LogEntry>,
    cluster_name: String,
    node_name: String,
}

impl GreptimeDBLogWriter {
    pub fn new(sender: mpsc::Sender<LogEntry>, cluster_name: String, node_name: String) -> Self {
        Self {
            sender,
            cluster_name,
            node_name,
        }
    }
}

impl GreptimeDBLogger for GreptimeDBLogWriter {
    fn log_info(&self, source: &str, source_id: &str, message: &str) {
        let entry = LogEntry {
            timestamp: Utc::now(),
            level: "INFO".to_string(),
            source: source.to_string(),
            source_id: source_id.to_string(),
            cluster_name: self.cluster_name.clone(),
            node_name: self.node_name.clone(),
            message: message.to_string(),
            fields: HashMap::new(),
        };

        // 非阻塞发送
        if let Err(_) = self.sender.try_send(entry) {
            eprintln!("Failed to send log entry to collector");
        }
    }

    fn log_warn(&self, source: &str, source_id: &str, message: &str) {
        let entry = LogEntry {
            timestamp: Utc::now(),
            level: "WARN".to_string(),
            source: source.to_string(),
            source_id: source_id.to_string(),
            cluster_name: self.cluster_name.clone(),
            node_name: self.node_name.clone(),
            message: message.to_string(),
            fields: HashMap::new(),
        };

        if let Err(_) = self.sender.try_send(entry) {
            eprintln!("Failed to send log entry to collector");
        }
    }

    fn log_error(&self, source: &str, source_id: &str, message: &str) {
        let entry = LogEntry {
            timestamp: Utc::now(),
            level: "ERROR".to_string(),
            source: source.to_string(),
            source_id: source_id.to_string(),
            cluster_name: self.cluster_name.clone(),
            node_name: self.node_name.clone(),
            message: message.to_string(),
            fields: HashMap::new(),
        };

        if let Err(_) = self.sender.try_send(entry) {
            eprintln!("Failed to send log entry to collector");
        }
    }
}
