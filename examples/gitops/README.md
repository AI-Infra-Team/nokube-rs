# NoKube GitOps Example

这个示例展示了如何使用NoKube部署一个完整的GitOps系统，该系统使用Python 3.10镜像轮询GitHub仓库变化并触发自动部署。

## 🏗️ 架构概览

```
┌─────────────────┐    ┌──────────────────┐    ┌─────────────────┐
│   GitHub Repo   │    │  GitOps Controller  │    │  NoKube Cluster │
│                 │    │                    │    │                │
│  ├─deployments/ │    │  ┌──────────────┐  │    │  ┌───────────┐  │
│  ├─configs/     │◄───┤  │ Python 3.10  │  ├───►│  │ ConfigMap │  │
│  └─secrets/     │    │  │ Polling      │  │    │  │ Secret    │  │
└─────────────────┘    │  └──────────────┘  │    │  │ Pods      │  │
         ▲              │                    │    └─────────────────┘
         │              ▼                            ▲
         │    ┌──────────────────┐                  │
         │    │ HTTP File Server │                  │
         └────┤                  │──────────────────┘
              │ /scripts/        │    wget下载
              │ /apps/           │    应用资源
              │ /configs/        │
              └──────────────────┘
```

## 📦 无状态服务设计原则

### 🔧 配置管理
- **ConfigMap**: 存储所有配置文件（非机密）
- **Secret**: 存储私密信息（密码、证书、token）
- **HTTP下载**: 应用二进制和资源包通过HTTP获取

### 🚀 服务启动流程
1. 容器启动时从ConfigMap读取`download_base_url`
2. wget下载启动脚本：`$DOWNLOAD_BASE_URL/scripts/{service_name}/init.sh`
3. wget下载应用包：`$DOWNLOAD_BASE_URL/apps/{service_name}/app.tar.gz`
4. 解压应用包到`/app`目录
5. 执行启动脚本，应用从`/etc/config`和`/etc/secret`读取配置

### 📁 HTTP服务器目录结构
```
https://releases.example.com/
├── scripts/
│   ├── etcd/
│   │   └── init.sh          # 启动脚本
│   └── monitoring/
│       └── init.sh
├── apps/
│   ├── etcd/
│   │   └── app.tar.gz       # 应用资源包
│   └── monitoring/
│       └── app.tar.gz
└── configs/
    └── templates/           # 配置模板（可选）
```

## 📦 组件说明

### 1. GitOps Controller (DaemonSet)
- **镜像**: `python:3.10-slim`
- **功能**: 轮询GitHub仓库，检测文件变化
- **部署**: 在管理节点上运行（使用NodeAffinity）
- **挂载**: Secret配置、工作空间、脚本

### 2. Webhook Server (Deployment)
- **镜像**: `python:3.10-slim`
- **功能**: 接收GitOps事件，处理部署请求
- **部署**: 在工作节点上运行（2个副本）
- **端口**: 8080 (Flask应用)

### 3. 配置管理
- **Secret**: GitHub token和GitOps配置
- **ConfigMap**: Python依赖和安装脚本
- **Workspace**: 持久化状态和部署文件

## 🚀 快速开始

### 1. 准备GitHub仓库

创建一个GitHub仓库，结构如下：
```
your-gitops-repo/
├── deployments/
│   ├── web-app.yaml
│   └── api-service.yaml
├── configs/
│   ├── database-config.json
│   └── app-settings.json
├── services/
│   └── ingress.yaml
└── monitoring/
    └── alerts.yaml
```

### 2. 生成GitHub Token

1. 访问 GitHub → Settings → Developer settings → Personal access tokens
2. 生成新token，需要以下权限：
   - `repo` (访问私有仓库)
   - `contents:read` (读取文件内容)

### 3. 使用Apply脚本生成清单

首先创建配置文件 `gitops-config.yaml`:

```yaml
# GitHub仓库配置
github_configs:
- repo_owner: "AI-Infra-Team"
  repo_name: "gitops-config"
  branch: "main"
  token: "ghp_your_token_here"

# 服务配置
services:
  - name: etcd
    repo: https://github.com/AI-Infra-Team/nokube
    k8s_yaml_dir: examples/etcd
  - name: monitoring
    repo: https://github.com/AI-Infra-Team/nokube
    k8s_yaml_dir: examples/monitoring

# 轮询配置  
poll_interval: 60
webhook_url: "https://api.yourorg.com/webhooks/gitops"
```

然后直接部署GitOps系统：

```bash
# 方式1: 直接部署到NoKube集群（推荐）
python apply-gitops.py \
  --cluster-name "my-k8s-cluster" \
  --config-file gitops-config.yaml

# 方式2: Dry run模式 - 只生成YAML不部署
python apply-gitops.py \
  --cluster-name "my-k8s-cluster" \
  --config-file gitops-config.yaml \
  --dry-run

# 方式3: Dry run + 手动部署
python apply-gitops.py \
  --cluster-name "my-k8s-cluster" \
  --config-file gitops-config.yaml \
  --dry-run \
  | nokube apply -f -
```

**默认行为**: 脚本会自动调用`nokube apply`直接部署GitOps系统到集群

### 4. 验证部署

```bash
# 检查NoKube集群中的服务状态
LD_LIBRARY_PATH=target ./target/nokube get pods --cluster home-cluster

# 查看GitOps controller服务详情
docker logs nokube-agent-container -f

# 检查已应用的配置文件
find ~/.nokube -name "*.json" | head -10
ls -la ~/.nokube/clusters/

# 如果启用了监控，查看Grafana服务状态
LD_LIBRARY_PATH=target ./target/nokube get services --cluster home-cluster
# 浏览器访问: http://localhost:3000 (admin/admin)

# 检查GreptimeDB指标存储
curl http://localhost:4000/v1/sql -d "SELECT * FROM nokube_actor_status LIMIT 5"

# 验证GitOps配置是否正确存储
cat ~/.nokube/clusters/my-k8s-cluster/configmaps/gitops-scripts-my-k8s-cluster/gitops-config.json
```

## 📖 配置详解

### Secret配置 (`gitops-config.json`)
```json
{
  "repo_owner": "your-org",
  "repo_name": "gitops-config", 
  "branch": "main",
  "target_files": [
    "deployments/web-app.yaml",
    "configs/database-config.json",
    "services/api-gateway.yaml",
    "monitoring/alerts.yaml"
  ],
  "poll_interval": 60,
  "webhook_url": "https://api.yourorg.com/webhooks/gitops"
}
```

### NodeAffinity配置
```yaml
nodeAffinity:
  required:
  - matchExpressions:
    - key: "node-role.nokube.io/management"
      operator: "In"
      values: ["true"]
```

## 🔄 工作流程

### 1. 文件变化检测
```
GitHub仓库文件更新 → GitOps Controller轮询检测 → 比较文件SHA → 触发部署流程
```

### 2. 部署触发流程
```python
# 伪代码
def check_file_changes():
    for file_path in target_files:
        current_sha = github.get_file_sha(file_path)
        if current_sha != last_known_sha[file_path]:
            trigger_deployment(file_path, current_sha)
```

### 3. 部署处理
```python
def trigger_deployment(file_path, new_sha):
    # 下载文件内容
    content = github.get_file_content(file_path)
    
    # 根据文件类型处理
    if file_path.endswith('.yaml'):
        apply_nokube_manifest(content)
    elif file_path.endswith('.json'):
        update_service_config(content)
```

## 📊 监控和日志

### 1. 健康检查端点
```bash
# NoKube Agent健康检查（通过容器）
docker exec nokube-agent-container ps aux | grep nokube

# 检查Grafana服务状态
LD_LIBRARY_PATH=target ./target/nokube describe service grafana --cluster home-cluster

# GreptimeDB健康检查
curl http://localhost:4000/v1/sql -d "SELECT 1"
```

### 2. 日志文件
```bash
# NoKube Agent日志
docker logs nokube-agent-container --tail 50

# 查看etcd存储的配置
find ~/.nokube -type f -name "*.json" | xargs ls -la

# 系统服务日志（如果有监控）
journalctl -u nokube-service --since "1 hour ago"
```

### 3. 指标收集
GitOps系统会自动收集以下指标到GreptimeDB：
```bash
# 查看系统指标
curl "http://localhost:4000/v1/sql" -d "SELECT * FROM nokube_cpu_usage ORDER BY timestamp DESC LIMIT 10"

# 查看容器指标  
curl "http://localhost:4000/v1/sql" -d "SELECT * FROM nokube_container_cpu_usage ORDER BY timestamp DESC LIMIT 10"

# 查看Actor状态指标
curl "http://localhost:4000/v1/sql" -d "SELECT * FROM nokube_actor_status WHERE actor_type='deployment' LIMIT 5"

# 在Grafana中查看仪表板
echo "访问 http://localhost:3000 查看实时监控面板"
echo "用户名: admin, 密码: admin"
```

## 🔧 自定义配置

### 1. 添加新的文件类型处理
```python
def handle_custom_file(self, file_path: str, content: str, sha: str):
    """处理自定义文件类型"""
    if file_path.endswith('.custom'):
        # 自定义处理逻辑
        logger.info(f"Processing custom file: {file_path}")
```

### 2. 集成外部系统
```python
def send_slack_notification(self, change: Dict):
    """发送Slack通知"""
    webhook_url = "https://hooks.slack.com/services/..."
    payload = {
        "text": f"🚀 GitOps deployment triggered: {change['file_path']}"
    }
    requests.post(webhook_url, json=payload)
```

### 3. 添加认证机制
```python
def verify_webhook_signature(self, request):
    """验证webhook签名"""
    signature = request.headers.get('X-Hub-Signature-256')
    secret = os.getenv('WEBHOOK_SECRET')
    # 验证逻辑...
```

## 🎯 最佳实践

### 1. 安全配置
- ✅ 使用Secret存储GitHub token
- ✅ 配置适当的RBAC权限
- ✅ 启用webhook签名验证
- ✅ 定期轮换访问token

### 2. 监控告警
- ✅ 监控轮询失败次数
- ✅ 跟踪部署成功率
- ✅ 设置资源使用告警
- ✅ 配置健康检查

### 3. 性能优化
- ✅ 合理设置轮询间隔
- ✅ 使用条件请求减少API调用
- ✅ 实现增量同步
- ✅ 配置资源限制

## 🐛 故障排除

### 常见问题

1. **NoKube Agent容器未启动**
   ```bash
   # 检查集群状态
   LD_LIBRARY_PATH=target ./target/nokube get pods --cluster home-cluster
   
   # 查看启动失败日志
   docker logs nokube-agent-container
   
   # 检查二进制文件权限
   ls -la target/nokube
   ```

2. **GitOps配置未应用**
   ```bash
   # 检查配置文件是否存在
   ls -la ~/.nokube/clusters/*/configmaps/
   
   # 查看配置内容
   find ~/.nokube -name "*gitops*" -exec cat {} \;
   
   # 检查Agent处理日志
   docker logs nokube-agent-container | grep -i gitops
   ```

3. **Grafana无法访问**
   ```bash
   # 检查Grafana服务状态
   LD_LIBRARY_PATH=target ./target/nokube describe service grafana --cluster home-cluster
   
   # 查看Grafana启动日志
   docker logs nokube-grafana
   
   # 检查端口占用
   netstat -tulpn | grep 3000
   
   # 手动测试连接
   curl -I http://localhost:3000
   ```

4. **GreptimeDB数据库问题**
   ```bash
   # 检查GreptimeDB服务状态
   LD_LIBRARY_PATH=target ./target/nokube describe service greptimedb --cluster home-cluster
   
   # 测试数据库连接
   curl http://localhost:4000/v1/sql -d "SHOW TABLES"
   
   # 查看数据库日志
   docker logs nokube-greptimedb --tail 20
   ```

5. **部署应用失败**
   ```bash
   # 检查apply命令执行日志
   docker logs nokube-agent-container | grep -A 5 -B 5 "apply"
   
   # 查看存储的配置是否正确
   find ~/.nokube -name "*.json" -exec jq . {} \; | head -50
   
   # 检查etcd连接
   docker logs nokube-agent-container | grep -i etcd
   ```

### 调试模式

启用详细日志模式：
```bash
# 重新启动容器并启用调试日志
docker stop nokube-agent-container
docker rm nokube-agent-container

# 在新的部署中添加调试环境变量
export RUST_LOG=debug
python apply-gitops.py --cluster-name "debug-cluster" --config-file gitops-config.yaml
```

## 📚 参考资源

- [NoKube Documentation](https://docs.nokube.io)
- [GitHub API Documentation](https://docs.github.com/en/rest)
- [Python Requests Library](https://requests.readthedocs.io)
- [Flask Web Framework](https://flask.palletsprojects.com)