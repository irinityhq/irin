#!/bin/bash
# Council War Room — Frontend launcher (launchd)
# Runs production mode: serves pre-built .next/ on port 3000.
# Rebuild with `npm run build` after code changes.
set -euo pipefail
source "$(dirname "$0")/env.sh"

if ! node --version >/dev/null 2>&1; then
    echo "ERROR: node not found or broken. Check NVM in env.sh" >&2
    exit 1
fi

cd "$(cd "$(dirname "$0")/../web" && pwd)"

# Build if .next/BUILD_ID is missing or stale
if [ ! -f .next/BUILD_ID ]; then
    echo "No production build found — running npm run build..." >&2
    npm run build
fi

exec npm run start -- --hostname 127.0.0.1 --port 3000
