#!/usr/bin/env python3
"""
Print Cargo.toml package.version, optionally with a leading 'v'.

Usage:
  python3 scripts/tools/get_version.py [--file Cargo.toml] [--prefix-v]
"""

import argparse
import sys
from pathlib import Path

try:
    import tomllib  # Python 3.11+
except ModuleNotFoundError:  # pragma: no cover
    import tomli as tomllib  # type: ignore


def main(argv):
    p = argparse.ArgumentParser()
    p.add_argument('--file', default='Cargo.toml', help='Path to Cargo.toml')
    p.add_argument('--prefix-v', action='store_true', help="Prefix output with 'v'")
    args = p.parse_args(argv)

    cargo_toml = Path(args.file)
    if not cargo_toml.exists():
        print(f"File not found: {cargo_toml}", file=sys.stderr)
        return 2

    data = tomllib.loads(cargo_toml.read_text(encoding='utf-8'))
    version = data.get('package', {}).get('version')
    if not version:
        print("package.version not found in Cargo.toml", file=sys.stderr)
        return 2

    if args.prefix_v and not version.startswith('v'):
        version = f"v{version}"

    print(version)
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv[1:]))
