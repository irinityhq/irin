#!/bin/bash
# IRIN — Backend launcher (launchd)
set -euo pipefail
source "$(dirname "$0")/env.sh"

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
export COUNCIL_SESSIONS_DIR="$REPO_ROOT/sessions"
export COUNCIL_RUNS_DIR="$REPO_ROOT/runs"

cd "$REPO_ROOT"
exec ./target/release/council --serve --port 8765
