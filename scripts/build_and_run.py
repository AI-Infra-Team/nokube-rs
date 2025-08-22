import os
from pathlib import Path
import subprocess

TARGET_DIR=Path(__file__).absolute().parent.parent / "target"

# export LD_LIBRARY_PATH=target:$LD_LIBRARY_PATH
if "LD_LIBRARY_PATH" not in os.environ:
    os.environ["LD_LIBRARY_PATH"] = str(TARGET_DIR)
else:
    os.environ["LD_LIBRARY_PATH"] = str(TARGET_DIR) + ":" + os.environ["LD_LIBRARY_PATH"]



args=os.sys.argv[1:]
subprocess.run([str(TARGET_DIR / "nokube")] + args)