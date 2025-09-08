use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use anyhow::Result;
use tracing::{info, error};
use crate::error::NokubeError;

/// 支持的容器运行时
#[derive(Debug, Clone)]
pub struct ContainerRuntime {
    /// 可执行文件路径，例如 /usr/bin/docker
    pub path: String,
    /// 名称展示，例如 docker/podman/nerdctl（仅用于日志预览）
    pub name: String,
}

/// Docker 卷挂载配置
#[derive(Debug, Clone)]
pub struct VolumeMount {
    /// 主机路径
    pub host_path: String,
    /// 容器内路径
    pub container_path: String,
    /// 是否只读
    pub read_only: bool,
}

impl VolumeMount {
    pub fn new(host_path: String, container_path: String, read_only: bool) -> Self {
        Self {
            host_path,
            container_path,
            read_only,
        }
    }
    
    /// 转换为 docker run 参数
    pub fn to_docker_arg(&self) -> String {
        if self.read_only {
            format!("{}:{}:ro", self.host_path, self.container_path)
        } else {
            format!("{}:{}", self.host_path, self.container_path)
        }
    }
}

/// Docker 端口映射配置
#[derive(Debug, Clone)]
pub struct PortMapping {
    /// 主机端口
    pub host_port: u16,
    /// 容器端口
    pub container_port: u16,
    /// 协议 (tcp/udp)
    pub protocol: String,
}

impl PortMapping {
    pub fn new(host_port: u16, container_port: u16) -> Self {
        Self {
            host_port,
            container_port,
            protocol: "tcp".to_string(),
        }
    }
    
    /// 转换为 docker run 参数
    pub fn to_docker_arg(&self) -> String {
        format!("{}:{}/{}", self.host_port, self.container_port, self.protocol)
    }
}

/// Docker 运行配置
#[derive(Debug, Clone)]
pub struct DockerRunConfig {
    /// 容器名称
    pub container_name: String,
    /// 镜像名称
    pub image: String,
    /// 卷挂载
    pub volumes: Vec<VolumeMount>,
    /// 端口映射
    pub ports: Vec<PortMapping>,
    /// 环境变量
    pub environment: HashMap<String, String>,
    /// 重启策略
    pub restart_policy: Option<String>,
    /// 是否后台运行
    pub detach: bool,
    /// 是否删除已有同名容器
    pub remove_existing: bool,
    /// 容器启动后执行的命令
    pub command: Option<Vec<String>>,
    /// 工作目录
    pub working_dir: Option<String>,
    /// 用户
    pub user: Option<String>,
    /// 网络模式
    pub network: Option<String>,
    /// 其他额外参数
    pub extra_args: Vec<String>,
}

impl DockerRunConfig {
    pub fn new(container_name: String, image: String) -> Self {
        Self {
            container_name,
            image,
            volumes: Vec::new(),
            ports: Vec::new(),
            environment: HashMap::new(),
            restart_policy: Some("unless-stopped".to_string()),
            detach: true,
            remove_existing: true,
            command: None,
            working_dir: None,
            user: None,
            network: None,
            extra_args: Vec::new(),
        }
    }
    
    /// 添加卷挂载
    pub fn add_volume(mut self, host_path: String, container_path: String, read_only: bool) -> Self {
        self.volumes.push(VolumeMount::new(host_path, container_path, read_only));
        self
    }
    
    /// 添加端口映射
    pub fn add_port(mut self, host_port: u16, container_port: u16) -> Self {
        self.ports.push(PortMapping::new(host_port, container_port));
        self
    }
    
    /// 添加环境变量
    pub fn add_env(mut self, key: String, value: String) -> Self {
        self.environment.insert(key, value);
        self
    }
    
    /// 设置重启策略
    pub fn restart_policy(mut self, policy: String) -> Self {
        self.restart_policy = Some(policy);
        self
    }
    
    /// 设置启动命令
    pub fn command(mut self, command: Vec<String>) -> Self {
        self.command = Some(command);
        self
    }
    
    /// 设置工作目录
    pub fn working_dir(mut self, dir: String) -> Self {
        self.working_dir = Some(dir);
        self
    }
    
    /// 设置用户
    pub fn user(mut self, user: String) -> Self {
        self.user = Some(user);
        self
    }
    
    /// 设置网络
    pub fn network(mut self, network: String) -> Self {
        self.network = Some(network);
        self
    }
    
    /// 校验配置
    pub fn validate(&self) -> Result<()> {
        // 校验容器名称
        if self.container_name.is_empty() {
            return Err(NokubeError::Config("Container name cannot be empty".to_string()).into());
        }
        
        // 校验镜像名称
        if self.image.is_empty() {
            return Err(NokubeError::Config("Image name cannot be empty".to_string()).into());
        }
        
        // 校验所有挂载卷的主机路径是否存在
        for volume in &self.volumes {
            let host_path = Path::new(&volume.host_path);
            if !host_path.exists() {
                return Err(NokubeError::FileOperation {
                    file_path: volume.host_path.clone(),
                    reason: format!("Volume mount source path '{}' does not exist", volume.host_path),
                }.into());
            }
            
            // 如果是文件，检查是否真的是文件
            if host_path.is_file() {
                info!("Volume mount source '{}' is a file", volume.host_path);
            } else if host_path.is_dir() {
                info!("Volume mount source '{}' is a directory", volume.host_path);
            } else {
                return Err(NokubeError::FileOperation {
                    file_path: volume.host_path.clone(),
                    reason: format!("Volume mount source path '{}' is neither a file nor directory", volume.host_path),
                }.into());
            }
        }
        
        // 校验端口范围
        for port in &self.ports {
            if port.host_port == 0 || port.container_port == 0 {
                return Err(NokubeError::Config(format!(
                    "Invalid port mapping: {}:{} - ports must be greater than 0",
                    port.host_port, port.container_port
                )).into());
            }
        }
        
        Ok(())
    }
}

/// Docker 运行器
pub struct DockerRunner;

impl DockerRunner {
    /// 查找可用的容器运行时（docker/podman/nerdctl）
    pub fn find_container_runtime() -> Result<ContainerRuntime> {
        // 候选运行时及其常见路径
        let candidates: Vec<(&str, &[&str])> = vec![
            ("docker", &["/usr/bin/docker", "/usr/local/bin/docker", "/bin/docker", "/snap/bin/docker"]),
            ("podman", &["/usr/bin/podman", "/usr/local/bin/podman", "/bin/podman", "/snap/bin/podman"]),
            ("nerdctl", &["/usr/bin/nerdctl", "/usr/local/bin/nerdctl", "/bin/nerdctl"]),
        ];

        // 1) 直接检查常见路径
        for (name, paths) in &candidates {
            for p in *paths {
                if Path::new(p).exists() {
                    info!("Found container runtime '{}' at: {}", name, p);
                    return Ok(ContainerRuntime { path: p.to_string(), name: (*name).to_string() });
                }
            }
        }

        // 2) 使用 which 查找
        for (name, _paths) in &candidates {
            if let Ok(output) = Command::new("which").arg(name).output() {
                if output.status.success() {
                    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !path.is_empty() && Path::new(&path).exists() {
                        info!("Found container runtime '{}' via which: {}", name, path);
                        return Ok(ContainerRuntime { path, name: (*name).to_string() });
                    }
                }
            }
        }

        // 3) 使用 whereis 查找（仅 docker/podman/nerdctl 名称）
        for (name, _paths) in &candidates {
            if let Ok(output) = Command::new("whereis").arg(name).output() {
                if output.status.success() {
                    let whereis_output = String::from_utf8_lossy(&output.stdout);
                    for part in whereis_output.split_whitespace().skip(1) {
                        if part.ends_with(&format!("/{}", name)) && Path::new(part).exists() {
                            info!("Found container runtime '{}' via whereis: {}", name, part);
                            return Ok(ContainerRuntime { path: part.to_string(), name: (*name).to_string() });
                        }
                    }
                }
            }
        }

        Err(NokubeError::ServiceDeploymentFailed {
            service: "container-runtime-lookup".to_string(),
            node: "localhost".to_string(),
            reason: "No supported container runtime found (docker/podman/nerdctl). Please install one or adjust PATH.".to_string(),
        }.into())
    }

    /// 返回可用容器运行时的可执行路径
    pub fn get_runtime_path() -> Result<String> {
        Ok(Self::find_container_runtime()?.path)
    }

    /// 运行 Docker 容器
    pub fn run(config: &DockerRunConfig) -> Result<String> {
        info!("Running Docker container: {}", config.container_name);
        
        // 首先校验配置
        config.validate().map_err(|e| {
            error!("Docker configuration validation failed: {}", e);
            e
        })?;
        
        // 如果需要删除已有容器
        if config.remove_existing {
            let _ = Self::remove_container(&config.container_name);
        }
        
        // 查找容器运行时
        let runtime = Self::find_container_runtime()?;
        info!("Using container runtime '{}' at path: {}", runtime.name, runtime.path);
        
        // 构建 docker run 命令
        let mut command = Command::new(&runtime.path);
        command.arg("run");
        
        // 后台运行
        if config.detach {
            command.arg("-d");
        }
        
        // 容器名称
        command.args(&["--name", &config.container_name]);
        
        // 重启策略
        if let Some(ref policy) = config.restart_policy {
            command.args(&["--restart", policy]);
        }
        
        // 工作目录
        if let Some(ref workdir) = config.working_dir {
            command.args(&["-w", workdir]);
        }
        
        // 用户
        if let Some(ref user) = config.user {
            command.args(&["-u", user]);
        }
        
        // 网络
        if let Some(ref network) = config.network {
            command.args(&["--network", network]);
        }
        
        // 卷挂载
        for volume in &config.volumes {
            command.args(&["-v", &volume.to_docker_arg()]);
        }
        
        // 端口映射
        for port in &config.ports {
            command.args(&["-p", &port.to_docker_arg()]);
        }
        
        // 环境变量
        for (key, value) in &config.environment {
            command.args(&["-e", &format!("{}={}", key, value)]);
        }
        
        // 额外参数
        for arg in &config.extra_args {
            command.arg(arg);
        }
        
        // 镜像名称
        command.arg(&config.image);
        
        // 启动命令
        if let Some(ref cmd) = config.command {
            for arg in cmd {
                command.arg(arg);
            }
        }
        
        // 构建完整的命令字符串用于错误报告
        let full_command = Self::format_command(&command, config, &runtime.name);
        info!("Executing Docker command: {}", full_command);
        
        // 执行命令
        let output = command.output().map_err(|e| {
            NokubeError::ServiceDeploymentFailed {
                service: config.container_name.clone(),
                node: "localhost".to_string(),
                reason: format!("Failed to execute docker command '{}': {} (os error {})", 
                    full_command, e, e.raw_os_error().unwrap_or(-1))
            }
        })?;
        
        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            return Err(NokubeError::ServiceDeploymentFailed {
                service: config.container_name.clone(),
                node: "localhost".to_string(),
                reason: format!("Docker command '{}' failed with exit code {}: {}", 
                    full_command,
                    output.status.code().unwrap_or(-1),
                    error_msg.trim())
            }.into());
        }
        
        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        info!("Container {} started with ID: {}", config.container_name, container_id);
        
        Ok(container_id)
    }
    
    /// 停止容器
    pub fn stop(container_name: &str) -> Result<()> {
        info!("Stopping Docker container: {}", container_name);
        
        // 查找Docker的绝对路径
        let runtime = Self::find_container_runtime()?;

        let output = Command::new(&runtime.path)
            .args(&["stop", container_name])
            .output()
            .map_err(|e| {
                NokubeError::ServiceDeploymentFailed {
                    service: container_name.to_string(),
                    node: "localhost".to_string(),
                    reason: format!("Failed to stop container '{}': {}", container_name, e)
                }
            })?;;
            
        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            error!("Failed to stop container '{}': {}", container_name, error_msg.trim());
        }
        
        Ok(())
    }
    
    /// 删除容器
    pub fn remove_container(container_name: &str) -> Result<()> {
        info!("Removing Docker container: {}", container_name);
        
        // 查找Docker的绝对路径
        let runtime = Self::find_container_runtime()?;

        let output = Command::new(&runtime.path)
            .args(&["rm", "-f", container_name])
            .output()
            .map_err(|e| {
                NokubeError::ServiceDeploymentFailed {
                    service: container_name.to_string(),
                    node: "localhost".to_string(),
                    reason: format!("Failed to remove container '{}': {}", container_name, e)
                }
            })?;
            
        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            // 删除失败通常不是致命错误（容器可能不存在）
            info!("Note: Failed to remove container '{}': {}", container_name, error_msg.trim());
        }
        
        Ok(())
    }
    
    /// 检查容器是否在运行
    pub fn is_running(container_name: &str) -> bool {
        // 查找Docker的绝对路径，如果失败则返回false
        let docker_path = match Self::get_runtime_path() {
            Ok(path) => path,
            Err(_) => return false,
        };
        
        let output = Command::new(&docker_path)
            .args(&["ps", "-q", "-f", &format!("name={}", container_name)])
            .output();
            
        match output {
            Ok(output) => !output.stdout.is_empty(),
            Err(_) => false,
        }
    }
    
    /// 格式化命令用于日志输出
    fn format_command(command: &Command, config: &DockerRunConfig, runtime_name: &str) -> String {
        let mut parts = vec![runtime_name.to_string(), "run".to_string()];
        
        if config.detach {
            parts.push("-d".to_string());
        }
        
        parts.push("--name".to_string());
        parts.push(config.container_name.clone());
        
        if let Some(ref policy) = config.restart_policy {
            parts.push("--restart".to_string());
            parts.push(policy.clone());
        }
        
        if let Some(ref workdir) = config.working_dir {
            parts.push("-w".to_string());
            parts.push(workdir.clone());
        }
        
        if let Some(ref user) = config.user {
            parts.push("-u".to_string());
            parts.push(user.clone());
        }
        
        if let Some(ref network) = config.network {
            parts.push("--network".to_string());
            parts.push(network.clone());
        }
        
        for volume in &config.volumes {
            parts.push("-v".to_string());
            parts.push(volume.to_docker_arg());
        }
        
        for port in &config.ports {
            parts.push("-p".to_string());
            parts.push(port.to_docker_arg());
        }
        
        for (key, value) in &config.environment {
            parts.push("-e".to_string());
            parts.push(format!("{}={}", key, value));
        }
        
        for arg in &config.extra_args {
            parts.push(arg.clone());
        }
        
        parts.push(config.image.clone());
        
        if let Some(ref cmd) = config.command {
            parts.extend(cmd.iter().cloned());
        }
        
        // 对包含空格的参数加引号
        parts.iter()
            .map(|part| {
                if part.contains(' ') || part.contains('&') || part.contains('|') || part.contains(';') || part.contains('$') {
                    format!("\"{}\"", part.replace("\"", "\\\""))
                } else {
                    part.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    
    #[test]
    fn test_docker_config_validation() {
        // 测试空容器名称
        let config = DockerRunConfig::new("".to_string(), "nginx".to_string());
        assert!(config.validate().is_err());
        
        // 测试空镜像名称
        let config = DockerRunConfig::new("test".to_string(), "".to_string());
        assert!(config.validate().is_err());
        
        // 测试不存在的挂载路径
        let config = DockerRunConfig::new("test".to_string(), "nginx".to_string())
            .add_volume("/non/existent/path".to_string(), "/data".to_string(), false);
        assert!(config.validate().is_err());
        
        // 测试有效配置
        let temp_dir = std::env::temp_dir();
        let config = DockerRunConfig::new("test".to_string(), "nginx".to_string())
            .add_volume(temp_dir.to_string_lossy().to_string(), "/data".to_string(), false);
        assert!(config.validate().is_ok());
    }
    
    #[test]
    fn test_volume_mount_formatting() {
        let volume = VolumeMount::new("/host/path".to_string(), "/container/path".to_string(), false);
        assert_eq!(volume.to_docker_arg(), "/host/path:/container/path");
        
        let volume_ro = VolumeMount::new("/host/path".to_string(), "/container/path".to_string(), true);
        assert_eq!(volume_ro.to_docker_arg(), "/host/path:/container/path:ro");
    }
    
    #[test]
    fn test_port_mapping_formatting() {
        let port = PortMapping::new(8080, 80);
        assert_eq!(port.to_docker_arg(), "8080:80/tcp");
    }
}
