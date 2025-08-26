use anyhow::Result;
use ssh2::{Session, Sftp};
use std::io::Read;
use std::net::TcpStream;
use std::path::Path;
use tracing::info;

#[derive(Clone)]
pub struct SSHManager {
    host: String,
    username: String,
    key_path: Option<String>,
    password: Option<String>,
}

impl SSHManager {
    pub fn new(host: String, username: String, key_path: Option<String>) -> Self {
        Self {
            host,
            username,
            key_path,
            password: None,
        }
    }

    pub fn new_with_password(
        host: String,
        username: String,
        key_path: Option<String>,
        password: Option<String>,
    ) -> Self {
        Self {
            host,
            username,
            key_path,
            password,
        }
    }

    pub async fn connect(&self) -> Result<Session> {
        let tcp = TcpStream::connect(&format!("{}:22", self.host))?;
        let mut sess = Session::new()?;
        sess.set_tcp_stream(tcp);
        sess.handshake()?;

        if let Some(key_path) = &self.key_path {
            sess.userauth_pubkey_file(&self.username, None, Path::new(key_path), None)?;
        } else if let Some(password) = &self.password {
            sess.userauth_password(&self.username, password)?;
        } else {
            anyhow::bail!("SSH 认证失败：未提供密钥或密码");
        }

        if !sess.authenticated() {
            anyhow::bail!("SSH authentication failed");
        }

        Ok(sess)
    }

    /// 执行命令，自动根据 require_root 加 sudo -E 前缀
    /// 执行命令，自动根据 require_root 加 sudo -E 前缀，show_progress=true 时实时输出到终端
    pub async fn execute_command(
        &self,
        command: &str,
        require_root: bool,
        show_progress: bool,
    ) -> Result<String> {
        let sess = self.connect().await?;
        let mut channel = sess.channel_session()?;
        let shell_cmd = if require_root && self.username != "root" {
            format!("sh -c 'sudo -E {}'", command.replace("'", "'\\''"))
        } else {
            format!("sh -c '{}'", command.replace("'", "'\\''"))
        };
        let cmd = shell_cmd;
        info!("Executing command on {}: {}", self.host, cmd);
        channel.exec(&cmd)?;
        let mut output = String::new();
        let mut error_output = String::new();
        if show_progress {
            use ssh2::Stream;
            let mut buf = [0u8; 4096];
            let mut err_buf = [0u8; 4096];
            let mut stdout_stream = channel.stream(0);
            let mut stderr_stream = channel.stream(1);
            loop {
                let n = stdout_stream.read(&mut buf).unwrap_or(0);
                if n > 0 {
                    let s = String::from_utf8_lossy(&buf[..n]);
                    print!("{}", s);
                    output.push_str(&s);
                }
                let m = stderr_stream.read(&mut err_buf).unwrap_or(0);
                if m > 0 {
                    let es = String::from_utf8_lossy(&err_buf[..m]);
                    eprint!("{}", es);
                    error_output.push_str(&es);
                }
                if n == 0 && m == 0 {
                    if channel.eof() {
                        break;
                    }
                }
            }
        } else {
            channel.read_to_string(&mut output)?;
            // stderr只能在show_progress时捕获
        }
        channel.wait_close()?;
        let exit_code = channel.exit_status()?;
        if exit_code != 0 {
            anyhow::bail!(
                "Command failed with exit code: {}\nstdout:\n{}\nstderr:\n{}",
                exit_code,
                output,
                error_output
            );
        }
        Ok(output)
    }

    pub async fn upload_file(&self, local_path: &str, remote_path: &str) -> Result<()> {
        let sess = self.connect().await
            .map_err(|e| anyhow::anyhow!("Failed to establish SSH connection to {}: {}", self.host, e))?;
        let sftp = sess.sftp()
            .map_err(|e| anyhow::anyhow!("Failed to establish SFTP session with {}: {:?}", self.host, e))?;

        info!(
            "Uploading file {} to {}:{}",
            local_path, self.host, remote_path
        );

        // 自动创建远程父目录并修正权限
        if let Some(parent) = Path::new(remote_path).parent() {
            let parent_str = parent.to_str().unwrap_or("");
            if !parent_str.is_empty() {
                // 创建目录
                self.execute_command(&format!("mkdir -p {}", parent_str), true, false)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to create parent directory {}: {}", parent_str, e))?;
                // 修正权限
                self.execute_command(
                    &format!(
                        "chown -R {}:{} {}",
                        self.username, self.username, parent_str
                    ),
                    true,
                    false,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Failed to set ownership for directory {}: {}", parent_str, e))?;
            }
        }

        let local_content = std::fs::read(local_path)
            .map_err(|e| anyhow::anyhow!("Failed to read local file {}: {}", local_path, e))?;
        
        info!("Read {} bytes from local file {}", local_content.len(), local_path);
        
        // 删除远程文件（如果存在）
        let _ = self.execute_command(&format!("rm -f {}", remote_path), true, false).await;
        
        let mut remote_file = sftp.create(Path::new(remote_path))
            .map_err(|e| anyhow::anyhow!("Failed to create remote file {} on {}: SFTP error: {:?}", remote_path, self.host, e))?;
        
        std::io::Write::write_all(&mut remote_file, &local_content)
            .map_err(|e| anyhow::anyhow!("Failed to write {} bytes to remote file {} on {}: {}", local_content.len(), remote_path, self.host, e))?;

        info!("Successfully uploaded {} bytes to {}:{}", local_content.len(), self.host, remote_path);
        Ok(())
    }

    pub async fn upload_directory(
        &self,
        local_dir: &str,
        remote_dir: &str,
        require_root: bool,
    ) -> Result<()> {
        let sess = self.connect().await
            .map_err(|e| anyhow::anyhow!("Failed to establish SSH connection to {}: {}", self.host, e))?;
        let sftp = sess.sftp()
            .map_err(|e| anyhow::anyhow!("Failed to establish SFTP session with {}: {:?}", self.host, e))?;

        info!(
            "Uploading directory {} to {}:{}",
            local_dir, self.host, remote_dir
        );

        // 检查本地目录是否存在
        if !std::path::Path::new(local_dir).exists() {
            anyhow::bail!("Local directory does not exist: {}", local_dir);
        }

        // 使用远程命令创建目录
        self.execute_command(&format!("mkdir -p {}", remote_dir), require_root, false)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create remote directory {}: {}", remote_dir, e))?;
        
        self.execute_command(
            &format!(
                "chown -R {}:{} {}",
                self.username, self.username, remote_dir
            ),
            require_root,
            false,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to set ownership for remote directory {}: {}", remote_dir, e))?;
        
        // 上传所有文件
        self.upload_directory_recursive(&sess, &sftp, local_dir, remote_dir, require_root)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to upload directory {} to {}:{}: {}", local_dir, self.host, remote_dir, e))?;

        // 上传完成后统一 chown 到 ssh 用户，确保所有文件权限
        self.execute_command(
            &format!(
                "chown -R {}:{} {}",
                self.username, self.username, remote_dir
            ),
            require_root,
            false,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to set final ownership for remote directory {}: {}", remote_dir, e))?;
        
        // 再统一 chmod 700，保证所有文件可执行且安全
        self.execute_command(&format!("chmod -R 700 {}", remote_dir), require_root, false)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to set permissions for remote directory {}: {}", remote_dir, e))?;

        info!("Directory upload completed: {} -> {}:{}", local_dir, self.host, remote_dir);
        Ok(())
    }

    async fn upload_directory_recursive(
        &self,
        sess: &Session,
        sftp: &Sftp,
        local_dir: &str,
        remote_dir: &str,
        require_root: bool,
    ) -> Result<()> {
        use std::boxed::Box;

        let entries = std::fs::read_dir(local_dir)
            .map_err(|e| anyhow::anyhow!("Failed to read local directory {}: {}", local_dir, e))?;
        
        for entry in entries {
            let entry = entry
                .map_err(|e| anyhow::anyhow!("Failed to read directory entry in {}: {}", local_dir, e))?;
            let local_path = entry.path();
            let file_name = local_path.file_name().unwrap().to_str().unwrap();
            let remote_path = format!("{}/{}", remote_dir, file_name);

            if local_path.is_dir() {
                // 递归前先用远程命令创建目录
                self.execute_command(&format!("mkdir -p {}", remote_path), require_root, false)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to create remote directory {}: {}", remote_path, e))?;
                Box::pin(self.upload_directory_recursive(
                    sess,
                    sftp,
                    local_path.to_str().unwrap(),
                    &remote_path,
                    require_root,
                ))
                .await
                .map_err(|e| anyhow::anyhow!("Failed to upload subdirectory {} to {}: {}", local_path.display(), remote_path, e))?;
            } else {
                let local_path_str = local_path.to_str().unwrap();
                let local_content = std::fs::read(&local_path)
                    .map_err(|e| anyhow::anyhow!("Failed to read local file {}: {}", local_path_str, e))?;
                
                info!("Uploading file {} ({} bytes) to {}", local_path_str, local_content.len(), remote_path);
                
                // 删除远程文件（如果存在）
                let _ = self.execute_command(&format!("rm -f {}", remote_path), require_root, false).await;
                
                let mut remote_file = sftp.create(std::path::Path::new(&remote_path))
                    .map_err(|e| anyhow::anyhow!("Failed to create remote file {} on {}: SFTP error: {:?}", remote_path, self.host, e))?;
                
                std::io::Write::write_all(&mut remote_file, &local_content)
                    .map_err(|e| anyhow::anyhow!("Failed to write {} bytes to remote file {} on {}: {}", local_content.len(), remote_path, self.host, e))?;
                
                info!("Successfully uploaded file {} to {}", local_path_str, remote_path);
            }
        }
        Ok(())
    }

    // ...existing code...

    pub async fn download_file(&self, remote_path: &str, local_path: &str) -> Result<()> {
        let sess = self.connect().await?;
        let sftp = sess.sftp()?;

        info!(
            "Downloading file {}:{} to {}",
            self.host, remote_path, local_path
        );

        let mut remote_file = sftp.open(Path::new(remote_path))?;
        let mut contents = Vec::new();
        remote_file.read_to_end(&mut contents)?;

        std::fs::write(local_path, contents)?;

        info!("File download completed");
        Ok(())
    }
}
