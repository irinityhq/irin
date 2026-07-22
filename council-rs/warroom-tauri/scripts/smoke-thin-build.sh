#!/bin/bash
set -euo pipefail

# Phase 1 thin Vite smoke is retired; delegate to hybrid smoke.
echo "NOTE: Phase 2 — thin Vite smoke replaced by smoke-hybrid-build.sh"
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/smoke-hybrid-build.sh"