#!/usr/bin/env bash
# Run the repo-pinned actionlint (bootstrap installs a wrapper that ignores the
# pre-queue schema gap for concurrency.queue only).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

bash scripts/bootstrap-actionlint.sh
exec .irin-tools/bin/actionlint -color "$@"
