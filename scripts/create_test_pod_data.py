#!/usr/bin/env python3

import json
import subprocess
import sys
from datetime import datetime, timezone

def create_test_pod_data():
    """创建测试用的pod数据"""
    
    # gitops-controller pod数据
    gitops_pod_data = {
        "name": "gitops-controller",
        "namespace": "default", 
        "node": "pinghu-container",
        "image": "python:3.10-slim",
        "container_id": "docker://abc123def456",
        "status": "ContainerCreating",
        "ready": False,
        "restart_count": 0,
        "start_time": datetime.now(timezone.utc).isoformat(),
        "pod_ip": None,
        "labels": {
            "app": "gitops",
            "component": "controller"
        },
        "ports": ["8080/TCP"],
        "priority": 0
    }
    
    # pod事件数据
    gitops_events = [
        {
            "type": "Normal",
            "reason": "Scheduled",
            "message": "Successfully assigned default/gitops-controller to pinghu-container",
            "age": "2m",
            "timestamp": datetime.now(timezone.utc).isoformat()
        },
        {
            "type": "Normal", 
            "reason": "Pulling",
            "message": "Pulling image \"python:3.10-slim\"",
            "age": "2m",
            "timestamp": datetime.now(timezone.utc).isoformat()
        },
        {
            "type": "Warning",
            "reason": "FailedCreatePodSandBox",
            "message": "Failed to create pod sandbox: network not ready", 
            "age": "1m",
            "timestamp": datetime.now(timezone.utc).isoformat()
        }
    ]
    
    return gitops_pod_data, gitops_events

def write_to_etcd(key, value):
    """向etcd写入数据"""
    try:
        # 使用etcdctl写入数据
        cmd = [
            "docker", "exec", "test-etcd-pa",
            "etcdctl", "--endpoints=localhost:2379", 
            "put", key, json.dumps(value, ensure_ascii=False)
        ]
        
        result = subprocess.run(cmd, capture_output=True, text=True, check=True)
        print(f"✅ 成功写入 {key}")
        return True
        
    except subprocess.CalledProcessError as e:
        print(f"❌ 写入失败 {key}: {e.stderr}")
        return False
    except Exception as e:
        print(f"❌ 写入出错 {key}: {e}")
        return False

def main():
    """主函数"""
    print("🚀 开始写入测试pod数据到etcd...")
    
    cluster_name = "home-cluster"
    pod_name = "gitops-controller"
    
    # 创建测试数据
    pod_data, events_data = create_test_pod_data()
    
    # 写入pod数据
    pod_key = f"/nokube/{cluster_name}/pods/{pod_name}"
    if not write_to_etcd(pod_key, pod_data):
        sys.exit(1)
    
    # 写入事件数据
    events_key = f"/nokube/{cluster_name}/events/pod/{pod_name}"
    if not write_to_etcd(events_key, events_data):
        sys.exit(1)
    
    print("✅ 所有测试数据写入完成！")
    print(f"现在可以运行: ./target/nokube describe pod {pod_name} --cluster {cluster_name}")

if __name__ == "__main__":
    main()