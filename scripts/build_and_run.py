import os
from pathlib import Path
import subprocess
import sys

SCRIPT_DIR = Path(__file__).absolute().parent

# 运行构建，如果失败立即退出
build_result = subprocess.run(["python3","build_with_container.py"], cwd=SCRIPT_DIR)
if build_result.returncode != 0:
    print("❌ Build failed! Terminating immediately.")
    sys.exit(build_result.returncode)

TARGET_DIR=Path(__file__).absolute().parent.parent / "target"

# export LD_LIBRARY_PATH=target:$LD_LIBRARY_PATH
if "LD_LIBRARY_PATH" not in os.environ:
    os.environ["LD_LIBRARY_PATH"] = str(TARGET_DIR)
else:
    os.environ["LD_LIBRARY_PATH"] = str(TARGET_DIR) + ":" + os.environ["LD_LIBRARY_PATH"]

print("✅ Build successful! Running nokube...")
args=os.sys.argv[1:]
run_result = subprocess.run([str(TARGET_DIR / "nokube")] + args, cwd=SCRIPT_DIR.parent)
sys.exit(run_result.returncode)