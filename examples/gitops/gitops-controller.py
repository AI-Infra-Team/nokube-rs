#!/usr/bin/env python3
"""
NoKube GitOps Controller
轮询GitHub仓库变化，触发GitOps部署
"""

import os
import json
import time
import hashlib
import logging
import requests
from typing import Dict, Optional, List
from dataclasses import dataclass
from pathlib import Path

# 配置日志
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)
logger = logging.getLogger('gitops-controller')

@dataclass
class GitHubConfig:
    """单个GitHub仓库配置"""
    repo_owner: str
    repo_name: str
    branch: str
    token: str
    key: Optional[str] = None  # 可选：当从映射加载时的键名

@dataclass
class ServiceConfig:
    """单个服务配置"""
    name: str
    k8s_yaml_dir: str  # k8s YAML文件目录
    repo: Optional[str] = None  # 兼容旧版：GitHub仓库URL
    github: Optional[str] = None  # 新版：引用 github_configs 中的 key

@dataclass
class GitOpsConfig:
    """GitOps总配置"""
    github_configs: List[GitHubConfig]
    services: List[ServiceConfig]
    poll_interval: int = 60  # 轮询间隔（秒）
    webhook_url: Optional[str] = None  # 保留字段；当前示例仅轮询
    applyer_nokube: Optional[Dict] = None  # 供本地 nokube applyer 使用
    
    @classmethod
    def from_secret(cls, secret_path: str) -> 'GitOpsConfig':
        """从挂载的secret中读取配置"""
        config_file = os.path.join(secret_path, 'gitops-config.json')
        
        if not os.path.exists(config_file):
            raise FileNotFoundError(f"GitOps config not found: {config_file}")
        
        with open(config_file, 'r', encoding='utf-8') as f:
            config_data = json.load(f)
        
        # 解析GitHub配置（支持 list 或 dict 两种格式）
        github_configs: List[GitHubConfig] = []
        gh_raw = config_data.get('github_configs', [])
        if isinstance(gh_raw, dict):
            for key, gh in gh_raw.items():
                github_configs.append(GitHubConfig(
                    repo_owner=gh['repo_owner'],
                    repo_name=gh['repo_name'],
                    branch=gh.get('branch', 'main'),
                    token=gh.get('token', ''),
                    key=key
                ))
        else:
            for gh in gh_raw:
                github_configs.append(GitHubConfig(
                    repo_owner=gh['repo_owner'],
                    repo_name=gh['repo_name'],
                    branch=gh.get('branch', 'main'),
                    token=gh.get('token', ''),
                    key=gh.get('key') if isinstance(gh, dict) else None
                ))
        
        # 解析服务配置
        services: List[ServiceConfig] = []
        for svc in config_data.get('services', []):
            services.append(ServiceConfig(
                name=svc['name'],
                k8s_yaml_dir=svc['k8s_yaml_dir'],
                repo=svc.get('repo'),
                github=svc.get('github')
            ))
        
        return cls(
            github_configs=github_configs,
            services=services,
            poll_interval=config_data.get('poll_interval', 60),
            webhook_url=config_data.get('webhook_url'),
            applyer_nokube=config_data.get('applyer_nokube')
        )

    @classmethod
    def from_yaml(cls, path: str) -> 'GitOpsConfig':
        """从挂载的 ConfigMap YAML 中读取配置"""
        import yaml as yaml_parser
        if not os.path.exists(path):
            raise FileNotFoundError(f"GitOps YAML config not found: {path}")
        with open(path, 'r', encoding='utf-8') as f:
            data = yaml_parser.safe_load(f) or {}
        gh_cfgs: List[GitHubConfig] = []
        gh_raw = data.get('github_configs', [])
        if isinstance(gh_raw, dict):
            for key, gh in gh_raw.items():
                gh_cfgs.append(GitHubConfig(
                    repo_owner=gh['repo_owner'],
                    repo_name=gh['repo_name'],
                    branch=gh.get('branch', 'main'),
                    token=gh.get('token', ''),
                    key=key
                ))
        else:
            for gh in gh_raw:
                gh_cfgs.append(GitHubConfig(
                    repo_owner=gh['repo_owner'],
                    repo_name=gh['repo_name'],
                    branch=gh.get('branch', 'main'),
                    token=gh.get('token', ''),
                    key=gh.get('key') if isinstance(gh, dict) else None
                ))

        services: List[ServiceConfig] = []
        for svc in data.get('services', []):
            services.append(ServiceConfig(
                name=svc['name'],
                k8s_yaml_dir=svc['k8s_yaml_dir'],
                repo=svc.get('repo'),
                github=svc.get('github')
            ))
        return cls(
            github_configs=gh_cfgs,
            services=services,
            poll_interval=int(data.get('poll_interval', 60)),
            webhook_url=data.get('webhook_url'),
            applyer_nokube=data.get('applyer_nokube')
        )

class GitHubClient:
    """GitHub API客户端"""
    
    def __init__(self, token: str):
        self.token = token
        self.session = requests.Session()
        self.session.headers.update({
            'Authorization': f'token {token}',
            'Accept': 'application/vnd.github.v3+json',
            'User-Agent': 'NoKube-GitOps/1.0'
        })
    
    def get_file_content(self, owner: str, repo: str, path: str, branch: str) -> Optional[Dict]:
        """获取文件内容和元数据"""
        url = f'https://api.github.com/repos/{owner}/{repo}/contents/{path}'
        params = {'ref': branch}
        
        try:
            response = self.session.get(url, params=params)
            response.raise_for_status()
            return response.json()
        except requests.RequestException as e:
            logger.error(f"Failed to get file {path}: {e}")
            return None
    
    def get_commit_info(self, owner: str, repo: str, branch: str) -> Optional[Dict]:
        """获取分支最新commit信息"""
        url = f'https://api.github.com/repos/{owner}/{repo}/branches/{branch}'
        
        try:
            response = self.session.get(url)
            response.raise_for_status()
            return response.json()['commit']
        except requests.RequestException as e:
            logger.error(f"Failed to get commit info for {branch}: {e}")
            return None

class GitOpsController:
    """GitOps控制器 - 完全无状态设计"""
    
    def __init__(self, config: GitOpsConfig):
        self.config = config
        # Optional: setup nokube applyer if configured
        self.applyer = None
        if self.config.applyer_nokube:
            try:
                from .applyer_nokube import NokubeApplyer  # if used as a module
            except ImportError:
                # fall back to same-dir import when not as a package
                from applyer_nokube import NokubeApplyer
            self.applyer = NokubeApplyer(self.config.applyer_nokube)
            ok, msg = self.applyer.install()
            if ok:
                logger.info(f"Nokube applyer installed: {msg}")
            else:
                logger.warning(f"Nokube applyer install failed: {msg}")
        # 为每个GitHub配置创建客户端 + 快速索引
        self.github_clients: Dict[str, GitHubClient] = {}
        self.github_by_key: Dict[str, GitHubConfig] = {}
        for gh_config in config.github_configs:
            client_key = f"{gh_config.repo_owner}/{gh_config.repo_name}"
            self.github_clients.setdefault(client_key, GitHubClient(gh_config.token))
            if gh_config.key:
                self.github_by_key[gh_config.key] = gh_config
        
        # 状态存储在内存中，不依赖本地文件
        self.current_state = {}
    
    def check_file_changes(self) -> List[Dict]:
        """检查所有服务的k8s YAML文件是否有变化（无状态检查）"""
        changes = []
        new_state = {}
        
        for service in self.config.services:
            # 解析GitHub仓库信息（优先使用 github key；回退到 repo URL）
            repo_owner = repo_name = None
            github_config = None
            if service.github:
                github_config = self.github_by_key.get(service.github)
                if not github_config:
                    logger.warning(f"Unknown github key in service '{service.name}': {service.github}")
                    continue
                repo_owner, repo_name = github_config.repo_owner, github_config.repo_name
            elif service.repo:
                repo_url = service.repo
                if repo_url.startswith('https://github.com/'):
                    repo_path = repo_url.replace('https://github.com/', '').rstrip('/')
                    parts = repo_path.split('/')
                    if len(parts) >= 2:
                        repo_owner, repo_name = parts[0], parts[1]
                if not repo_owner:
                    logger.warning(f"Unsupported repo URL format: {repo_url}")
                    continue
                # 匹配对应的 GitHub 配置（用于 token/branch）
                for gh in self.config.github_configs:
                    if gh.repo_owner == repo_owner and gh.repo_name == repo_name:
                        github_config = gh
                        break
            
            if not github_config:
                logger.warning(f"No GitHub config found for {repo_owner}/{repo_name}")
                continue
            
            client_key = f"{repo_owner}/{repo_name}"
            github_client = self.github_clients[client_key]
            
            # 检查k8s_yaml_dir目录下的所有YAML文件
            yaml_files = self.get_yaml_files_in_directory(
                github_client, repo_owner, repo_name, github_config.branch, service.k8s_yaml_dir
            )
            
            for yaml_file in yaml_files:
                file_key = f"{service.name}/{yaml_file}"
                file_info = github_client.get_file_content(
                    repo_owner, repo_name, yaml_file, github_config.branch
                )
                
                if not file_info:
                    logger.warning(f"Could not fetch file: {yaml_file}")
                    continue
                
                current_sha = file_info['sha']
                new_state[file_key] = current_sha
                
                # 检查是否有变化（与内存状态对比）
                last_sha = self.current_state.get(file_key)
                if last_sha != current_sha:
                    logger.info(f"File changed: {file_key} ({last_sha} -> {current_sha})")
                    changes.append({
                        'service_name': service.name,
                        'file_path': yaml_file,
                        'file_key': file_key,
                        'old_sha': last_sha,
                        'new_sha': current_sha,
                        'content': file_info,
                        'repo_owner': repo_owner,
                        'repo_name': repo_name
                    })
        
        # 更新内存状态
        self.current_state = new_state
        
        return changes
    
    def get_yaml_files_in_directory(self, github_client: GitHubClient, 
                                   owner: str, repo: str, branch: str, directory: str) -> List[str]:
        """获取指定目录下的所有YAML文件"""
        url = f'https://api.github.com/repos/{owner}/{repo}/contents/{directory}'
        params = {'ref': branch}
        
        try:
            response = github_client.session.get(url, params=params)
            response.raise_for_status()
            contents = response.json()
            
            yaml_files = []
            for item in contents:
                if item['type'] == 'file' and (item['name'].endswith('.yaml') or item['name'].endswith('.yml')):
                    yaml_files.append(item['path'])
                elif item['type'] == 'dir':
                    # 递归获取子目录中的YAML文件
                    sub_yaml_files = self.get_yaml_files_in_directory(
                        github_client, owner, repo, branch, item['path']
                    )
                    yaml_files.extend(sub_yaml_files)
            
            return yaml_files
            
        except requests.RequestException as e:
            logger.error(f"Failed to get directory contents {directory}: {e}")
            return []
    
    def trigger_deployment(self, changes: List[Dict]):
        """触发部署"""
        logger.info(f"Triggering deployment for {len(changes)} file changes")
        
        for change in changes:
            service_name = change['service_name']
            file_path = change['file_path']
            new_sha = change['new_sha']
            
            # 解码文件内容
            import base64
            content = base64.b64decode(change['content']['content']).decode('utf-8')
            
            logger.info(f"Processing service: {service_name}, file: {file_path}")
            logger.info(f"New SHA: {new_sha}")
            
            # 处理k8s YAML文件
            self.handle_k8s_yaml_file(service_name, file_path, content, new_sha)
            
            # 简化：当前示例仅轮询触发，不发送 webhook 通知
    
    def handle_k8s_yaml_file(self, service_name: str, file_path: str, content: str, sha: str):
        """处理k8s YAML文件变化 - 无状态处理"""
        logger.info(f"Handling k8s YAML for service {service_name}: {file_path}")
        
        # 解析YAML内容，确保服务无状态化
        try:
            import yaml as yaml_parser
            yaml_docs = list(yaml_parser.safe_load_all(content))
            
            for doc in yaml_docs:
                if doc and 'kind' in doc:
                    self.ensure_stateless_service(doc, service_name)
            
            # 重新序列化处理后的YAML
            processed_content = yaml_parser.dump_all(yaml_docs, default_flow_style=False)
            
        except Exception as e:
            logger.warning(f"Failed to parse YAML, using original content: {e}")
            processed_content = content
        
        # 直接应用到NoKube，不保存本地文件
        self.apply_nokube_manifest(service_name, processed_content, file_path, sha)
    
    def ensure_stateless_service(self, yaml_doc: Dict, service_name: str):
        """确保k8s服务无状态化 - 配置通过ConfigMap/Secret，资源通过HTTP下载"""
        kind = yaml_doc.get('kind', '')
        
        if kind in ['Deployment', 'DaemonSet', 'StatefulSet']:
            spec = yaml_doc.setdefault('spec', {})
            template = spec.setdefault('template', {})
            pod_spec = template.setdefault('spec', {})
            
            # 确保容器配置
            containers = pod_spec.setdefault('containers', [])
            for container in containers:
                self.configure_stateless_container(container, service_name)
            
            # 添加标准的volume挂载
            self.add_standard_volumes(pod_spec, service_name)
            
        elif kind == 'ConfigMap':
            # ConfigMap用于存储配置文件
            self.validate_configmap(yaml_doc, service_name)
            
        elif kind == 'Secret':
            # Secret用于存储私密配置
            self.validate_secret(yaml_doc, service_name)
    
    def configure_stateless_container(self, container: Dict, service_name: str):
        """配置无状态容器"""
        # 确保环境变量从ConfigMap/Secret引用
        env = container.setdefault('env', [])
        
        # 添加标准环境变量
        standard_env = [
            {
                'name': 'SERVICE_NAME',
                'value': service_name
            },
            {
                'name': 'CONFIG_PATH',
                'value': '/etc/config'
            },
            {
                'name': 'SECRET_PATH', 
                'value': '/etc/secret'
            },
            {
                'name': 'DOWNLOAD_BASE_URL',
                'valueFrom': {
                    'configMapKeyRef': {
                        'name': f'{service_name}-config',
                        'key': 'download_base_url'
                    }
                }
            }
        ]
        
        # 添加启动脚本，确保通过HTTP下载资源
        container['command'] = ['/bin/bash']
        container['args'] = ['-c', f"""
# 下载启动脚本
wget -O /tmp/init.sh $DOWNLOAD_BASE_URL/scripts/{service_name}/init.sh
chmod +x /tmp/init.sh

# 下载应用资源
mkdir -p /app
wget -O /app/app.tar.gz $DOWNLOAD_BASE_URL/apps/{service_name}/app.tar.gz
cd /app && tar -xzf app.tar.gz

# 执行启动脚本
/tmp/init.sh
"""]
        
        # 标准volume挂载
        volume_mounts = container.setdefault('volumeMounts', [])
        standard_mounts = [
            {
                'name': 'config-volume',
                'mountPath': '/etc/config',
                'readOnly': True
            },
            {
                'name': 'secret-volume',
                'mountPath': '/etc/secret', 
                'readOnly': True
            },
            {
                'name': 'tmp-volume',
                'mountPath': '/tmp',
                'readOnly': False
            }
        ]
        
        for mount in standard_mounts:
            if not any(vm.get('name') == mount['name'] for vm in volume_mounts):
                volume_mounts.append(mount)
    
    def add_standard_volumes(self, pod_spec: Dict, service_name: str):
        """添加标准的volume配置"""
        volumes = pod_spec.setdefault('volumes', [])
        
        standard_volumes = [
            {
                'name': 'config-volume',
                'configMap': {
                    'name': f'{service_name}-config'
                }
            },
            {
                'name': 'secret-volume', 
                'secret': {
                    'secretName': f'{service_name}-secret'
                }
            },
            {
                'name': 'tmp-volume',
                'emptyDir': {}
            }
        ]
        
        for volume in standard_volumes:
            if not any(v.get('name') == volume['name'] for v in volumes):
                volumes.append(volume)
    
    def validate_configmap(self, yaml_doc: Dict, service_name: str):
        """验证ConfigMap配置"""
        metadata = yaml_doc.setdefault('metadata', {})
        name = metadata.get('name', '')
        
        if not name.startswith(service_name):
            logger.warning(f"ConfigMap name should start with service name: {name}")
        
        data = yaml_doc.setdefault('data', {})
        
        # 确保包含下载基础URL
        if 'download_base_url' not in data:
            data['download_base_url'] = 'https://releases.example.com'
            logger.info(f"Added default download_base_url to ConfigMap {name}")
    
    def validate_secret(self, yaml_doc: Dict, service_name: str):
        """验证Secret配置"""
        metadata = yaml_doc.setdefault('metadata', {})
        name = metadata.get('name', '')
        
        if not name.startswith(service_name):
            logger.warning(f"Secret name should start with service name: {name}")
        
        # Secret应该只包含私密配置
        secret_type = yaml_doc.get('type', 'Opaque')
        if secret_type not in ['Opaque', 'kubernetes.io/tls']:
            logger.warning(f"Unexpected secret type: {secret_type}")
    
    def handle_yaml_file(self, file_path: str, content: str, sha: str):
        """处理通用 YAML 文件变化（非 K8s 资源）

        当前实现仅将文件持久化，留待上层流程或后续处理；不直接触发集群应用。
        """
        logger.info(f"Handling YAML file: {file_path}")

        # 保存文件到本地
        local_file = f'/pod-workspace/deploy/{os.path.basename(file_path)}'
        os.makedirs(os.path.dirname(local_file), exist_ok=True)

        with open(local_file, 'w', encoding='utf-8') as f:
            f.write(content)

        logger.info(f"YAML file saved to: {local_file}")
    
    def handle_json_file(self, file_path: str, content: str, sha: str):
        """处理JSON配置文件变化"""
        logger.info(f"Handling JSON file: {file_path}")
        
        try:
            config_data = json.loads(content)
            logger.info(f"JSON config loaded: {len(config_data)} keys")
            
            # 可以在这里更新配置到etcd或触发服务重启
            self.update_service_config(file_path, config_data, sha)
            
        except json.JSONDecodeError as e:
            logger.error(f"Invalid JSON in {file_path}: {e}")
    
    def handle_generic_file(self, file_path: str, content: str, sha: str):
        """处理通用文件变化"""
        logger.info(f"Handling generic file: {file_path}")
        
        # 保存到本地并记录变化
        local_file = f'/pod-workspace/files/{os.path.basename(file_path)}'
        os.makedirs(os.path.dirname(local_file), exist_ok=True)
        
        with open(local_file, 'w', encoding='utf-8') as f:
            f.write(content)
        
        logger.info(f"File saved to: {local_file}")
    
    def apply_nokube_manifest(self, service_name: str, yaml_content: str, original_path: str, sha: str):
        """直接应用 NoKube 清单到集群（无回退路径）"""
        logger.info(f"Applying NoKube manifest for service {service_name}: {original_path} (SHA: {sha})")

        try:
            if self.applyer is None:
                raise RuntimeError("No Applyer configured. Provide applyer_nokube in config to enable apply().")

            ok, out = self.applyer.apply(yaml_content)
            if not ok:
                raise RuntimeError(f"nokube apply failed: {out.strip()[:400]}")

            logger.info(f"nokube apply succeeded: {out.strip()[:200]}")
            if self.config.webhook_url:
                self.send_apply_notification(service_name, original_path, sha, "success")

        except Exception as e:
            logger.error(f"Failed to apply manifest for service {service_name}: {e}")
            if self.config.webhook_url:
                self.send_apply_notification(service_name, original_path, sha, "failed", str(e))
    
    # 已移除: HTTP API 回退 (call_nokube_api)
    
    def kubectl_apply(self, yaml_content: str, service_name: str):
        """使用kubectl apply应用清单（备选方案）"""
        import subprocess
        import tempfile
        
        # 创建临时文件
        with tempfile.NamedTemporaryFile(mode='w', suffix='.yaml', delete=False) as temp_file:
            temp_file.write(yaml_content)
            temp_file_path = temp_file.name
        
        try:
            # 执行kubectl apply
            result = subprocess.run([
                'kubectl', 'apply', '-f', temp_file_path,
                '--namespace', 'default'
            ], capture_output=True, text=True, timeout=30)
            
            if result.returncode != 0:
                raise Exception(f"kubectl apply failed: {result.stderr}")
            
            logger.info(f"kubectl apply output: {result.stdout}")
            
        finally:
            # 清理临时文件
            os.unlink(temp_file_path)
    
    def send_apply_notification(self, service_name: str, file_path: str, sha: str, status: str, error: str = None):
        """发送应用结果通知"""
        notification = {
            'event': 'manifest_applied',
            'service_name': service_name,
            'file_path': file_path,
            'sha': sha,
            'status': status,
            'timestamp': time.time()
        }
        
        if error:
            notification['error'] = error
        
        try:
            response = requests.post(
                self.config.webhook_url,
                json=notification,
                timeout=10
            )
            logger.info(f"Apply notification sent: {response.status_code}")
        except Exception as e:
            logger.warning(f"Failed to send apply notification: {e}")
    
    def update_service_config(self, file_path: str, config_data: Dict, sha: str):
        """更新服务配置"""
        logger.info(f"Updating service config from: {file_path} (SHA: {sha})")
        
        # 这里应该更新etcd中的配置或触发服务重新加载
        config_record = {
            'timestamp': time.time(),
            'file_path': file_path,
            'config_keys': list(config_data.keys()),
            'sha': sha,
            'status': 'updated'
        }
        
        # 保存配置更新记录
        config_updates_file = '/pod-workspace/config-updates.jsonl'
        with open(config_updates_file, 'a', encoding='utf-8') as f:
            f.write(json.dumps(config_record) + '\n')
        
        logger.info(f"Config update recorded: {config_record}")
    
    def send_webhook_notification(self, change: Dict):
        """发送webhook通知"""
        if not self.config.webhook_url:
            return
        
        payload = {
            'event': 'gitops_deployment',
            'repository': f"{self.config.repo_owner}/{self.config.repo_name}",
            'branch': self.config.branch,
            'file_path': change['file_path'],
            'old_sha': change['old_sha'],
            'new_sha': change['new_sha'],
            'timestamp': time.time()
        }
        
        try:
            response = requests.post(
                self.config.webhook_url,
                json=payload,
                timeout=10
            )
            response.raise_for_status()
            logger.info(f"Webhook notification sent successfully")
        except requests.RequestException as e:
            logger.error(f"Failed to send webhook notification: {e}")
    
    def run(self):
        """运行GitOps控制器"""
        logger.info("Starting GitOps controller (polling only)")
        logger.info(f"Configured repos: {len(self.config.github_configs)}; services: {len(self.config.services)}; poll: {self.config.poll_interval}s")
        
        while True:
            try:
                logger.debug("Checking for file changes...")
                changes = self.check_file_changes()
                
                if changes:
                    logger.info(f"Found {len(changes)} file changes")
                    self.trigger_deployment(changes)
                else:
                    logger.debug("No changes detected")
                
            except Exception as e:
                logger.error(f"Error during polling: {e}")
            
            # 等待下一次轮询
            time.sleep(self.config.poll_interval)

def main():
    """主函数"""
    try:
        # 优先从 ConfigMap (YAML) 加载；如不存在则回退到 Secret(JSON)
        cfg_path = os.getenv('GITOPS_CONFIG', '/etc/gitops/gitops-config.yaml')
        if os.path.exists(cfg_path):
            logger.info(f"Loading GitOps config from YAML: {cfg_path}")
            config = GitOpsConfig.from_yaml(cfg_path)
        else:
            secret_path = '/pod-workspace/secret'
            logger.info(f"Fallback: loading GitOps config from Secret JSON: {secret_path}")
            config = GitOpsConfig.from_secret(secret_path)
        
        # 创建并运行GitOps控制器
        controller = GitOpsController(config)
        controller.run()
        
    except KeyboardInterrupt:
        logger.info("GitOps controller stopped by user")
    except Exception as e:
        logger.error(f"GitOps controller failed: {e}")
        raise

if __name__ == '__main__':
    main()
