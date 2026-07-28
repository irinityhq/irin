#!/usr/bin/env bash
# Run Selene on gateway OpenResty Lua (advisory by default).
#
# Default paths: gateway/lua
# Config:        selene.toml (repo root)
# Std:           security/selene/std/openresty.yml
#
# Exit policy:
#   - advisory (default): exit 0 even when findings exist
#   - IRIN_SELENE_FAIL=1 or --fail: nonzero when findings
#   - missing binary: skip with bootstrap hint, exit 0
#
# Usage:
#   scripts/run-selene.sh
#   scripts/run-selene.sh gateway/lua/router.lua
#   scripts/run-selene.sh --fail gateway/lua
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  ROOT="$(cd "$(dirname "$0")/.." && pwd)"
}
cd "$ROOT"

BIN="$ROOT/.irin-tools/bin/selene"
CONFIG="$ROOT/selene.toml"

DEFAULT_PATHS=(
  gateway/lua
)

fail_hard=0
if [[ "${IRIN_SELENE_FAIL:-0}" == "1" ]]; then
  fail_hard=1
fi

paths=()

usage() {
  cat <<'EOF'
Usage: scripts/run-selene.sh [--fail] [PATH...]

  --fail   Nonzero exit on findings (or set IRIN_SELENE_FAIL=1)
  PATH...  Lint targets (default: gateway/lua)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h | --help)
      usage
      exit 0
      ;;
    --fail)
      fail_hard=1
      shift
      ;;
    --)
      shift
      paths+=("$@")
      break
      ;;
    -*)
      printf 'run-selene: unknown option: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
    *)
      paths+=("$1")
      shift
      ;;
  esac
done

if [[ ! -x "$BIN" ]]; then
  printf 'run-selene: selene missing at %s; install with: make tools\n' "$BIN" >&2
  # Advisory skip when the pin is not bootstrapped yet.
  exit 0
fi

if [[ ! -f "$CONFIG" ]]; then
  printf 'run-selene: ERROR: config missing: %s\n' "$CONFIG" >&2
  exit 1
fi

if [[ ${#paths[@]} -eq 0 ]]; then
  paths=("${DEFAULT_PATHS[@]}")
fi

scan_paths=()
for p in "${paths[@]}"; do
  if [[ -e "$p" ]]; then
    scan_paths+=("$p")
  else
    printf 'run-selene: skip missing path: %s\n' "$p" >&2
  fi
done
if [[ ${#scan_paths[@]} -eq 0 ]]; then
  printf 'run-selene: ERROR: no lint paths exist\n' >&2
  exit 1
fi

cmd=(
  "$BIN"
  --config "$CONFIG"
  "${scan_paths[@]}"
)

printf 'run-selene: %s\n' "${cmd[*]}"
set +e
"${cmd[@]}"
rc=$?
set -e

# Selene: 0 clean, 1 findings, other = tool/config error.
if [[ "$fail_hard" == "1" ]]; then
  exit "$rc"
fi

if [[ "$rc" -ge 2 ]]; then
  printf 'run-selene: tool/config error (rc=%s)\n' "$rc" >&2
  exit "$rc"
fi

if [[ "$rc" -eq 1 ]]; then
  printf 'run-selene: findings present (advisory; IRIN_SELENE_FAIL=1 or --fail to gate)\n' >&2
fi
exit 0
