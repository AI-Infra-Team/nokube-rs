# Simple GitOps for Kubernetes

本示例提供一个与 Kubernetes 兼容的简易 GitOps 方案，仅使用 Python 3.10 镜像按固定间隔轮询 Git 仓库变化，并以标准 Kubernetes YAML 清单驱动集群变更。

## 🏗️ 架构概览

提示: 本目录包含一份 Obsidian Canvas 架构图，便于整体把握：
- Canvas: `examples/gitops/gitops_design.canvas`（建议用 Obsidian 打开查看）

```
┌─────────────────┐    ┌──────────────────┐    ┌──────────────────────┐
│   Git Repository│    │  GitOps Controller  │    │  Kubernetes Cluster │
│                 │    │                    │    │                │
│  ├─deployments/ │    │  ┌──────────────┐  │    │  ┌───────────┐  │
│  ├─configs/     │◄───┤  │ Python 3.10  │  ├───►│  │ ConfigMap │  │
│  └─secrets/     │    │  │ Polling      │  │    │  │ Secret    │  │
└─────────────────┘    │  └──────────────┘  │    │  │ Pods      │  │
         ▲              │                    │    └──────────────────────┘
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

### 🌐 HTTP服务器（含状态API）
- 同一 HTTP Server 提供静态资源与状态API。
- GET `/v1/gitops/repos`: 返回当前 GitOps 关注的仓库及最近部署信息
  - 字段说明：
    - `repo`: `owner/repo`
    - `url`: 仓库地址
    - `branch`: 关注分支（来自 `github_configs`）
    - `last_deploy_time`: 最近部署时间（ISO8601，若无则为空）
    - `last_commit_id`: 最近部署的 commit id（若无则为空）
    - `services`: 使用该仓库的服务名列表
    - `service_count`: 服务数量

示例：
```bash
curl -s http://localhost:8080/v1/gitops/repos | jq
```
返回：
```json
{
  "count": 2,
  "repos": [
    {
      "repo": "AI-Infra-Team/nokube",
      "url": "https://github.com/AI-Infra-Team/nokube",
      "branch": "main",
      "last_deploy_time": "2025-09-01T11:45:02Z",
      "last_commit_id": "a1b2c3d4e5f6...",
      "services": ["etcd", "monitoring"],
      "service_count": 2
    }
  ],
  "timestamp": "2025-09-01T11:46:00Z"
}
```

## 📦 组件说明（与 Kubernetes 兼容）

### 1. GitOps Controller (DaemonSet)
- **镜像**: `python:3.10-slim`
- **功能**: 轮询GitHub仓库，检测文件变化
- **部署**: 在管理节点上运行（使用NodeAffinity）
- **挂载**: Secret配置、工作空间、脚本

### 2. 配置管理（通过 ConfigMap 挂载）
- **ConfigMap**: GitOps 控制器配置（gitops-config.yaml）、Python 依赖与启动脚本
- **Secret**: 仅在需要时存放敏感信息（本示例默认不需要）
- **Workspace**: 持久化状态和部署文件（可选）

## 🚀 快速开始（示例）

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

首先创建配置文件 `gitops-config.yaml`（将通过 ConfigMap 挂载到容器）：

```yaml
# GitHub仓库配置（键值映射，服务通过 key 引用）
github_configs:
  nokube:
    repo_owner: "AI-Infra-Team"
    repo_name: "nokube"
    branch: "main"
    token: "ghp_your_token_here"

# 服务配置（通过 github: <key> 关联到上面的仓库）
services:
  - name: etcd
    github: nokube
    k8s_yaml_dir: examples/etcd
  - name: monitoring
    github: nokube
    k8s_yaml_dir: examples/monitoring

# 轮询配置
poll_interval: 60
```

然后直接部署GitOps系统：

```bash
# 方式1: Dry run 模式（输出 YAML）
python apply-gitops.py \
  --cluster-name "my-k8s-cluster" \
  --config-file gitops-config.yaml \
  --dry-run

# 方式2: 管道部署到 Kubernetes（推荐）
python apply-gitops.py \
  --cluster-name "my-k8s-cluster" \
  --config-file gitops-config.yaml \
  --dry-run \
  | kubectl apply -f -

# 方式3: 部署轮询 Controller（YAML 位于本目录）
kubectl apply -f examples/gitops/gitops-daemonset.yaml
```

备注: 脚本可直接输出标准 Kubernetes YAML，建议使用 `kubectl apply -f -` 完成部署。

### 4. 验证部署

```bash
kubectl get pods -n default
kubectl logs ds/gitops-controller -n default
kubectl get deploy,svc,cm,secret -n default
```

## 📖 配置详解

### 配置 Schema 概览
- `github_configs`（map）：键为仓库标识；值包含 `repo_owner`、`repo_name`、`branch`（默认 `main`）、`token`（私库必填）。
- `services`（list）：每个服务包含 `name`、`github`（引用上面的键）、`k8s_yaml_dir`。兼容旧写法 `repo: https://github.com/<owner>/<repo>`。
- `poll_interval`（秒）：轮询间隔。
- `applyer_nokube`：`{ cluster_name, nokube_download_url }`，用于直接通过 CLI 应用；下载地址应指向容器平台匹配的 `nokube` 可执行文件。

### Controller 配置（通过 ConfigMap 挂载）
```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: gitops-config
  namespace: default
data:
  gitops-config.yaml: |
    github_configs:
      your-repo:
        repo_owner: your-org
        repo_name: gitops-config
        branch: main
        token: ghp_your_token

    services:
      - name: example
        github: your-repo
        k8s_yaml_dir: k8s

    poll_interval: 60
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

### 2. 部署触发流程（轮询）
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
        kubectl_apply(content)
    elif file_path.endswith('.json'):
        update_service_config(content)
```

## 📊 监控和日志

## 🧩 NoKube Applyer（可选）

本示例内置一个最小“Applyer”机制，用于把渲染后的 Kubernetes YAML 应用到目标集群。包括抽象接口与 NoKube 的具体实现。

### 1) 抽象 Applyer 接口（概念）
- 约定方法：
  - `install() -> (ok: bool, msg: str)`：安装/准备依赖（可选）；若失败，仅记录并通知（无回退）。
  - `apply(yaml_content: str) -> (ok: bool, out: str)`：将 YAML 应用到目标集群。
- 使用位置：`examples/gitops/gitops-controller.py` 的 `GitOpsController.apply_nokube_manifest()`。
- 调用流程（节选）：
```python
applied = False
if self.applyer is not None:
    ok, out = self.applyer.apply(yaml_content)
    if ok:
        applied = True
if not applied:
    self.call_nokube_api(service_name, yaml_content, sha)
```
- 可扩展性：实现任意自定义 Applyer（如 `KubectlApplyer`、`ArgoApplyer`），只需满足上述两个方法。

#### 接口调用时机
- `install()`：在 Controller 初始化时调用一次（检测到 `applyer_nokube` 配置后即执行）。成功则启用 Applyer；失败也继续运行，但后续 apply 可能失败（无回退）。
- `apply(yaml)`：在每次检测到变更并生成/处理 YAML 后调用（逐个文件串行处理）。若失败，记录并通知（无回退）。
- 通知：成功/失败都会记录日志；若配置了 `webhook_url`，会发送成功/失败通知。

### GitOps 核心执行流程（ASCII）
```
┌────────────────────┐
│  Controller Start  │
└─────────┬──────────┘
          │
          v
   Load config (YAML)
          │
          v
   Build Applyer (if configured)
          │
          v
   applyer.install()  ────┐
          │               │fail
          v               ▼
       OK? ───────────► No applyer (log only)
          │yes
          v
  ┌───────────────────────────────┐
  │   Loop: poll Git repos        │
  │   ─ detect changes?           │
  │   ─ fetch file contents       │
  │   ─ ensure stateless (mutate) │
  │   ─ render YAML               │
  └───────┬───────────────────────┘
          │
          v
   try applyer.apply(yaml)
          │
       OK?│yes
          v
  record success → notify
          │
          └── no
              │
              v
        call_nokube_api()
              │
           OK?│yes/no → record + notify
```

### 2) Nokube Applyer 实现（内置）
- 文件：`examples/gitops/applyer_nokube.py`
- 作用：通过本地 `nokube` CLI 将 YAML 直接应用到指定的 NoKube 集群。
- API 行为：
- `install()`：若二进制不存在，则从 `nokube_download_url` 下载到固定路径 `/pod-workspace/bin/nokube` 并授予执行权限；随后健康检查二进制。
  - `apply(yaml)`：执行 `nokube apply --cluster <cluster_name>`，经 stdin 传入 YAML。
- 启用方式（`gitops-config.yaml` 顶层增加 `applyer_nokube`）：
```yaml
applyer_nokube:
  cluster_name: "your-cluster"                       # 目标集群名
  nokube_download_url: "https://releases.example.com/nokube/linux-amd64/nokube"

```
- 可选：NoKube 全局配置样例 `examples/gitops/applyer_nokube_spec.config.yaml`（`etcd_endpoints`）。如需 `nokube` CLI 访问 etcd，请确保容器里存在 `~/.nokube/config.yaml` 或 `/etc/.nokube/config.yaml`（可通过挂载或镜像内置）。
- 最小示例：
```python
from applyer_nokube import NokubeApplyer
applyer = NokubeApplyer({
    "nokube_config_file": "~/.nokube/config.yaml",
    "cluster_name": "home-cluster",
})
ok, msg = applyer.install()
ok, out = applyer.apply(yaml_text)
```
- 无回退：若 `apply()` 失败，仅记录错误并可通知（如配置了 `webhook_url`）。

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
## 🗺️ 设计要点（与 Canvas 对齐）

- 仅依赖 Kubernetes：输出标准 YAML，应用由 `kubectl` 或任意 Kubernetes 客户端完成
- 仅轮询：固定间隔检测仓库变化，简化运行面
- 无状态/幂等：多次应用结果一致，便于回滚与重放
- 配置分离：ConfigMap 管理控制器配置与脚本，Secret 仅用于敏感信息
