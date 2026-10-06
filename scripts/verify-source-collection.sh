#!/bin/bash
# Verify repository source behavior and CSDL bindings before application packaging.
# This runs the same production checks as the repository CI test. It does not
# certify missing policies or compiled modules; use temper verify after packaging.
set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ "$#" -ne 1 ]; then
    echo "Usage: $0 SPECS_DIRECTORY" >&2
    exit 2
fi
SOURCE_DIR="$(cd "$1" && pwd)"
cd "$WORKSPACE_ROOT"
TEMPER_VERIFY_SOURCE_DIR="$SOURCE_DIR" cargo test --locked -p temper-cli --bin temper \
    verify::repository_tests::repository_source_collections_pass_behavior_verification \
    -- --exact --nocapture
