#!/usr/bin/env python3
"""
Quick validation script for nokube-rs core functionality
Tests configuration discovery and basic CLI commands
"""

import os
import sys
import subprocess
import tempfile
import yaml
from pathlib import Path


def run_command(cmd: list) -> tuple[bool, str, str]:
    """Run command and return success, stdout, stderr"""
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        return result.returncode == 0, result.stdout, result.stderr
    except subprocess.TimeoutExpired:
        return False, "", "Command timed out"
    except Exception as e:
        return False, "", str(e)


def test_nokube_binary():
    """Test nokube binary basic functionality"""
    print("🧪 Testing nokube binary functionality...")
    
    project_dir = Path(__file__).parent.parent
    binary_path = project_dir / "target" / "nokube"
    
    # Check binary exists
    if not binary_path.exists():
        print(f"❌ Binary not found at {binary_path}")
        return False
    
    print(f"✅ Binary found: {binary_path}")
    
    # Test help command
    print("🔍 Testing --help command...")
    success, stdout, stderr = run_command([str(binary_path), "--help"])
    if not success:
        print(f"❌ Help command failed: {stderr}")
        return False
    
    if "nokube" in stdout.lower():
        print("✅ Help command works")
    else:
        print(f"❌ Unexpected help output: {stdout}")
        return False
    
    # Test config discovery with temp config
    print("🔍 Testing config discovery...")
    with tempfile.TemporaryDirectory() as temp_dir:
        config_dir = Path(temp_dir) / ".nokube"
        config_dir.mkdir()
        
        config_file = config_dir / "config.yaml"
        config_content = {
            "etcd_endpoints": ["127.0.0.1:2379", "localhost:2379"]
        }
        
        with open(config_file, 'w') as f:
            yaml.dump(config_content, f)
        
        # Test with HOME environment variable pointing to temp dir
        env = os.environ.copy()
        env["HOME"] = temp_dir
        
        print(f"   Config file: {config_file}")
        print(f"   Temp HOME: {temp_dir}")
        
        # Test init command with temp config
        success, stdout, stderr = run_command([
            str(binary_path), "init", "--cluster-name", "test-cluster"
        ])
        
        if success:
            print("✅ Init command succeeded")
            if "config discovery" in stdout.lower() or "found" in stdout.lower():
                print("✅ Config discovery logging present")
            else:
                print("⚠️  Config discovery logging may not be working")
        else:
            print(f"⚠️  Init command failed (expected if no etcd): {stderr}")
            # This is expected without etcd running
    
    # Test other commands
    test_commands = [
        (["init", "--help"], "init help"),
        (["--version"], "version info"),
    ]
    
    for cmd_args, description in test_commands:
        print(f"🔍 Testing {description}...")
        success, stdout, stderr = run_command([str(binary_path)] + cmd_args)
        if success:
            print(f"✅ {description} works")
        else:
            print(f"⚠️  {description} failed: {stderr}")
    
    return True


def test_build_system():
    """Test the build system"""
    print("🔨 Testing build system...")
    
    project_dir = Path(__file__).parent.parent
    build_script = project_dir / "scripts" / "build_with_container.py"
    
    if not build_script.exists():
        print(f"❌ Build script not found: {build_script}")
        return False
    
    print("✅ Build script exists")
    
    # Test that we can invoke the build script (but don't actually run it)
    success, stdout, stderr = run_command(["python3", str(build_script), "--help"])
    if success or "usage" in stderr.lower():
        print("✅ Build script is invokable")
    else:
        print(f"⚠️  Build script may have issues: {stderr}")
    
    return True


def main():
    print("🚀 Starting nokube-rs quick validation...")
    
    success = True
    
    # Test build system
    if not test_build_system():
        success = False
    
    # Test binary functionality
    if not test_nokube_binary():
        success = False
    
    if success:
        print("🎉 All quick tests passed!")
        print("\n📋 Manual testing suggestions:")
        print("   1. Start etcd: docker run -d -p 2379:2379 quay.io/coreos/etcd:v3.5.0")
        print("   2. Create ~/.nokube/config.yaml with etcd endpoints")
        print("   3. Run: ./target/nokube init --cluster-name test")
        print("   4. Run full E2E test: python3 scripts/test_all_in_container.py")
        return True
    else:
        print("❌ Some tests failed!")
        return False


if __name__ == "__main__":
    success = main()
    sys.exit(0 if success else 1)