use anyhow::Result;
use std::process::Command;
use tracing::info;
use crate::config::cluster_config::ProxyConfig;

pub struct RemoteExecutor {
    proxy_config: Option<ProxyConfig>,
}

impl RemoteExecutor {
    pub fn new() -> Self {
        Self {
            proxy_config: None,
        }
    }

    pub fn with_proxy(proxy_config: ProxyConfig) -> Self {
        Self {
            proxy_config: Some(proxy_config),
        }
    }

    pub fn set_proxy(&mut self, proxy_config: Option<ProxyConfig>) {
        self.proxy_config = proxy_config;
    }

    fn apply_proxy_env(&self, command: &mut Command) {
        if let Some(proxy) = &self.proxy_config {
            if let Some(http_proxy) = &proxy.http_proxy {
                command.env("http_proxy", http_proxy);
                command.env("HTTP_PROXY", http_proxy);
            }
            if let Some(https_proxy) = &proxy.https_proxy {
                command.env("https_proxy", https_proxy);
                command.env("HTTPS_PROXY", https_proxy);
            }
            if let Some(no_proxy) = &proxy.no_proxy {
                command.env("no_proxy", no_proxy);
                command.env("NO_PROXY", no_proxy);
            }
        }
    }

    pub async fn execute_command(&self, command: &str) -> Result<CommandResult> {
        info!("Executing command: {}", command);
        
        let mut cmd = if cfg!(target_os = "windows") {
            let mut c = Command::new("cmd");
            c.args(&["/C", command]);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        };

        self.apply_proxy_env(&mut cmd);
        let output = cmd.output()?;

        let result = CommandResult {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            success: output.status.success(),
        };

        info!("Command completed with exit code: {}", result.exit_code);
        Ok(result)
    }

    pub async fn execute_script(&self, script_content: &str) -> Result<CommandResult> {
        info!("Executing script");
        
        // Write script to temporary file and execute it
        let temp_file = "/tmp/nokube_script.sh";
        std::fs::write(temp_file, script_content)?;
        
        let mut cmd = Command::new("sh");
        cmd.arg(temp_file);
        self.apply_proxy_env(&mut cmd);
        
        let output = cmd.output()?;

        // Clean up temporary file
        std::fs::remove_file(temp_file).ok();

        let result = CommandResult {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            success: output.status.success(),
        };

        info!("Script completed with exit code: {}", result.exit_code);
        Ok(result)
    }
}

#[derive(Debug, Clone)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub success: bool,
}