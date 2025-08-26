#!/usr/bin/env python3
"""
Complete end-to-end testing script for nokube-rs
Creates containerized test environment with master and worker nodes
"""

import os
import sys
import subprocess
import json
import yaml
import tempfile
import time
from pathlib import Path
from typing import Dict, List, Optional


class NokubeE2ETest:
    def __init__(self, project_dir: str):
        self.project_dir = Path(project_dir).resolve()
        self.scripts_dir = self.project_dir / "scripts"
        self.target_dir = self.project_dir / "target"
        
        # Container configuration
        self.master_container = "nokube_test_master"
        self.worker_container = "nokube_test_worker"
        self.etcd_container = "nokube_test_etcd"
        self.network_name = "nokube_test_network"
        
        # Test configuration
        self.cluster_name = "test-cluster"
        self.etcd_endpoints = ["etcd:2379"]
        self.etcd_host_port = "12379"  # Use different port to avoid conflicts
        
    def run_command(self, cmd: List[str], check: bool = True, capture_output: bool = True) -> subprocess.CompletedProcess:
        """Execute command with proper error handling"""
        try:
            print(f"🔧 Running: {' '.join(cmd)}")
            result = subprocess.run(cmd, check=check, capture_output=capture_output, text=True)
            if result.stdout and capture_output:
                print(f"   Output: {result.stdout.strip()}")
            return result
        except subprocess.CalledProcessError as e:
            print(f"❌ Command failed: {e}")
            if e.stdout:
                print(f"   STDOUT: {e.stdout}")
            if e.stderr:
                print(f"   STDERR: {e.stderr}")
            if check:
                raise
            return e
    
    def cleanup_previous_test(self):
        """Clean up any previous test containers and networks"""
        print("🧹 Cleaning up previous test environment...")
        
        containers = [self.master_container, self.worker_container, self.etcd_container]
        for container in containers:
            self.run_command(["docker", "stop", container], check=False)
            self.run_command(["docker", "rm", container], check=False)
        
        self.run_command(["docker", "network", "rm", self.network_name], check=False)
    
    def build_nokube_binary(self):
        """Build nokube binary using containerized build"""
        print("🔨 Building nokube binary...")
        
        build_script = self.scripts_dir / "build_with_container.py"
        self.run_command(["python3", str(build_script)], capture_output=False)
        
        # Verify binary exists
        binary_path = self.target_dir / "nokube"
        if not binary_path.exists():
            raise RuntimeError(f"Binary not found at {binary_path}")
        
        print(f"✅ Binary built successfully: {binary_path}")
        return binary_path
    
    def create_test_network(self):
        """Create Docker network for test containers"""
        print("🌐 Creating test network...")
        self.run_command([
            "docker", "network", "create", 
            "--driver", "bridge",
            self.network_name
        ])
    
    def start_etcd_container(self):
        """Start etcd container for cluster coordination"""
        print("🗃️ Starting etcd container...")
        
        self.run_command([
            "docker", "run", "-d",
            "--name", self.etcd_container,
            "--network", self.network_name,
            "--network-alias", "etcd",
            "-p", f"{self.etcd_host_port}:2379",
            "-p", "12380:2380",
            "quay.io/coreos/etcd:v3.5.0",
            "/usr/local/bin/etcd",
            "--name", "etcd0",
            "--data-dir", "/etcd-data",
            "--listen-client-urls", "http://0.0.0.0:2379",
            "--advertise-client-urls", "http://etcd:2379",
            "--listen-peer-urls", "http://0.0.0.0:2380",
            "--initial-advertise-peer-urls", "http://etcd:2380",
            "--initial-cluster", "etcd0=http://etcd:2380",
            "--initial-cluster-token", "etcd-cluster-1",
            "--initial-cluster-state", "new",
            "--log-level", "info",
            "--logger", "zap",
            "--log-outputs", "stderr"
        ])
        
        # Wait for etcd to be ready
        print("⏳ Waiting for etcd to be ready...")
        time.sleep(5)
        
        # Test etcd connectivity
        self.run_command([
            "docker", "exec", self.etcd_container,
            "/usr/local/bin/etcdctl", "endpoint", "health"
        ])
        
        print("✅ etcd is ready")
    
    def create_test_configs(self) -> Dict[str, Path]:
        """Create configuration files for master and worker nodes"""
        print("📝 Creating test configurations...")
        
        # Create temporary directory for configs
        config_dir = Path(tempfile.mkdtemp(prefix="nokube_test_"))
        
        # Master node config
        master_config = {
            "etcd_endpoints": self.etcd_endpoints
        }
        
        master_config_path = config_dir / "master_config.yaml"
        with open(master_config_path, 'w') as f:
            yaml.dump(master_config, f)
        
        # Worker node config
        worker_config = {
            "etcd_endpoints": self.etcd_endpoints
        }
        
        worker_config_path = config_dir / "worker_config.yaml"
        with open(worker_config_path, 'w') as f:
            yaml.dump(worker_config, f)
        
        # Cluster configuration
        cluster_config = {
            "name": self.cluster_name,
            "nodes": [
                {
                    "id": "master-node",
                    "host": "master",
                    "role": "master",
                    "ssh_port": 22,
                    "username": "root"
                },
                {
                    "id": "worker-node",
                    "host": "worker", 
                    "role": "worker",
                    "ssh_port": 22,
                    "username": "root"
                }
            ],
            "services": [],
            "monitoring": {
                "enabled": True,
                "metrics_interval": 30,
                "config_poll_interval": 60,
                "prometheus_push_gateway": "http://localhost:9091"
            }
        }
        
        cluster_config_path = config_dir / "cluster_config.yaml"
        with open(cluster_config_path, 'w') as f:
            yaml.dump(cluster_config, f)
        
        print(f"✅ Configurations created in {config_dir}")
        return {
            "config_dir": config_dir,
            "master_config": master_config_path,
            "worker_config": worker_config_path,
            "cluster_config": cluster_config_path
        }
    
    def create_test_dockerfile(self, config_path: Path) -> Path:
        """Create Dockerfile for test containers with nokube binary and config"""
        dockerfile_content = f"""
FROM ubuntu:22.04

# Install basic dependencies
RUN apt-get update && apt-get install -y \\
    curl \\
    wget \\
    net-tools \\
    htop \\
    iotop \\
    python3 \\
    python3-pip \\
    openssh-server \\
    && rm -rf /var/lib/apt/lists/*

# Install psutil for monitoring
RUN pip3 install psutil

# Create nokube directories
RUN mkdir -p /opt/nokube-agent \\
    /var/log/nokube \\
    /etc/nokube \\
    ~/.nokube

# Copy nokube binary
COPY nokube /usr/local/bin/nokube
RUN chmod +x /usr/local/bin/nokube

# Copy configuration
COPY config.yaml ~/.nokube/config.yaml

# Setup SSH
RUN mkdir /var/run/sshd
RUN echo 'root:nokube123' | chpasswd
RUN sed -i 's/#PermitRootLogin prohibit-password/PermitRootLogin yes/' /etc/ssh/sshd_config

# Expose SSH port
EXPOSE 22

# Start script
RUN echo '#!/bin/bash\\n\\
service ssh start\\n\\
echo "Container ready"\\n\\
tail -f /dev/null' > /start.sh
RUN chmod +x /start.sh

CMD ["/start.sh"]
"""
        
        dockerfile_path = config_path.parent / "Dockerfile"
        with open(dockerfile_path, 'w') as f:
            f.write(dockerfile_content)
        
        return dockerfile_path
    
    def build_test_image(self, configs: Dict[str, Path], node_type: str) -> str:
        """Build Docker image for test node"""
        print(f"🐳 Building {node_type} test image...")
        
        build_dir = configs["config_dir"] / node_type
        build_dir.mkdir(exist_ok=True)
        
        # Copy binary and config
        binary_path = self.target_dir / "nokube"
        config_key = f"{node_type}_config"
        
        import shutil
        shutil.copy2(binary_path, build_dir / "nokube")
        shutil.copy2(configs[config_key], build_dir / "config.yaml")
        
        # Create Dockerfile
        dockerfile = self.create_test_dockerfile(build_dir)
        shutil.copy2(dockerfile, build_dir / "Dockerfile")
        
        # Build image
        image_name = f"nokube_test_{node_type}:latest"
        self.run_command([
            "docker", "build", "-t", image_name,
            str(build_dir)
        ], capture_output=False)
        
        print(f"✅ {node_type} image built: {image_name}")
        return image_name
    
    def start_test_containers(self, master_image: str, worker_image: str):
        """Start master and worker test containers"""
        print("🚀 Starting test containers...")
        
        # Start master container
        self.run_command([
            "docker", "run", "-d",
            "--name", self.master_container,
            "--network", self.network_name,
            "--network-alias", "master",
            "-p", "2222:22",  # SSH port
            master_image
        ])
        
        # Start worker container
        self.run_command([
            "docker", "run", "-d", 
            "--name", self.worker_container,
            "--network", self.network_name,
            "--network-alias", "worker",
            "-p", "2223:22",  # SSH port
            worker_image
        ])
        
        # Wait for containers to be ready
        print("⏳ Waiting for containers to be ready...")
        time.sleep(10)
        
        print("✅ Test containers started")
    
    def test_nokube_functionality(self):
        """Test nokube binary functionality in containers"""
        print("🧪 Testing nokube functionality...")
        
        # Test config discovery on master
        print("🔍 Testing config discovery on master...")
        result = self.run_command([
            "docker", "exec", self.master_container,
            "nokube", "init", "--cluster-name", self.cluster_name
        ])
        
        if result.returncode == 0:
            print("✅ Master node config discovery and cluster init successful")
        else:
            print("❌ Master node test failed")
            return False
        
        # Test config discovery on worker  
        print("🔍 Testing config discovery on worker...")
        result = self.run_command([
            "docker", "exec", self.worker_container,
            "nokube", "init", "--cluster-name", f"{self.cluster_name}-worker"
        ])
        
        if result.returncode == 0:
            print("✅ Worker node config discovery successful")
        else:
            print("❌ Worker node test failed")
            return False
        
        # Test other commands
        commands_to_test = [
            ["nokube", "--help"],
            ["nokube", "init", "--help"],
        ]
        
        for cmd in commands_to_test:
            print(f"🧪 Testing command: {' '.join(cmd)}")
            result = self.run_command(["docker", "exec", self.master_container] + cmd)
            if result.returncode != 0:
                print(f"❌ Command failed: {' '.join(cmd)}")
                return False
        
        return True
    
    def show_test_results(self):
        """Show test environment information"""
        print("📊 Test Environment Information:")
        print(f"   Master container: {self.master_container} (SSH: localhost:2222)")
        print(f"   Worker container: {self.worker_container} (SSH: localhost:2223)")
        print(f"   etcd container: {self.etcd_container} (Client: localhost:{self.etcd_host_port})")
        print(f"   Network: {self.network_name}")
        print(f"   Cluster name: {self.cluster_name}")
        
        print("🔧 Manual testing commands:")
        print(f"   docker exec -it {self.master_container} bash")
        print(f"   docker exec -it {self.worker_container} bash")
        print(f"   docker exec {self.master_container} nokube --help")
        
    def run_full_test(self):
        """Run complete end-to-end test"""
        print("🚀 Starting nokube-rs E2E test...")
        
        try:
            # Step 1: Cleanup
            self.cleanup_previous_test()
            
            # Step 2: Build binary
            self.build_nokube_binary()
            
            # Step 3: Create network
            self.create_test_network()
            
            # Step 4: Start etcd
            self.start_etcd_container()
            
            # Step 5: Create configs
            configs = self.create_test_configs()
            
            # Step 6: Build test images
            master_image = self.build_test_image(configs, "master")
            worker_image = self.build_test_image(configs, "worker")
            
            # Step 7: Start containers
            self.start_test_containers(master_image, worker_image)
            
            # Step 8: Test functionality
            if self.test_nokube_functionality():
                print("🎉 All tests passed!")
                self.show_test_results()
                return True
            else:
                print("❌ Some tests failed!")
                return False
                
        except Exception as e:
            print(f"💥 Test failed with error: {e}")
            return False


def main():
    if len(sys.argv) > 1:
        project_dir = sys.argv[1]
    else:
        project_dir = Path(__file__).parent.parent
    
    tester = NokubeE2ETest(project_dir)
    
    if "--cleanup-only" in sys.argv:
        tester.cleanup_previous_test()
        print("🧹 Cleanup completed")
        return
    
    success = tester.run_full_test()
    sys.exit(0 if success else 1)


if __name__ == "__main__":
    main()