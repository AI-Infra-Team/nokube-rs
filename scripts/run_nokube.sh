#!/bin/bash
# Script to run nokube with proper library path

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Set the library path to include the directory containing the SSL libraries
export LD_LIBRARY_PATH="${SCRIPT_DIR}/../target:${LD_LIBRARY_PATH}"

# Run the nokube binary
"${SCRIPT_DIR}/../target/nokube" "$@"