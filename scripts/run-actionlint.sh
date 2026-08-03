#!/usr/bin/env bash
# Run the repo-pinned actionlint with IRIN-known schema allowances.
#
# GitHub Actions supports concurrency.queue:max (2026-05) so main can retain
# every merge receipt. Pinned actionlint 1.7.12 predates that key and rejects
# it as an unexpected concurrency field. Ignore only that one schema gap;
# every other finding still fails the run.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

bash scripts/bootstrap-actionlint.sh

# Match actionlint's "unexpected key \"queue\" for \"concurrency\" section" only.
exec .irin-tools/bin/actionlint -color \
  -ignore 'unexpected key "queue" for "concurrency" section' \
  "$@"
