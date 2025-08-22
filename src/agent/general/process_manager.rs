use std::process::{Child, Command, Stdio};
use std::collections::HashMap;
use anyhow::Result;
use tracing::{info, error, warn};
use tokio::signal;

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
        info!("Process {} started with PID: {:?}", name, self.processes.get(&name).unwrap().id());
        Ok(())
    }

    pub fn spawn_docker_container(&mut self, name: String, image: String, args: Vec<String>) -> Result<()> {
        info!("Starting Docker container: {}", name);
        
        let mut command = Command::new("docker");
        command.arg("run").arg("-d").arg("--name").arg(&name);
        
        for arg in args {
            command.arg(arg);
        }
        command.arg(image);

        let output = command.output()?;
        
        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to start container {}: {}", name, error_msg);
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        self.docker_containers.push(name.clone());
        
        info!("Container {} started with ID: {}", name, container_id);
        Ok(())
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
        
        let stop_output = Command::new("docker")
            .args(&["stop", name])
            .output()?;
            
        if !stop_output.status.success() {
            warn!("Failed to stop container {}: {}", name, String::from_utf8_lossy(&stop_output.stderr));
        }

        let rm_output = Command::new("docker")
            .args(&["rm", name])
            .output()?;
            
        if !rm_output.status.success() {
            warn!("Failed to remove container {}: {}", name, String::from_utf8_lossy(&rm_output.stderr));
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
        let mut sigterm = signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");
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