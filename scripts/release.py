#!/usr/bin/env python3
"""
Release helper: create/update a GitHub Release and upload assets.

Usage (in CI or locally):
  python3 scripts/release.py --tag v0.1.0 \
    --asset dist/nokube.run \
    --asset target/release/nokube \
    [--notes-file RELEASE_NOTES.md] [--name "nokube v0.1.0"] [--draft] [--prerelease]

Env vars required in CI:
  - GITHUB_TOKEN: GitHub token with repo access (Actions provides this automatically)
  - GITHUB_REPOSITORY: "owner/repo"

Notes:
  - Computes SHA256 for each asset and appends to the release body.
  - If the release already exists, it is reused; assets with the same name are replaced.
"""

import argparse
import hashlib
import json
import os
import sys
from pathlib import Path
from typing import List, Optional

import requests
try:
    import tomllib  # Python 3.11+
except ModuleNotFoundError:  # pragma: no cover
    import tomli as tomllib  # type: ignore


API_BASE = "https://api.github.com"


def sha256sum(path: Path) -> str:
    h = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def get_env(name: str, required: bool = True, default: Optional[str] = None) -> str:
    val = os.environ.get(name, default)
    if required and not val:
        print(f"Missing required env var: {name}", file=sys.stderr)
        sys.exit(2)
    return val or ""


def gh_headers(token: str) -> dict:
    return {
        "Authorization": f"Bearer {token}",
        "Accept": "application/vnd.github+json",
        "User-Agent": "nokube-release-script",
    }


def find_or_create_release(repo: str, token: str, tag: str, name: str, body: str, draft: bool, prerelease: bool) -> dict:
    s = requests.Session()
    s.headers.update(gh_headers(token))

    # Try to find existing
    r = s.get(f"{API_BASE}/repos/{repo}/releases/tags/{tag}")
    if r.status_code == 200:
        return r.json()
    elif r.status_code != 404:
        print(f"Error checking release: {r.status_code} {r.text}", file=sys.stderr)
        sys.exit(1)

    # Create new
    payload = {
        "tag_name": tag,
        "name": name,
        "body": body,
        "draft": draft,
        "prerelease": prerelease,
    }
    r = s.post(f"{API_BASE}/repos/{repo}/releases", json=payload)
    if r.status_code in (201, 200):
        return r.json()
    # Handle race where release gets created in parallel
    if r.status_code == 422:
        r2 = s.get(f"{API_BASE}/repos/{repo}/releases/tags/{tag}")
        if r2.status_code == 200:
            return r2.json()
    print(f"Error creating release: {r.status_code} {r.text}", file=sys.stderr)
    sys.exit(1)


def delete_asset_if_exists(repo: str, token: str, release_id: int, asset_name: str) -> None:
    s = requests.Session()
    s.headers.update(gh_headers(token))
    r = s.get(f"{API_BASE}/repos/{repo}/releases/{release_id}/assets")
    if r.status_code != 200:
        print(f"Warning: cannot list assets: {r.status_code} {r.text}", file=sys.stderr)
        return
    for asset in r.json():
        if asset.get("name") == asset_name:
            asset_id = asset.get("id")
            dr = s.delete(f"{API_BASE}/repos/{repo}/releases/assets/{asset_id}")
            if dr.status_code not in (204, 200):
                print(f"Warning: failed to delete asset {asset_name}: {dr.status_code} {dr.text}", file=sys.stderr)
            else:
                print(f"Deleted existing asset: {asset_name}")
            return


def upload_asset(upload_url: str, token: str, asset_path: Path) -> None:
    # upload_url template: .../assets{?name,label}
    url = upload_url.split('{', 1)[0]
    params = {"name": asset_path.name}
    headers = gh_headers(token)
    headers["Content-Type"] = "application/octet-stream"
    with asset_path.open('rb') as f:
        r = requests.post(url, headers=headers, params=params, data=f.read())
    if r.status_code not in (201, 200):
        print(f"Error uploading asset {asset_path.name}: {r.status_code} {r.text}", file=sys.stderr)
        sys.exit(1)
    print(f"Uploaded asset: {asset_path.name}")


def main(argv: List[str]) -> int:
    p = argparse.ArgumentParser(description="Create/update GitHub release and upload assets")
    p.add_argument("--tag", required=False, help="Release tag, e.g., v0.1.0; defaults to Cargo.toml version if unset")
    p.add_argument("--name", required=False, help="Release name; defaults to tag")
    p.add_argument("--asset", action="append", default=[], help="Path to asset to upload (repeatable)")
    p.add_argument("--notes-file", required=False, help="Path to Markdown release notes file")
    p.add_argument("--draft", action="store_true", help="Create as draft release")
    p.add_argument("--prerelease", action="store_true", help="Mark as prerelease")
    p.add_argument("--cargo-file", default="Cargo.toml", help="Path to Cargo.toml for version fallback")
    p.add_argument("--tag-prefix", default="v", help="Prefix to apply to version when deriving tag (default: v)")
    args = p.parse_args(argv)

    repo = get_env("GITHUB_REPOSITORY")
    token = get_env("GITHUB_TOKEN")
    tag = args.tag or os.environ.get("GITHUB_REF_NAME")
    if not tag:
        # Fallback to Cargo.toml version
        cargo_path = Path(args.cargo_file)
        if not cargo_path.exists():
            print(f"--tag not provided and {cargo_path} not found", file=sys.stderr)
            return 2
        try:
            data = tomllib.loads(cargo_path.read_text(encoding="utf-8"))
            version = data.get("package", {}).get("version")
        except Exception as e:
            print(f"Failed to parse {cargo_path}: {e}", file=sys.stderr)
            return 2
        if not version:
            print("package.version not found in Cargo.toml", file=sys.stderr)
            return 2
        prefix = args.tag_prefix
        # Ensure we only prefix if not present
        tag = version if (prefix and version.startswith(prefix)) else f"{prefix}{version}" if prefix else version

    name = args.name or tag

    # Load notes if provided
    body_parts: List[str] = []
    if args.notes_file:
        notes_path = Path(args.notes_file)
        if notes_path.exists():
            body_parts.append(notes_path.read_text(encoding="utf-8"))
        else:
            print(f"Warning: notes file not found: {notes_path}", file=sys.stderr)

    # Validate assets
    assets: List[Path] = []
    for a in args.asset:
        pth = Path(a)
        if not pth.exists():
            print(f"Asset not found: {pth}", file=sys.stderr)
            return 2
        assets.append(pth)

    # Compute checksums
    if assets:
        body_parts.append("SHA256SUMS:\n")
        body_parts.append("```")
        for a in assets:
            body_parts.append(f"{sha256sum(a)}  {a.name}")
        body_parts.append("```")

    body = "\n\n".join(body_parts).strip()

    release = find_or_create_release(repo, token, tag, name, body, args.draft, args.prerelease)
    upload_url = release.get("upload_url")
    release_id = release.get("id")
    if not upload_url or not release_id:
        print("Invalid release response: missing upload_url or id", file=sys.stderr)
        return 1

    for a in assets:
        delete_asset_if_exists(repo, token, release_id, a.name)
        upload_asset(upload_url, token, a)

    print(f"Release ready: https://github.com/{repo}/releases/tag/{tag}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
