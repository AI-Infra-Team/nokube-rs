#!/usr/bin/env python3
"""
Setup script for nokube-rs project dependencies
Installs all required Python dependencies for build scripts and testing

Installation Commands:
    # Install core dependencies only (pyyaml, psutil)
    pip install -e .
    
    # Install with all development tools (docker, pytest, etc.)
    pip install -e ".[dev]"
    
    # Upgrade existing installation
    pip install -e ".[dev]" --upgrade
"""

from setuptools import setup
from pathlib import Path

# Read README for long description
readme_file = Path(__file__).parent / "README.md"
long_description = ""
if readme_file.exists():
    with open(readme_file, "r", encoding="utf-8") as f:
        long_description = f.read()

# Define project metadata
setup(
    name="nokube-rs-tools",
    version="0.1.0",
    author="AI-Infra-Team",
    description="Build tools and testing utilities for nokube-rs distributed service manager",
    long_description=long_description,
    long_description_content_type="text/markdown",
    python_requires=">=3.8",
    install_requires=[
        # Core dependencies for build and test scripts
        "pyyaml>=6.0",           # YAML configuration parsing
        "psutil>=5.8.0",         # System monitoring and process management
    ],
    extras_require={
        "dev": [
            "docker>=6.0.0",        # Docker API client
            "pytest>=7.0.0",        # Testing framework
            "pytest-asyncio>=0.21.0",  # Async testing support
            "click>=8.0.0",          # CLI framework
            "colorama>=0.4.4",       # Cross-platform colored terminal text
            "tqdm>=4.64.0",          # Progress bars
            "requests>=2.28.0",      # HTTP requests (for health checks)
            "paramiko>=2.11.0",      # SSH client (for remote operations)
            "black>=22.0.0",         # Code formatting
            "flake8>=5.0.0",         # Linting
            "isort>=5.10.0",         # Import sorting
            "mypy>=0.991",           # Type checking
        ],
    },
)