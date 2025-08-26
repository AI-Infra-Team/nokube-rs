#!/usr/bin/env python3
"""
Container-based build script that ensures image exists, builds inside it, 
and copies the binary to a mounted output directory.
"""

import os
import sys
import subprocess
import argparse
from pathlib import Path


def sudo_prefix() -> list:
    """Return [sudo, -E] if not running as root, empty list otherwise."""
    return ["sudo", "-E"] if os.geteuid() != 0 else []


def main():
    os.chdir(Path(__file__).absolute().parent)
    parser = argparse.ArgumentParser(description="Build with container and copy output")
    parser.add_argument("--output-dir", "-o", default="../target", 
                       help="Output directory to copy the built binary (default: ../target)")
    parser.add_argument("--force-rebuild", "-f", action="store_true",
                       help="Force rebuild even if container is running")
    
    args = parser.parse_args()
    
    try:
        builder = ContainerBuilder(Path(__file__).absolute().parent.parent, args.output_dir)
        builder.build_and_copy(force_rebuild=args.force_rebuild)
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)


class ContainerBuilder:
    def __init__(self, project_dir: str, output_dir: str):
        self.project_dir = Path(project_dir).resolve()
        self.output_dir = Path(output_dir).resolve()
        self.scripts_dir = self.project_dir / "scripts"
        
        # Ensure output directory exists
        self.output_dir.mkdir(parents=True, exist_ok=True)
        
    def _ensure_image_exists(self) -> str:
        """Ensure build image exists by calling prepare_build_img.py"""
        print("Ensuring build image exists...")
        try:
            # Run without capturing output for real-time progress
            subprocess.run([
                "python3", "prepare_build_img.py"
            ], cwd=self.scripts_dir, check=True)
            
            # Since we're not capturing output, need to find the image directly
            result = subprocess.run(
                sudo_prefix() + ["docker", "images", "--format", "{{.Repository}}:{{.Tag}}"],
                capture_output=True, text=True, check=True)
            
            for line in result.stdout.split('\n'):
                if 'build:latest' in line:
                    return line.strip()
                    
            raise RuntimeError("Could not determine build image name")
            
        except subprocess.CalledProcessError as e:
            print(f"Failed to prepare build image: {e}")
            raise
    
    def _container_running(self, container_name: str) -> bool:
        """Check if build container is running."""
        try:
            result = subprocess.run(
                sudo_prefix() + ["docker", "ps", "--filter", f"name={container_name}", 
                "--format", "{{.Names}}"],
                capture_output=True, text=True, check=True)
            return container_name in result.stdout
        except subprocess.CalledProcessError:
            return False
    
    def _stop_and_remove_container(self, container_name: str) -> None:
        """Stop and remove existing container."""
        try:
            subprocess.run(sudo_prefix() + ["docker", "stop", container_name], 
                          capture_output=True, check=True)
            print(f"Stopped existing container: {container_name}")
        except subprocess.CalledProcessError:
            pass
        
        try:
            subprocess.run(sudo_prefix() + ["docker", "rm", container_name], 
                          capture_output=True, check=True)
            print(f"Removed existing container: {container_name}")
        except subprocess.CalledProcessError:
            pass
    
    def _start_container(self, image_name: str, container_name: str) -> None:
        """Start the build container with volume mounts."""
        # Mount the project directory and output directory
        mount_args = [
            "-v", f"{self.project_dir}:/app",
            "-v", f"{self.output_dir}:/output"
        ]
        
        cmd = sudo_prefix() + [
            "docker", "run", "-d", "--name", container_name
        ] + mount_args + [image_name]
        
        subprocess.run(cmd, check=True)
        print(f"Started container: {container_name}")
    
    def _exec_in_container(self, container_name: str, command: list) -> subprocess.CompletedProcess:
        """Execute a command inside the container."""
        cmd = sudo_prefix() + ["docker", "exec", container_name] + command
        return subprocess.run(cmd, check=True, capture_output=True, text=True)
    
    def _build_in_container(self, container_name: str) -> None:
        """Build the project inside the container."""
        print("Building project in container...")
        
        # Detect proxy environment variables
        proxy_vars = [
            'http_proxy', 'HTTP_PROXY',
            'https_proxy', 'HTTPS_PROXY', 
            'ftp_proxy', 'FTP_PROXY',
            'no_proxy', 'NO_PROXY',
            'all_proxy', 'ALL_PROXY'
        ]
        
        env_args = []
        for var in proxy_vars:
            value = os.environ.get(var)
            if value:
                env_args.extend(["-e", f"{var}={value}"])
                print(f"Passing proxy env to container: {var}={value}")
        
        # Change to app directory and build with real-time output
        try:
            cmd = sudo_prefix() + ["docker", "exec"] + env_args + [container_name, "bash", "-c", "echo 'Environment variables:' && env | grep -i proxy; cd /app && cargo build --release"]
            # Run without capturing output to show real-time progress
            subprocess.run(cmd, check=True)
            print("Build completed successfully")
        except subprocess.CalledProcessError as e:
            print(f"Build failed: {e}")
            raise
    
    def _copy_binary_to_output(self, container_name: str) -> None:
        """Copy the built binary and required libraries to the output directory."""
        print("Copying binary and libraries to output directory...")
        
        # Get the binary name from Cargo.toml
        binary_name = self._get_binary_name()
        
        # Copy the binary from container to output directory
        try:
            self._exec_in_container(container_name, [
                "cp", f"/app/target/release/{binary_name}", f"/output/{binary_name}"
            ])
            print(f"Binary copied to: {self.output_dir}/{binary_name}")
            
            # Copy required SSL libraries
            print("Copying SSL libraries...")
            self._exec_in_container(container_name, [
                "bash", "-c", "find /usr/lib/x86_64-linux-gnu -name 'libssl.so*' -exec cp {} /output/ \\;"
            ])
            self._exec_in_container(container_name, [
                "bash", "-c", "find /usr/lib/x86_64-linux-gnu -name 'libcrypto.so*' -exec cp {} /output/ \\;"
            ])
            print("SSL libraries copied to output directory")
            
        except subprocess.CalledProcessError as e:
            print(f"Failed to copy binary or libraries: {e}")
            raise
    
    def _get_binary_name(self) -> str:
        """Get the binary name from Cargo.toml."""
        cargo_toml = self.project_dir / "Cargo.toml"
        if cargo_toml.exists():
            with open(cargo_toml, 'r') as f:
                for line in f:
                    if line.startswith('name = '):
                        # Extract name from 'name = "nokube-rs"'
                        name = line.split('=')[1].strip().strip('"')
                        # Check if there's a [[bin]] section with different name
                        break
            
            # Look for [[bin]] section
            with open(cargo_toml, 'r') as f:
                content = f.read()
                if '[[bin]]' in content:
                    lines = content.split('\n')
                    in_bin_section = False
                    for line in lines:
                        if line.strip().startswith('[[bin]]'):
                            in_bin_section = True
                        elif line.strip().startswith('name = ') and in_bin_section:
                            return line.split('=')[1].strip().strip('"')
                        elif line.strip().startswith('[') and in_bin_section and not line.strip().startswith('[[bin]]'):
                            break
            
            return name.replace('-', '_')  # Default behavior for binary names
        return "app"  # fallback
    
    def build_and_copy(self, force_rebuild: bool = False) -> None:
        """Main build and copy process."""
        # Ensure image exists
        image_name = self._ensure_image_exists()
        print(f"Using image: {image_name}")
        
        # Generate container name from image name
        container_name = image_name.replace('_build:latest', '').replace(':', '_')
        
        # Handle existing container
        if self._container_running(container_name):
            if force_rebuild:
                print("Force rebuild requested, stopping existing container...")
                self._stop_and_remove_container(container_name)
            else:
                print("Container already running, using existing container...")
        else:
            # Remove any stopped container with the same name
            self._stop_and_remove_container(container_name)
        
        # Start container if not running
        if not self._container_running(container_name):
            self._start_container(image_name, container_name)
        
        try:
            # Build in container
            self._build_in_container(container_name)
            
            # Copy binary to output
            self._copy_binary_to_output(container_name)
            
            print(f"Build and copy completed successfully!")
            print(f"Output binary: {self.output_dir}/{self._get_binary_name()}")
            
        except Exception as e:
            print(f"Build process failed: {e}")
            raise


if __name__ == "__main__":
    main()