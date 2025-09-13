#!/usr/bin/env python3
"""
NoKube GitOps Webhook Server
接收GitOps事件并处理部署请求
"""

import os
import json
import logging
from datetime import datetime
from flask import Flask, request, jsonify
from typing import Dict, Any, List, Optional, Tuple
try:
    import yaml  # type: ignore
except Exception:  # pragma: no cover - optional dependency in example
    yaml = None
from typing import Dict, Any

# 配置日志
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)
logger = logging.getLogger('gitops-webhook')

# 创建Flask应用
app = Flask(__name__)

class WebhookHandler:
    """Webhook处理器"""
    
    def __init__(self, workspace_path: str):
        self.workspace_path = workspace_path
        self.events_file = os.path.join(workspace_path, 'webhook-events.jsonl')
        
    def handle_gitops_event(self, event_data: Dict[str, Any]) -> Dict[str, Any]:
        """处理GitOps事件"""
        logger.info(f"Handling GitOps event: {event_data.get('event', 'unknown')}")
        
        # 记录事件
        event_record = {
            'timestamp': datetime.utcnow().isoformat(),
            'event_data': event_data,
            'status': 'received'
        }
        
        # 保存事件到文件
        self.save_event(event_record)
        
        # 根据事件类型处理
        event_type = event_data.get('event')
        
        if event_type == 'gitops_deployment':
            return self.handle_deployment_event(event_data)
        elif event_type == 'config_update':
            return self.handle_config_update_event(event_data)
        else:
            logger.warning(f"Unknown event type: {event_type}")
            return {'status': 'ignored', 'message': f'Unknown event type: {event_type}'}
    
    def handle_deployment_event(self, event_data: Dict[str, Any]) -> Dict[str, Any]:
        """处理部署事件"""
        file_path = event_data.get('file_path', '')
        repository = event_data.get('repository', '')
        branch = event_data.get('branch', '')
        new_sha = event_data.get('new_sha', '')
        
        logger.info(f"Processing deployment event for {repository}:{branch}/{file_path}")
        
        # 创建部署任务
        deployment_task = {
            'id': f"deploy-{new_sha[:8]}",
            'timestamp': datetime.utcnow().isoformat(),
            'repository': repository,
            'branch': branch,
            'file_path': file_path,
            'sha': new_sha,
            'status': 'queued'
        }
        
        # 保存部署任务
        tasks_file = os.path.join(self.workspace_path, 'deployment-tasks.jsonl')
        with open(tasks_file, 'a', encoding='utf-8') as f:
            f.write(json.dumps(deployment_task) + '\n')
        
        logger.info(f"Deployment task created: {deployment_task['id']}")
        
        return {
            'status': 'queued',
            'task_id': deployment_task['id'],
            'message': f'Deployment task created for {file_path}'
        }
    
    def handle_config_update_event(self, event_data: Dict[str, Any]) -> Dict[str, Any]:
        """处理配置更新事件"""
        file_path = event_data.get('file_path', '')
        config_keys = event_data.get('config_keys', [])
        
        logger.info(f"Processing config update event for {file_path}")
        logger.info(f"Updated config keys: {config_keys}")
        
        # 这里可以触发配置重新加载
        # 例如：通知相关服务重新读取配置
        
        return {
            'status': 'processed',
            'message': f'Config update processed for {file_path}'
        }
    
    def save_event(self, event_record: Dict[str, Any]):
        """保存事件记录"""
        try:
            os.makedirs(os.path.dirname(self.events_file), exist_ok=True)
            with open(self.events_file, 'a', encoding='utf-8') as f:
                f.write(json.dumps(event_record) + '\n')
        except Exception as e:
            logger.error(f"Failed to save event: {e}")

# 创建全局webhook处理器
workspace_path = os.getenv('NOKUBE_WORKSPACE', '/workspace')
webhook_handler = WebhookHandler(workspace_path)


def _load_gitops_config(config_path: Optional[str] = None) -> Dict[str, Any]:
    """Load GitOps config from YAML if present; otherwise return empty skeleton.

    Looks for env `GITOPS_CONFIG` (defaults to /etc/gitops/gitops-config.yaml).
    """
    path = config_path or os.getenv('GITOPS_CONFIG', '/etc/gitops/gitops-config.yaml')
    data: Dict[str, Any] = {"github_configs": [], "services": []}
    try:
        if path and os.path.exists(path) and yaml is not None:
            with open(path, 'r', encoding='utf-8') as f:
                loaded = yaml.safe_load(f) or {}
            data['github_configs'] = loaded.get('github_configs', []) or []
            data['services'] = loaded.get('services', []) or []
    except Exception as e:  # non-fatal: best-effort
        logger.warning(f"Failed to load GitOps config from {path}: {e}")
    return data


def _parse_repo_fullname(repo_url: str) -> Optional[Tuple[str, str]]:
    """Parse https GitHub URL to (owner, repo)"""
    try:
        if repo_url.startswith('https://github.com/'):
            tail = repo_url.replace('https://github.com/', '').strip('/')
            parts = tail.split('/')
            if len(parts) >= 2:
                return parts[0], parts[1]
    except Exception:
        pass
    return None


def _load_recent_deployments(limit: int = 1000) -> List[Dict[str, Any]]:
    tasks: List[Dict[str, Any]] = []
    tasks_file = os.path.join(workspace_path, 'deployment-tasks.jsonl')
    if not os.path.exists(tasks_file):
        return tasks
    try:
        with open(tasks_file, 'r', encoding='utf-8') as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    tasks.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        # keep only the last N
        if len(tasks) > limit:
            tasks = tasks[-limit:]
    except Exception as e:
        logger.warning(f"Failed to read deployment tasks: {e}")
    return tasks

@app.route('/health', methods=['GET'])
def health_check():
    """健康检查端点"""
    return jsonify({
        'status': 'healthy',
        'timestamp': datetime.utcnow().isoformat(),
        'service': 'gitops-webhook-server'
    })

@app.route('/webhook/gitops', methods=['POST'])
def gitops_webhook():
    """GitOps webhook端点"""
    try:
        # 验证请求
        if not request.is_json:
            return jsonify({'error': 'Request must be JSON'}), 400
        
        event_data = request.get_json()
        if not event_data:
            return jsonify({'error': 'Empty request body'}), 400
        
        # 处理事件
        result = webhook_handler.handle_gitops_event(event_data)
        
        logger.info(f"Webhook processed successfully: {result}")
        
        return jsonify({
            'success': True,
            'result': result,
            'timestamp': datetime.utcnow().isoformat()
        })
        
    except Exception as e:
        logger.error(f"Webhook processing failed: {e}")
        return jsonify({
            'success': False,
            'error': str(e),
            'timestamp': datetime.utcnow().isoformat()
        }), 500

@app.route('/webhook/github', methods=['POST'])
def github_webhook():
    """GitHub webhook端点（可选）"""
    try:
        # 验证GitHub webhook签名（在生产环境中应该验证）
        event_type = request.headers.get('X-GitHub-Event', 'unknown')
        
        if event_type == 'push':
            # 处理push事件
            payload = request.get_json()
            
            if payload:
                repository = payload.get('repository', {}).get('full_name', '')
                branch = payload.get('ref', '').replace('refs/heads/', '')
                commits = payload.get('commits', [])
                
                logger.info(f"GitHub push event: {repository}:{branch} ({len(commits)} commits)")
                
                # 转换为内部GitOps事件格式
                for commit in commits:
                    for modified_file in commit.get('modified', []) + commit.get('added', []):
                        gitops_event = {
                            'event': 'gitops_deployment',
                            'repository': repository,
                            'branch': branch,
                            'file_path': modified_file,
                            'new_sha': commit['id'],
                            'old_sha': None,
                            'timestamp': datetime.utcnow().timestamp()
                        }
                        
                        # 处理事件
                        webhook_handler.handle_gitops_event(gitops_event)
        
        return jsonify({'success': True, 'event_type': event_type})
        
    except Exception as e:
        logger.error(f"GitHub webhook processing failed: {e}")
        return jsonify({'success': False, 'error': str(e)}), 500

@app.route('/status', methods=['GET'])
def get_status():
    """获取GitOps状态"""
    try:
        # 读取最近的事件
        events = []
        events_file = webhook_handler.events_file
        
        if os.path.exists(events_file):
            with open(events_file, 'r', encoding='utf-8') as f:
                lines = f.readlines()
                # 获取最近10个事件
                for line in lines[-10:]:
                    try:
                        events.append(json.loads(line.strip()))
                    except json.JSONDecodeError:
                        continue
        
        # 读取部署任务
        tasks = []
        tasks_file = os.path.join(workspace_path, 'deployment-tasks.jsonl')
        
        if os.path.exists(tasks_file):
            with open(tasks_file, 'r', encoding='utf-8') as f:
                lines = f.readlines()
                # 获取最近10个任务
                for line in lines[-10:]:
                    try:
                        tasks.append(json.loads(line.strip()))
                    except json.JSONDecodeError:
                        continue
        
        return jsonify({
            'status': 'running',
            'recent_events': events,
            'recent_tasks': tasks,
            'workspace_path': workspace_path,
            'timestamp': datetime.utcnow().isoformat()
        })
        
    except Exception as e:
        logger.error(f"Status check failed: {e}")
        return jsonify({'error': str(e)}), 500


@app.route('/v1/gitops/repos', methods=['GET'])
def list_gitops_repos():
    """List repos watched by GitOps and their latest deployment status.

    Aggregates from config (github_configs + services) and recent deployment tasks.
    """
    try:
        cfg = _load_gitops_config()
        services = cfg.get('services', [])
        gh_cfgs_raw = cfg.get('github_configs', {})

        # Normalize github_configs to a key -> {owner, name, branch}
        gh_by_key: Dict[str, Dict[str, Any]] = {}
        if isinstance(gh_cfgs_raw, dict):
            for k, gh in gh_cfgs_raw.items():
                gh_by_key[k] = {
                    'owner': gh.get('repo_owner'),
                    'name': gh.get('repo_name'),
                    'branch': gh.get('branch', 'main'),
                }
        elif isinstance(gh_cfgs_raw, list):
            for gh in gh_cfgs_raw:
                k = gh.get('key') or f"{gh.get('repo_owner')}/{gh.get('repo_name')}"
                gh_by_key[k] = {
                    'owner': gh.get('repo_owner'),
                    'name': gh.get('repo_name'),
                    'branch': gh.get('branch', 'main'),
                }

        # Build mapping: repo(full_name) -> branch (from github_configs) and services
        repo_branch: Dict[str, str] = {}
        for g in gh_by_key.values():
            owner, name = g.get('owner'), g.get('name')
            if owner and name:
                repo_branch[f"{owner}/{name}"] = g.get('branch', 'main')

        repo_services: Dict[str, List[str]] = {}
        repo_urls: Dict[str, str] = {}
        for svc in services:
            name = svc.get('name', '')
            full: Optional[str] = None
            if 'github' in svc and svc['github'] in gh_by_key:
                g = gh_by_key[svc['github']]
                if g.get('owner') and g.get('name'):
                    full = f"{g['owner']}/{g['name']}"
            elif 'repo' in svc:
                parsed = _parse_repo_fullname(svc.get('repo', ''))
                if parsed:
                    full = f"{parsed[0]}/{parsed[1]}"
            if not full:
                continue
            repo_services.setdefault(full, []).append(name)
            repo_urls[full] = f"https://github.com/{full}"

        # Load recent deployment tasks and compute latest per repo
        tasks = _load_recent_deployments()
        latest: Dict[str, Dict[str, Any]] = {}
        for t in tasks:
            full = t.get('repository') or ''
            if not full:
                continue
            ts = t.get('timestamp')
            if isinstance(ts, str):
                # try parse ISO back to epoch
                try:
                    ts_val = datetime.fromisoformat(ts.replace('Z', '+00:00')).timestamp()
                except Exception:
                    ts_val = None
            else:
                ts_val = ts
            if ts_val is None:
                continue
            prev = latest.get(full)
            if not prev or ts_val >= prev.get('ts', 0):
                latest[full] = {
                    'ts': ts_val,
                    'iso': datetime.utcfromtimestamp(ts_val).isoformat() + 'Z',
                    'sha': t.get('sha') or t.get('new_sha')
                }

        # Build response list
        result: List[Dict[str, Any]] = []
        all_repos = set(repo_services.keys()) | set(latest.keys()) | set(repo_branch.keys())
        for full in sorted(all_repos):
            url = repo_urls.get(full, f"https://github.com/{full}")
            branch = repo_branch.get(full, 'main')
            last = latest.get(full, {})
            entry = {
                'repo': full,
                'url': url,
                'branch': branch,
                'last_deploy_time': last.get('iso'),
                'last_commit_id': last.get('sha'),
                'services': sorted(repo_services.get(full, [])),
                'service_count': len(repo_services.get(full, [])),
            }
            result.append(entry)

        return jsonify({'repos': result, 'count': len(result), 'timestamp': datetime.utcnow().isoformat() + 'Z'})

    except Exception as e:
        logger.error(f"Failed to list gitops repos: {e}")
        return jsonify({'error': str(e)}), 500

def main():
    """主函数"""
    host = os.getenv('FLASK_HOST', '0.0.0.0')
    port = int(os.getenv('FLASK_PORT', '8080'))
    debug = os.getenv('FLASK_DEBUG', 'false').lower() == 'true'
    
    logger.info(f"Starting GitOps webhook server on {host}:{port}")
    logger.info(f"Workspace path: {workspace_path}")
    logger.info(f"Debug mode: {debug}")
    
    # 创建工作空间目录
    os.makedirs(workspace_path, exist_ok=True)
    
    # 启动Flask应用
    app.run(host=host, port=port, debug=debug)

if __name__ == '__main__':
    main()
