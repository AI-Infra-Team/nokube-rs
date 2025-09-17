use super::docker_runner::{DockerRunConfig, DockerRunner};
use anyhow::Result;
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use tokio::signal;
use tracing::{error, info, warn};

#[derive(Default)]
pub struct ProcessManager {
    processes: HashMap<String, Child>,
    docker_containers: Vec<String>,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            processes: HashMap::new(),
            docker_containers: Vec::new(),
        }
    }

    pub fn spawn_process(&mut self, name: String, mut command: Command) -> Result<()> {
        info!("Starting process: {}", name);

        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        self.processes.insert(name.clone(), child);
        info!(
            "Process {} started with PID: {:?}",
            name,
            self.processes.get(&name).unwrap().id()
        );
        Ok(())
    }

    /// 直接使用DockerRunConfig创建容器方式 - 推荐使用
    pub fn spawn_docker_container_with_config(
        &mut self,
        config: DockerRunConfig,
    ) -> Result<String> {
        let container_name = config.container_name.clone();
        info!("Starting Docker container with config: {}", container_name);

        match DockerRunner::run(&config) {
            Ok(container_id) => {
                self.docker_containers.push(container_name.clone());
                info!(
                    "Container {} started with ID: {}",
                    container_name, container_id
                );
                Ok(container_id)
            }
            Err(e) => {
                error!("Failed to start container {}: {}", container_name, e);
                Err(e)
            }
        }
    }

    /// 兼容性方法 - 保持老接口，但使用新的DockerRunner
    pub fn spawn_docker_container(
        &mut self,
        name: String,
        image: String,
        docker_options: Vec<String>,
        command_args: Option<Vec<String>>,
    ) -> Result<()> {
        info!("Starting Docker container: {}", name);

        // 创建 DockerRunConfig
        let mut config = DockerRunConfig::new(name.clone(), image);

        // 解析 docker_options 来配置 DockerRunConfig
        let mut i = 0;
        while i < docker_options.len() {
            let arg = &docker_options[i];
            match arg.as_str() {
                "-v" | "--volume" => {
                    if i + 1 < docker_options.len() {
                        let volume_spec = &docker_options[i + 1];
                        if let Some((host_path, rest)) = volume_spec.split_once(':') {
                            let (container_path, read_only) =
                                if let Some((container_path, mode)) = rest.split_once(':') {
                                    (container_path, mode == "ro")
                                } else {
                                    (rest, false)
                                };
                            config = config.add_volume(
                                host_path.to_string(),
                                container_path.to_string(),
                                read_only,
                            );
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "-p" | "--publish" => {
                    if i + 1 < docker_options.len() {
                        let port_spec = &docker_options[i + 1];
                        if let Some((host_port, container_port)) = port_spec.split_once(':') {
                            if let (Ok(hp), Ok(cp)) =
                                (host_port.parse::<u16>(), container_port.parse::<u16>())
                            {
                                config = config.add_port(hp, cp);
                            }
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "-e" | "--env" => {
                    if i + 1 < docker_options.len() {
                        let env_spec = &docker_options[i + 1];
                        if let Some((key, value)) = env_spec.split_once('=') {
                            config = config.add_env(key.to_string(), value.to_string());
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "--restart" => {
                    if i + 1 < docker_options.len() {
                        config = config.restart_policy(docker_options[i + 1].clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "-w" | "--workdir" => {
                    if i + 1 < docker_options.len() {
                        config = config.working_dir(docker_options[i + 1].clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "-u" | "--user" => {
                    if i + 1 < docker_options.len() {
                        config = config.user(docker_options[i + 1].clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "--network" => {
                    if i + 1 < docker_options.len() {
                        config = config.network(docker_options[i + 1].clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                _ => {
                    // 其他参数作为额外参数处理
                    config.extra_args.push(arg.clone());
                    i += 1;
                }
            }
        }

        // 设置启动命令
        if let Some(cmd_args) = command_args {
            config = config.command(cmd_args);
        }

        // 使用 DockerRunner 运行容器
        match self.spawn_docker_container_with_config(config) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn stop_process(&mut self, name: &str) -> Result<()> {
        if let Some(mut child) = self.processes.remove(name) {
            info!("Stopping process: {}", name);
            child.kill()?;
            child.wait()?;
            info!("Process {} stopped", name);
        }
        Ok(())
    }

    pub fn stop_docker_container(&mut self, name: &str) -> Result<()> {
        info!("Stopping Docker container: {}", name);

        // 使用 DockerRunner 停止和删除容器
        if let Err(e) = DockerRunner::stop(name) {
            warn!("Failed to stop container {}: {}", name, e);
        }

        if let Err(e) = DockerRunner::remove_container(name) {
            warn!("Failed to remove container {}: {}", name, e);
        }

        self.docker_containers.retain(|c| c != name);
        info!("Container {} stopped and removed", name);
        Ok(())
    }

    pub async fn wait_for_shutdown_signal(&self) {
        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("Received Ctrl+C, shutting down...");
            }
            _ = self.wait_for_sigterm() => {
                info!("Received SIGTERM, shutting down...");
            }
        }
    }

    #[cfg(unix)]
    async fn wait_for_sigterm(&self) {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");
        sigterm.recv().await;
    }

    #[cfg(not(unix))]
    async fn wait_for_sigterm(&self) {
        // Windows doesn't have SIGTERM, just wait indefinitely
        std::future::pending::<()>().await;
    }

    pub fn cleanup_all(&mut self) -> Result<()> {
        info!("Cleaning up all processes and containers...");

        // Stop all processes
        let process_names: Vec<String> = self.processes.keys().cloned().collect();
        for name in process_names {
            if let Err(e) = self.stop_process(&name) {
                error!("Failed to stop process {}: {}", name, e);
            }
        }

        // Stop all Docker containers
        let container_names: Vec<String> = self.docker_containers.clone();
        for name in container_names {
            if let Err(e) = self.stop_docker_container(&name) {
                error!("Failed to stop container {}: {}", name, e);
            }
        }

        info!("Cleanup completed");
        Ok(())
    }

    pub fn is_process_running(&mut self, name: &str) -> bool {
        if let Some(child) = self.processes.get_mut(name) {
            match child.try_wait() {
                Ok(Some(_)) => {
                    // Process has exited
                    self.processes.remove(name);
                    false
                }
                Ok(None) => {
                    // Process is still running
                    true
                }
                Err(_) => {
                    // Error checking status, assume not running
                    self.processes.remove(name);
                    false
                }
            }
        } else {
            false
        }
    }
}
