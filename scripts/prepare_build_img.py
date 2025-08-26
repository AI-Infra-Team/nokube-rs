#!/usr/bin/env python3
"""
Docker build toolchain with dynamic Dockerfile generation and dependency caching.
"""

import os
import sys
import json
import yaml
import hashlib
import subprocess
import argparse
import re
from pathlib import Path
from typing import Dict, List, Set, Tuple


def sudo_prefix() -> list:
    """Return [sudo, -E] if not running as root, empty list otherwise."""
    return ["sudo", "-E"] if os.geteuid() != 0 else []


def main():
    os.chdir(Path(__file__).absolute().parent)
    parser = argparse.ArgumentParser(description="Docker build toolchain with optimized Dockerfile generation")
    parser.add_argument("--clean", action="store_true", help="Clean build images and cache")
    
    args = parser.parse_args()
    
    try:
        builder = DockerBuildTool(Path(__file__).absolute().parent.parent)
        
        if args.clean:
            # Clean up build images
            build_image_name = f"{builder.container_name}_build:latest"
            try:
                subprocess.run(sudo_prefix() + ["docker", "rmi", build_image_name], check=True)
                print(f"Removed image: {build_image_name}")
            except subprocess.CalledProcessError:
                print("No image to remove")
            
            # Clean up cache
            if builder.cache_file.exists():
                builder.cache_file.unlink()
                print("Removed dependency cache")
        else:
            builder.build()
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)


class DockerBuildTool:
    def __init__(self, project_dir: str = "."):
        self.project_dir = Path(project_dir).resolve()
        self.scripts_dir = self.project_dir / "scripts"
        self.deps_file = self.scripts_dir / "deps.yaml"
        self.cache_file = self.scripts_dir / "deps_cache.json"
        self.dockerfile_path = self.scripts_dir / "Dockerfile"
        
        # Load configuration first to get project-specific settings
        self.config = self.load_dependencies()
        
        # Generate container name from config or project path
        self.container_name = self._generate_container_name()
        
        # Detect proxy environment variables
        self.proxy_env = self._detect_proxy_env()
        
        self.scripts_dir.mkdir(exist_ok=True)
    
    def _generate_container_name(self) -> str:
        """Generate container name from config or project path."""
        project_config = self.config.get("project", {})
        
        # Use configured project name if available
        if "name" in project_config:
            project_name = project_config["name"]
        else:
            # Fallback to path-based name
            path_str = str(self.project_dir)
            # Remove leading slash and replace special chars
            project_name = re.sub(r'[^a-zA-Z0-9_-]', '_', path_str.lstrip('/'))
            # Remove multiple underscores
            project_name = re.sub(r'_+', '_', project_name).strip('_')
        
        return f"{project_name}_build"
    
    def _detect_proxy_env(self) -> Dict[str, str]:
        """Detect proxy environment variables from current environment."""
        proxy_vars = [
            'http_proxy', 'HTTP_PROXY',
            'https_proxy', 'HTTPS_PROXY', 
            'ftp_proxy', 'FTP_PROXY',
            'no_proxy', 'NO_PROXY',
            'all_proxy', 'ALL_PROXY'
        ]
        
        detected = {}
        for var in proxy_vars:
            value = os.environ.get(var)
            if value:
                detected[var] = value
                print(f"Detected proxy env: {var}={value}")
        
        return detected
    
    def _container_exists(self) -> bool:
        """Check if build container exists."""
        try:
            result = subprocess.run(
                sudo_prefix() + ["docker", "ps", "-a", "--filter", f"name={self.container_name}", "--format", "{{.Names}}"],
                capture_output=True, text=True, check=True
            )
            return self.container_name in result.stdout
        except subprocess.CalledProcessError:
            return False
    
    def _container_running(self) -> bool:
        """Check if build container is running."""
        try:
            result = subprocess.run(
                sudo_prefix() + ["docker", "ps", "--filter", f"name={self.container_name}", "--format", "{{.Names}}"],
                capture_output=True, text=True, check=True
            )
            return self.container_name in result.stdout
        except subprocess.CalledProcessError:
            return False
    
    def load_dependencies(self) -> Dict:
        """Load dependency configuration from YAML file."""
        if not self.deps_file.exists():
            raise FileNotFoundError(f"Dependencies file not found: {self.deps_file}")
        
        with open(self.deps_file, 'r') as f:
            return yaml.safe_load(f)
    
    def load_cache(self) -> Dict:
        """Load previous dependency cache."""
        if not self.cache_file.exists():
            return {
                "apt_packages": [], 
                "pip_packages": [], 
                "hash": "",
                "cached_deps_order": {
                    "apt_packages": [],
                    "pip_packages": []
                }
            }
        
        with open(self.cache_file, 'r') as f:
            cache = json.load(f)
            # Ensure cached_deps_order exists for backward compatibility
            if "cached_deps_order" not in cache:
                cache["cached_deps_order"] = {
                    "apt_packages": cache.get("apt_packages", []),
                    "pip_packages": cache.get("pip_packages", [])
                }
            return cache
    
    def save_cache(self, deps: Dict, cached_deps_order: Dict[str, List[str]]) -> None:
        """Save current dependencies to cache with order information."""
        cache_data = {
            "apt_packages": deps.get("apt_packages", []),
            "pip_packages": deps.get("pip_packages", []),
            "hash": self._compute_deps_hash(deps),
            "cached_deps_order": cached_deps_order
        }
        
        with open(self.cache_file, 'w') as f:
            json.dump(cache_data, f, indent=2)
    
    def _compute_deps_hash(self, deps: Dict) -> str:
        """Compute hash of dependencies for change detection."""
        deps_str = json.dumps({
            "apt_packages": sorted(deps.get("apt_packages", [])),
            "pip_packages": sorted(deps.get("pip_packages", [])),
            "system": deps.get("system", {})
        }, sort_keys=True)
        return hashlib.sha256(deps_str.encode()).hexdigest()
    
    def detect_changes(self, current_deps: Dict, cached_deps: Dict) -> Dict[str, Set[str]]:
        """Detect changes in dependencies."""
        current_apt = set(current_deps.get("apt_packages", []))
        cached_apt = set(cached_deps.get("apt_packages", []))
        
        current_pip = set(current_deps.get("pip_packages", []))
        cached_pip = set(cached_deps.get("pip_packages", []))
        
        return {
            "apt_added": current_apt - cached_apt,
            "apt_removed": cached_apt - current_apt,
            "pip_added": current_pip - cached_pip,
            "pip_removed": cached_pip - current_pip
        }
    
    def _compute_cached_deps_order(self, current_deps: Dict, cached_deps: Dict, changes: Dict) -> Tuple[Dict[str, List[str]], int]:
        """Compute optimal dependency order for caching and determine invalidation point."""
        cached_order = cached_deps.get("cached_deps_order", {"apt_packages": [], "pip_packages": []})
        
        new_order = {"apt_packages": [], "pip_packages": []}
        invalidation_step = -1  # -1 means no invalidation needed
        
        # For APT packages
        current_apt = set(current_deps.get("apt_packages", []))
        cached_apt_order = cached_order.get("apt_packages", [])
        apt_added = changes.get("apt_added", set())
        apt_removed = changes.get("apt_removed", set())
        
        step_idx = 1
        
        # Process existing APT packages in cached order
        for pkg in cached_apt_order:
            if pkg in current_apt:  # Package still needed
                new_order["apt_packages"].append(pkg)
                if invalidation_step == -1 and pkg in apt_removed:
                    # This shouldn't happen, but handle it
                    invalidation_step = step_idx
                step_idx += 1
            else:
                # Package removed, invalidation starts here
                if invalidation_step == -1:
                    invalidation_step = step_idx
        
        # Add new APT packages at the end
        for pkg in sorted(apt_added):
            new_order["apt_packages"].append(pkg)
            if invalidation_step == -1:
                invalidation_step = step_idx  # First new package invalidates cache
            step_idx += 1
        
        # For PIP packages
        current_pip = set(current_deps.get("pip_packages", []))
        cached_pip_order = cached_order.get("pip_packages", [])
        pip_added = changes.get("pip_added", set())
        pip_removed = changes.get("pip_removed", set())
        
        # Process existing PIP packages in cached order
        for pkg in cached_pip_order:
            if pkg in current_pip:  # Package still needed
                new_order["pip_packages"].append(pkg)
                if invalidation_step == -1 and pkg in pip_removed:
                    # This shouldn't happen, but handle it
                    invalidation_step = step_idx
                step_idx += 1
            else:
                # Package removed, invalidation starts here
                if invalidation_step == -1:
                    invalidation_step = step_idx
        
        # Add new PIP packages at the end
        for pkg in sorted(pip_added):
            new_order["pip_packages"].append(pkg)
            if invalidation_step == -1:
                invalidation_step = step_idx  # First new package invalidates cache
            step_idx += 1
            
        return new_order, invalidation_step
    
    def generate_dockerfile(self, deps: Dict, cached_deps_order: Dict[str, List[str]], invalidation_step: int = -1) -> str:
        """Generate optimized Dockerfile with step-by-step caching strategy."""
        system_config = deps.get("system", {})
        project_config = deps.get("project", {})
        build_config = deps.get("build", {})
        
        base_image = system_config.get("base_image", "ubuntu:18.04")
        rust_version = system_config.get("rust_version", "1.70.0")
        
        # Project configuration
        project_name = project_config.get("name", "app")
        project_type = project_config.get("type", "rust")  # rust, nodejs, python, etc.
        main_executable = project_config.get("main_executable", project_name)
        
        # Build configuration
        build_dependencies = build_config.get("dependencies", ["Cargo.toml", "Cargo.lock"])
        if "build.rs" in self.project_dir.glob("*"):
            build_dependencies.append("build.rs")
        
        dockerfile_content = f"""# Generated Dockerfile - Do not edit manually
# Project: {project_name} ({project_type})
# Cache invalidation starts at step: {invalidation_step if invalidation_step != -1 else 'none'}
FROM {base_image} AS base

# Set environment variables
ENV DEBIAN_FRONTEND=noninteractive
"""

        # Add language-specific environment variables
        if project_type == "rust":
            dockerfile_content += f"""ENV RUST_VERSION={rust_version}
ENV PATH="/root/.cargo/bin:${{PATH}}"
"""

        # Add proxy environment variables to Dockerfile
        if self.proxy_env:
            dockerfile_content += "\n# Proxy environment variables\n"
            for env_var, env_value in self.proxy_env.items():
                dockerfile_content += f"ENV {env_var}={env_value}\n"
            dockerfile_content += "\n"

        dockerfile_content += """# Update package lists (always at the beginning for base layer caching)
RUN apt-get update

# Install base system packages (cached layer)
RUN apt-get install -y \\
    curl \\
    build-essential \\
    pkg-config \\
    libssl-dev \\
    ca-certificates \\
    gnupg \\
    lsb-release \\
    && rm -rf /var/lib/apt/lists/*

# Install Python3 and pip (cached layer)
RUN apt-get update && apt-get install -y python3 python3-pip \\
    && rm -rf /var/lib/apt/lists/*

"""

        # Add language-specific installation
        if project_type == "rust":
            dockerfile_content += f"""# Install Rust (cached layer)
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain {rust_version} \\
    && rustup component add rustfmt clippy

"""

        step_idx = 1
        
        # Add APT packages step by step for optimal caching
        apt_packages = cached_deps_order.get("apt_packages", [])
        for pkg in apt_packages:
            cache_status = "# CACHE MISS - rebuilding from here" if step_idx >= invalidation_step and invalidation_step != -1 else ""
            dockerfile_content += f"""# Install APT package: {pkg} (step {step_idx}) {cache_status}
FROM base AS step_{step_idx}
RUN apt-get update && apt-get install -y {pkg} \\
    && rm -rf /var/lib/apt/lists/*

"""
            step_idx += 1
            
        # Add PIP packages step by step for optimal caching  
        pip_packages = cached_deps_order.get("pip_packages", [])
        prev_stage = f"step_{step_idx-1}" if apt_packages else "base"
        
        for pkg in pip_packages:
            cache_status = "# CACHE MISS - rebuilding from here" if step_idx >= invalidation_step and invalidation_step != -1 else ""
            dockerfile_content += f"""# Install PIP package: {pkg} (step {step_idx}) {cache_status}
FROM {prev_stage} AS step_{step_idx}
RUN pip3 install {pkg}

"""
            prev_stage = f"step_{step_idx}"
            step_idx += 1
        
        # Final build stage
        final_stage = f"step_{step_idx-1}" if (apt_packages or pip_packages) else "base"
        
        dockerfile_content += f"""# Final build stage
FROM {final_stage} AS build

# Set working directory
WORKDIR /app

"""

        # Copy build dependencies based on project type
        if project_type == "rust":
            dockerfile_content += f"""# Copy Cargo files for dependency caching
COPY {' '.join(build_dependencies)} ./

# Create src directory and dummy main for dependency compilation
RUN mkdir src && echo "fn main() {{}}" > src/main.rs

# Build dependencies (cached layer)
RUN cargo build --release && rm -rf src/

# Copy source code
COPY src/ src/

# Build the application
RUN cargo build --release && echo "v1"

"""
        elif project_type == "nodejs":
            dockerfile_content += f"""# Copy package files for dependency caching
COPY package*.json ./

# Install dependencies
RUN npm ci --only=production

# Copy source code
COPY . .

# Build the application
RUN npm run build

"""

        # Keep container alive instead of runtime stage
        dockerfile_content += """# Keep container alive
CMD ["tail", "-f", "/dev/null"]
"""
        
        return dockerfile_content
    
    def build(self) -> None:
        """Build project using Dockerfile with optimized caching."""
        # Load current dependencies
        current_deps = self.load_dependencies()
        
        # Load cached dependencies
        cached_deps = self.load_cache()
        
        # Detect changes
        changes = self.detect_changes(current_deps, cached_deps)
        current_hash = self._compute_deps_hash(current_deps)
        
        # Compute optimal dependency order for caching
        cached_deps_order, invalidation_step = self._compute_cached_deps_order(current_deps, cached_deps, changes)
        
        # Check if we need to rebuild
        rebuild_needed = (
            current_hash != cached_deps.get("hash", "") or
            any(changes.values()) or
            invalidation_step != -1
        )
        
        if rebuild_needed:
            if invalidation_step == -1:
                print("Dependencies changed, generating new Dockerfile...")
            else:
                print(f"Dependencies changed, cache will be invalidated from step {invalidation_step}")
            
            # Generate optimized Dockerfile
            dockerfile_content = self.generate_dockerfile(current_deps, cached_deps_order, invalidation_step)
            
            # Write Dockerfile
            with open(self.dockerfile_path, 'w') as f:
                f.write(dockerfile_content)
            
            # Save cache immediately after generating Dockerfile (not after build)
            self.save_cache(current_deps, cached_deps_order)
            
            # Build Docker image
            build_image_name = f"{self.container_name}_build:latest"
            print(f"Building Docker image: {build_image_name}")
            
            subprocess.run(sudo_prefix() + [
                "docker", "build", 
                "-t", build_image_name,
                "-f", str(self.dockerfile_path),
                str(self.project_dir)
            ], check=True)
            
            print(f"Build completed successfully. Image: {build_image_name}")
        else:
            print("No dependency changes detected, skipping rebuild.")
            build_image_name = f"{self.container_name}_build:latest"
            
            # Check if image exists
            try:
                subprocess.run(sudo_prefix() + [
                    "docker", "image", "inspect", build_image_name
                ], check=True, capture_output=True)
                print(f"Using existing image: {build_image_name}")
            except subprocess.CalledProcessError:
                print("Image not found, forcing rebuild...")
                # Generate and build Dockerfile
                dockerfile_content = self.generate_dockerfile(current_deps, cached_deps_order, invalidation_step)
                with open(self.dockerfile_path, 'w') as f:
                    f.write(dockerfile_content)
                
                # Save cache when generating Dockerfile
                self.save_cache(current_deps, cached_deps_order)
                
                subprocess.run(sudo_prefix() + [
                    "docker", "build", 
                    "-t", build_image_name,
                    "-f", str(self.dockerfile_path),
                    str(self.project_dir)
                ], check=True)
                
                print(f"Build completed. Image: {build_image_name}")


if __name__ == "__main__":
    main()