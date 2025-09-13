#!/usr/bin/env python3
"""
Nokube Applyer for GitOps

Provides a simple adapter that ensures a `nokube` binary is available
and applies Kubernetes YAML to a target NoKube cluster via the
`nokube apply` CLI.

Config schema (YAML under gitops-config.yaml -> applyer_nokube):
- cluster_name: string; target cluster name (required)
- nokube_download_url: string; URL to download `nokube` binary (required)
  (Binary always installed to absolute path: /pod-workspace/bin/nokube)
"""

from __future__ import annotations

import os
import stat
import shutil
import subprocess
from pathlib import Path
from typing import Dict, Optional, Tuple
import requests


class NokubeApplyer:
    def __init__(self, cfg: Dict):
        self.cfg = cfg or {}
        self.cluster_name: str = self._required("cluster_name")
        self.download_url: str = self._required("nokube_download_url")
        # Fixed absolute install path inside writable workspace
        self.binary_path: str = "/pod-workspace/bin/nokube"

    # ---------- public API ----------
    def install(self) -> Tuple[bool, str]:
        """Ensure nokube binary is available locally.

        - If binary exists at /pod-workspace/bin/nokube and is executable, keep it.
        - Otherwise, download from `self.download_url` and install to that path.
        - Performs a light sanity check that the nokube binary is invocable.
        Returns (ok, message).
        """

        try:
            bin_path = Path(self.binary_path)

            if bin_path.exists():
                # Ensure executable
                bin_path.chmod(bin_path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
            else:
                # Download binary
                bin_path.parent.mkdir(parents=True, exist_ok=True)
                resp = requests.get(self.download_url, timeout=60)
                resp.raise_for_status()
                with open(bin_path, "wb") as f:
                    f.write(resp.content)
                bin_path.chmod(0o755)

            # Quick nokube binary smoke test
            ok, out = self._check_nokube()
            if not ok:
                return False, out

            return True, f"nokube ready; binary: {self.binary_path}"
        except Exception as e:
            return False, f"install failed: {e}"

    def apply(self, yaml_content: str) -> Tuple[bool, str]:
        """Apply YAML to the configured NoKube cluster using `nokube apply`.
        Returns (ok, stdout_or_error).
        """
        if not yaml_content or not yaml_content.strip():
            return False, "empty YAML content"

        # Ensure nokube available
        ok, out = self._check_nokube()
        if not ok:
            return False, out

        try:
            proc = subprocess.run(
                [self.binary_path, "apply", "--cluster", self.cluster_name],
                input=yaml_content.encode("utf-8"),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            if proc.returncode != 0:
                return False, proc.stderr.decode("utf-8", errors="ignore")
            return True, proc.stdout.decode("utf-8", errors="ignore")
        except FileNotFoundError:
            return False, f"nokube binary not found: {self.binary_path}"
        except Exception as e:
            return False, f"apply failed: {e}"

    # ---------- helpers ----------
    def _required(self, key: str) -> str:
        val = self.cfg.get(key)
        if not val:
            raise ValueError(f"Missing required applyer_nokube config: {key}")
        return str(val)

    def _check_nokube(self) -> Tuple[bool, str]:
        try:
            proc = subprocess.run(
                [self.binary_path, "--help"],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            if proc.returncode != 0:
                return False, proc.stderr.decode("utf-8", errors="ignore")
            return True, proc.stdout.decode("utf-8", errors="ignore")
        except FileNotFoundError:
            return False, f"nokube binary not found: {self.binary_path}"
