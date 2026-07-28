#!/usr/bin/env bash
# Run IRIN Opengrep rules on critical product-security surfaces (Phase 1A).
#
# Default paths: gateway/sidecar-rs/src, sentinel/sovereign-protocol/src,
#                council-rs/src, gateway/lua
# Rules:         security/opengrep/rules/
# Artifacts:     .irin-tools/findings/opengrep-<ts>.{json,sarif} (gitignored)
#
# Exit policy:
#   - advisory (default): exit 0 even when findings exist
#   - IRIN_OPENGREP_FAIL=1 or --fail: nonzero when findings or scan errors
#
# Usage:
#   scripts/run-opengrep.sh
#   scripts/run-opengrep.sh gateway/sidecar-rs/src/watch/api
#   scripts/run-opengrep.sh --fail --sarif /tmp/out.sarif path...
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  ROOT="$(cd "$(dirname "$0")/.." && pwd)"
}
cd "$ROOT"

BIN="$ROOT/.irin-tools/bin/opengrep"
RULES="$ROOT/security/opengrep/rules"
FINDINGS_DIR="$ROOT/.irin-tools/findings"
BOOTSTRAP="$ROOT/scripts/bootstrap-dev-tools.sh"

DEFAULT_PATHS=(
  gateway/sidecar-rs/src
  sentinel/sovereign-protocol/src
  council-rs/src
  gateway/lua
)

fail_hard=0
if [[ "${IRIN_OPENGREP_FAIL:-0}" == "1" ]]; then
  fail_hard=1
fi

sarif_override=""
paths=()

usage() {
  cat <<'EOF'
Usage: scripts/run-opengrep.sh [--fail] [--sarif PATH] [PATH...]

  --fail         Nonzero exit on findings (or set IRIN_OPENGREP_FAIL=1)
  --sarif PATH   Write SARIF to PATH (JSON still under .irin-tools/findings/)
  PATH...        Scan targets (default: critical gateway/council/sentinel paths)
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
    --sarif)
      [[ $# -ge 2 ]] || {
        printf 'run-opengrep: --sarif requires a path\n' >&2
        exit 2
      }
      sarif_override="$2"
      shift 2
      ;;
    --)
      shift
      paths+=("$@")
      break
      ;;
    -*)
      printf 'run-opengrep: unknown option: %s\n' "$1" >&2
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
  printf 'run-opengrep: opengrep missing; bootstrapping tools\n' >&2
  bash "$BOOTSTRAP"
fi
[[ -x "$BIN" ]] || {
  printf 'run-opengrep: ERROR: opengrep not installed at %s\n' "$BIN" >&2
  exit 1
}
[[ -d "$RULES" ]] || {
  printf 'run-opengrep: ERROR: rules dir missing: %s\n' "$RULES" >&2
  exit 1
}

if [[ ${#paths[@]} -eq 0 ]]; then
  paths=("${DEFAULT_PATHS[@]}")
fi

scan_paths=()
for p in "${paths[@]}"; do
  if [[ -e "$p" ]]; then
    scan_paths+=("$p")
  else
    printf 'run-opengrep: skip missing path: %s\n' "$p" >&2
  fi
done
if [[ ${#scan_paths[@]} -eq 0 ]]; then
  printf 'run-opengrep: ERROR: no scan paths exist\n' >&2
  exit 1
fi

mkdir -p "$FINDINGS_DIR"
ts="$(date -u +%Y%m%dT%H%M%SZ)"
json_out="$FINDINGS_DIR/opengrep-${ts}.json"
if [[ -n "$sarif_override" ]]; then
  sarif_out="$sarif_override"
  mkdir -p "$(dirname "$sarif_out")"
else
  sarif_out="$FINDINGS_DIR/opengrep-${ts}.sarif"
fi

# Symlink "latest" pointers for agent/CI convenience (still gitignored).
latest_json="$FINDINGS_DIR/opengrep-latest.json"
latest_sarif="$FINDINGS_DIR/opengrep-latest.sarif"

cmd=(
  "$BIN" scan
  --config "$RULES"
  --disable-version-check
  --json-output "$json_out"
  --sarif-output "$sarif_out"
  --exclude '**/target/**'
  --exclude '**/node_modules/**'
  --exclude '**/.irin-tools/**'
)
if [[ "$fail_hard" == "1" ]]; then
  cmd+=(--error)
fi
cmd+=("${scan_paths[@]}")

printf 'run-opengrep: %s\n' "${cmd[*]}"
set +e
"${cmd[@]}"
rc=$?
set -e

# Opengrep: 0 clean, 1 findings (with --error), other = tool/config error.
if [[ -f "$json_out" ]]; then
  ln -sfn "$(basename "$json_out")" "$latest_json"
fi
if [[ -f "$sarif_out" ]]; then
  # latest_sarif always under findings; custom --sarif may be outside.
  if [[ "$sarif_out" == "$FINDINGS_DIR/"* ]]; then
    ln -sfn "$(basename "$sarif_out")" "$latest_sarif"
  else
    ln -sfn "$sarif_out" "$latest_sarif"
  fi
fi

findings=0
if [[ -f "$json_out" ]] && command -v python3 >/dev/null 2>&1; then
  findings="$(python3 - "$json_out" <<'PY'
import json, sys
path = sys.argv[1]
try:
    with open(path) as f:
        data = json.load(f)
    print(len(data.get("results") or []))
except Exception:
    print(0)
PY
)"
  printf 'run-opengrep: findings=%s json=%s sarif=%s\n' "$findings" "$json_out" "$sarif_out"
else
  printf 'run-opengrep: json=%s sarif=%s (rc=%s)\n' "$json_out" "$sarif_out" "$rc"
fi

if [[ "$fail_hard" == "1" ]]; then
  exit "$rc"
fi

# Advisory: tool/config failure still surfaces nonzero (rule broken ≠ product finding).
if [[ "$rc" -ge 2 ]]; then
  printf 'run-opengrep: tool/config error (rc=%s)\n' "$rc" >&2
  exit "$rc"
fi
exit 0
