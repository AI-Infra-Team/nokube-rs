#!/usr/bin/env python3
"""
NoKube GitOps Apply Tool
直接部署GitOps系统到NoKube集群
"""

import os
import sys
import json
import yaml
import base64
import argparse
import time
from typing import List, Dict

def encode_secret_data(data: str) -> str:
    """Base64编码secret数据"""
    return base64.b64encode(data.encode('utf-8')).decode('ascii')

def create_gitops_config(
    github_configs: List[Dict],
    services: List[Dict],
    poll_interval: int = 60,
) -> dict:
    """创建GitOps配置"""
    
    config = {
        "github_configs": github_configs,
        "services": services,
        "poll_interval": poll_interval
    }
    return config

def generate_nokube_manifest(
    cluster_name: str,
    github_configs: List[Dict],
    services: List[Dict],
    poll_interval: int = 60,
) -> dict:
    """生成完全无状态的NoKube GitOps清单"""
    
    # 创建GitOps配置
    gitops_config = create_gitops_config(
        github_configs, services, poll_interval
    )
    
    # 准备Secret数据（只存储敏感信息）
    secret_data = {}
    
    # 准备ConfigMap数据（存储配置和代码）
    config_data = {
        "gitops-config.json": json.dumps(gitops_config, indent=2)
    }
    
    # GitOps控制器Python代码
    gitops_controller_code = '''#!/usr/bin/env python3
import json
import os
import sys
import time
import requests
from typing import Dict, List, Optional

# 配置日志
import logging
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)
logger = logging.getLogger('gitops-controller')

class GitOpsController:
    """无状态GitOps控制器"""
    
    def __init__(self, config_path: str):
        # 读取配置
        with open(config_path, 'r') as f:
            self.config = json.load(f)
        
        # 内存状态
        self.current_state = {}
        
        logger.info(f"GitOps Controller initialized for {len(self.config['services'])} services")
    
    def run(self):
        """主循环"""
        poll_interval = self.config.get('poll_interval', 60)
        
        while True:
            try:
                logger.info("Checking for changes...")
                changes = self.check_file_changes()
                
                if changes:
                    logger.info(f"Found {len(changes)} changes")
                    self.trigger_deployment(changes)
                else:
                    logger.info("No changes detected")
                    
            except Exception as e:
                logger.error(f"Error in main loop: {e}")
            
            time.sleep(poll_interval)
    
    def check_file_changes(self) -> List[Dict]:
        """检查文件变化（简化版本）"""
        # 这里是简化的检查逻辑
        logger.info("Checking GitHub repositories for changes...")
        return []
    
    def trigger_deployment(self, changes: List[Dict]):
        """触发部署"""
        for change in changes:
            logger.info(f"Processing change: {change}")
            # 调用NoKube API
            self.call_nokube_api(change)
    
    def call_nokube_api(self, change: Dict):
        """调用NoKube API"""
        api_url = os.getenv('NOKUBE_API_URL', 'http://nokube-api:8080')
        logger.info(f"Calling NoKube API: {api_url}")

if __name__ == "__main__":
    controller = GitOpsController("/etc/config/gitops-config.json")
    controller.run()
'''
    
    
    manifest = {
        "apiVersion": "nokube.io/v1",
        "kind": "GitOpsCluster",
        "metadata": {
            "name": f"gitops-{cluster_name}",
            "namespace": "default"
        },
        "spec": {
            "clusterName": cluster_name,
            
            # ConfigMap存储配置和Python代码文件
            "configMap": {
                "name": f"gitops-scripts-{cluster_name}",
                "data": {
                    "gitops-config.json": config_data["gitops-config.json"],
                    "gitops-controller.py": gitops_controller_code,
                    "requirements.txt": "requests==2.31.0\nPyYAML==6.0.1"
                }
            },
            
            # GitOps Controller Deployment
            "deployment": {
                "name": f"gitops-controller-{cluster_name}",
                "replicas": 1,  # 单实例就够了
                "nodeAffinity": {
                    "preferred": [{
                        "weight": 100,
                        "preference": {
                            "matchExpressions": [{
                                "key": "node-role.nokube.io/management",
                                "operator": "In",
                                "values": ["true"]
                            }]
                        }
                    }]
                },
                "containerSpec": {
                    "name": "gitops-controller",
                    "image": "python:3.10-slim",
                    "command": ["/bin/bash"],
                    "args": ["-c", "pip install -r /etc/config/requirements.txt && python /etc/config/gitops-controller.py"],
                    "env": {
                        "PYTHONUNBUFFERED": "1",
                        "TZ": "UTC",
                        "NOKUBE_API_URL": "http://nokube-api:8080"
                    },
                    "volumeMounts": [
                        {
                            "name": "config-volume",
                            "mountPath": "/etc/config",
                            "readOnly": True
                        }
                    ]
                }
            }
        }
    }

    return manifest

def main():
    """主函数"""
    parser = argparse.ArgumentParser(description="NoKube GitOps Apply Tool - Direct GitOps Deployment")
    
    parser.add_argument("--cluster-name", required=True, help="Target cluster name")
    parser.add_argument("--config-file", required=True, help="GitOps configuration file (YAML format)")
    parser.add_argument("--dry-run", action="store_true", help="Only generate YAML, don't deploy")
    
    args = parser.parse_args()
    
    # 读取配置文件
    if not os.path.exists(args.config_file):
        print(f"Error: Configuration file not found: {args.config_file}")
        return
    
    with open(args.config_file, 'r', encoding='utf-8') as f:
        config_data = yaml.safe_load(f)
    
    # 解析配置
    github_configs = config_data.get('github_configs', [])
    services = config_data.get('services', [])
    poll_interval = config_data.get('poll_interval', 60)
    
    if not github_configs:
        print("Error: No github_configs found in configuration file")
        return
    
    if not services:
        print("Error: No services found in configuration file")
        return
    
    # 生成清单
    manifest = generate_nokube_manifest(
        cluster_name=args.cluster_name,
        github_configs=github_configs,
        services=services,
        poll_interval=poll_interval,
    )
    
    if args.dry_run:
        # Dry run模式：只输出YAML
        output = yaml.dump(manifest, default_flow_style=False, indent=2)
        print(output)
        print(f"# 🎯 Dry run mode - YAML generated for cluster: {args.cluster_name}", file=sys.stderr)
        print(f"# 💡 To deploy: python {' '.join(sys.argv).replace('--dry-run', '')}", file=sys.stderr)
    else:
        # 直接部署到NoKube
        success = deploy_to_nokube(manifest, args.cluster_name)

        if success:
            print(f"✅ GitOps successfully deployed to cluster: {args.cluster_name}")
            print(f"🏷️  Target cluster: {args.cluster_name}")
            print(f"📦 GitHub configs: {len(github_configs)} repositories")
            print(f"🔧 Services: {len(services)} services")
            print("")
            print("🚀 GitOps Controller is starting up...")
            print("📊 Running brief post-apply status checks...")

            # Sleep a bit to allow resources to come up
            wait_sec = int(os.getenv('NOKUBE_POST_APPLY_WAIT', '10'))
            if wait_sec > 0:
                time.sleep(wait_sec)

            # Perform status checks (best-effort)
            try:
                import subprocess
                nokube_path, lib_path = find_nokube_paths()
                if nokube_path:
                    # 构造运行环境以便找到动态库
                    env = os.environ.copy()
                    if lib_path:
                        env['LD_LIBRARY_PATH'] = lib_path + ':' + env.get('LD_LIBRARY_PATH', '')

                    # 清理可能存在的历史 webhook 部署（忽略失败）
                    try:
                        subprocess.run(
                            [nokube_path, 'delete', 'deployment', f'gitops-webhook-server-{args.cluster_name}', '--cluster', args.cluster_name],
                            capture_output=True, text=True, timeout=15, env=env
                        )
                    except Exception:
                        pass
                    _run_status_checks(nokube_path, lib_path, args.cluster_name)
                else:
                    print("⚠️  Skipping status checks: nokube binary not found.")
            except Exception as e:
                print(f"⚠️  Status checks encountered an error: {e}")

            print("")
            print("📊 You can also check with these commands:")
            print(f"   LD_LIBRARY_PATH=target ./target/nokube get deployments --cluster {args.cluster_name}")
            print(f"   LD_LIBRARY_PATH=target ./target/nokube get pods --cluster {args.cluster_name}")
            print(f"   LD_LIBRARY_PATH=target ./target/nokube describe deployment gitops-controller-{args.cluster_name} --cluster {args.cluster_name}")
            print(f"   LD_LIBRARY_PATH=target ./target/nokube get services --cluster {args.cluster_name}")
            print(f"   LD_LIBRARY_PATH=target ./target/nokube get daemonsets --cluster {args.cluster_name}")
            print("📋 Use 'nokube get --help' for more resource types and output formats")
        else:
            print(f"❌ Failed to deploy GitOps to cluster: {args.cluster_name}")
            return 1

def _run_status_checks(nokube_path: str, lib_path: str, cluster_name: str):
    import subprocess

    env = os.environ.copy()
    if lib_path:
        env['LD_LIBRARY_PATH'] = lib_path + ':' + env.get('LD_LIBRARY_PATH', '')

    def run(cmd: list, timeout: int = 30):
        try:
            res = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, env=env)
            ok = (res.returncode == 0)
            out = res.stdout.strip()
            err = res.stderr.strip()
            return ok, out, err
        except Exception as e:
            return False, '', str(e)

    checks = [
        [nokube_path, 'get', 'deployments', '--cluster', cluster_name],
        [nokube_path, 'get', 'pods', '--cluster', cluster_name],
        [nokube_path, 'get', 'services', '--cluster', cluster_name],
        [nokube_path, 'describe', 'deployment', f'gitops-controller-{cluster_name}', '--cluster', cluster_name],
    ]

    print("— nokube get deployments —")
    ok, out, err = run(checks[0])
    print(out or err)

    print("— nokube get pods —")
    ok, out, err = run(checks[1])
    print(out or err)

    print("— nokube get services —")
    ok, out, err = run(checks[2])
    print(out or err)

    print(f"— describe gitops-controller-{cluster_name} —")
    ok, out, err = run(checks[3], timeout=60)
    print(out or err)

    # webhook 相关检查已移除以简化逻辑

def deploy_to_nokube(manifest, cluster_name):
    """直接部署到NoKube集群"""
    try:
        import subprocess
        import tempfile
        
        # 查找nokube二进制文件和库文件
        nokube_path, lib_path = find_nokube_paths()
        if not nokube_path:
            print("❌ nokube binary not found in target directory. Please run build first.")
            return False
        
        print(f"📁 Using nokube binary: {nokube_path}")
        print(f"📚 Using libraries: {lib_path}")
        
        # 创建临时YAML文件
        with tempfile.NamedTemporaryFile(mode='w', suffix='.yaml', delete=False) as temp_file:
            yaml.dump(manifest, temp_file, default_flow_style=False, indent=2)
            temp_file_path = temp_file.name
        
        # 设置环境变量
        env = os.environ.copy()
        if lib_path:
            env['LD_LIBRARY_PATH'] = lib_path + ':' + env.get('LD_LIBRARY_PATH', '')
        
        # 调用nokube apply
        print(f"🚀 Deploying GitOps to cluster: {cluster_name}")
        result = subprocess.run([
            nokube_path, 'apply', '-f', temp_file_path, '--cluster', cluster_name
        ], capture_output=True, text=True, timeout=60, env=env)
        
        # 清理临时文件
        os.unlink(temp_file_path)
        
        if result.returncode == 0:
            print(result.stdout)
            return True
        else:
            print(f"❌ NoKube apply failed:")
            print(result.stderr)
            return False
            
    except subprocess.TimeoutExpired:
        print("❌ NoKube apply timed out")
        return False
    except Exception as e:
        print(f"❌ Error deploying to NoKube: {e}")
        return False

def find_nokube_paths():
    """查找nokube二进制文件和库文件路径"""
    # 从当前脚本位置向上找target目录
    current_dir = os.path.dirname(os.path.abspath(__file__))
    
    # 可能的target目录位置
    target_paths = [
        os.path.join(current_dir, "..", "..", "target"),  # examples/gitops -> target
        os.path.join(current_dir, "..", "..", "..", "target"),  # 如果嵌套更深
        os.path.join(os.getcwd(), "target"),  # 当前工作目录下的target
        "./target",
        "../target", 
        "../../target"
    ]
    
    for target_dir in target_paths:
        abs_target = os.path.abspath(target_dir)
        if os.path.exists(abs_target):
            # 检查nokube二进制文件
            nokube_path = os.path.join(abs_target, "nokube")
            if os.path.exists(nokube_path) and os.access(nokube_path, os.X_OK):
                return nokube_path, abs_target
    
    return None, None


if __name__ == "__main__":
    main()
