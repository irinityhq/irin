#!/usr/bin/env bash
# W5 shipping-method rehearsal smoke.
#
# Runs hermetic refuse contracts + fake-gh publication path. Zero provider,
# zero Apple, zero network. Does not run --prepare-production (live RC path).
# Does not manufacture Accepted via the interactive recorder happy path.
#
# Coverage (plan W5 refuse list):
#   1–3,5–7,11–12  unit hermetics (store / status / release-tx / export-import)
#   4              remove-worktree harvest + incomplete refuse
#   8              ship-board vitest (durable home via make link-ship-board)
#   9              classifier exact_candidate / docs-light
#  10              fake-gh publication (asset refuse / equal / public retry)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
section() { printf '\n======== %s ========\n' "$*"; }

run() {
  local name="$1"
  shift
  section "$name"
  "$@"
  pass "$name"
}

# Guard: never invoke live prepare / Apple under this smoke.
if printf '%s' "$*" | grep -Eq 'prepare-production|notary|stapler'; then
  fail "shipping-method-smoke refuses Apple/prepare arguments"
fi

# Static guards BEFORE any child run: fail closed on live prepare/Apple/clobber
# invocations in the driver + W5 children. Comments, prose (fail/pass/printf/...),
# meta-greps, and re.compile pattern lines are ignored.
section "W5 smoke static guards (pre-run)"
grep -q 'test-publish-fake-gh' "$0" || fail "smoke must drive fake-gh publish test"
grep -q 'test-remove-worktree-evidence' "$0" || fail "smoke must drive harvest test"
python3 - "$ROOT" <<'PY' || fail "static guards found live Apple/prepare/clobber invocation"
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
files = [
    root / "scripts/shipping-method-smoke.sh",
    root / "scripts/test-publish-fake-gh.sh",
    root / "scripts/test-remove-worktree-evidence.sh",
]
# Command-shaped live effects (not mere mentions in strings/greps/patterns).
_b = r"(?:^|[;&|`(\n]|\s)"
forbidden = [
    re.compile(_b + r"(?:bash\s+)?(?:\S*/)?release-transaction\.sh\s+--prepare-production\b"),
    re.compile(_b + r"(?:bash\s+)?(?:\"[^\"]*\"|'[^']*'|\$[A-Za-z_][A-Za-z0-9_]*)\s+--prepare-production\b"),
    re.compile(_b + r"(?:\S*/)?notarytool\b"),
    re.compile(_b + r"xcrun\s+stapler\b"),
    re.compile(_b + r"gh\s+release\s+upload\s+\S+[^\n]*--clobber\b"),
]
driver_tx = re.compile(_b + r"(?:bash\s+)?(?:\S*/)?release-transaction\.sh\b")
prose = re.compile(r"^\s*(fail|pass|printf|section|echo|note)\b")
meta = re.compile(r"\b(?:grep|re\.compile)\b")

bad = []
for path in files:
    for i, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        code = line.split("#", 1)[0]
        if not code.strip() or prose.match(code) or meta.search(code):
            continue
        for pat in forbidden:
            if pat.search(code):
                bad.append(f"{path.name}:{i}: {line.strip()}")
                break
        else:
            if path.name == "shipping-method-smoke.sh" and driver_tx.search(code):
                bad.append(
                    f"{path.name}:{i}: driver must not call release-transaction: {line.strip()}"
                )

if bad:
    print("static guard violations:", file=sys.stderr)
    for b in bad:
        print(f"  {b}", file=sys.stderr)
    raise SystemExit(1)
print("static guards: no live prepare/Apple/clobber invocations in W5 driver+children")
PY
pass "static guards (pre-run; no Apple/prepare/clobber in driver+children)"

# --- unit hermetics already on tip (W1–W4) ---------------------------------
run "W1 candidate-store contracts" \
  bash packaging/test-candidate-store.sh

run "W2 candidate-status contracts" \
  bash scripts/test-candidate-status.sh

run "W3 prepare/publish/install/acceptance contracts" \
  bash scripts/test-release-transaction-w3.sh

run "W4 export/import contracts" \
  bash scripts/test-export-import-candidate.sh

run "W4 classifier (PR isolation / merged-SHA / docs-light)" \
  bash scripts/test-classify-ci-paths.sh

# --- W5 new hermetics ------------------------------------------------------
run "W5 remove-worktree harvest + incomplete refuse" \
  bash scripts/test-remove-worktree-evidence.sh

run "W5 fake-gh publication path" \
  bash scripts/test-publish-fake-gh.sh

# --- board: durable home via make link-ship-board --------------------------
section "W5 ship-board adapter + domain contracts"
if [[ -L "$ROOT/tools/ship-board" || -d "$ROOT/tools/ship-board" ]]; then
  if [[ ! -d "$ROOT/tools/ship-board/node_modules" ]]; then
    fail "tools/ship-board present but node_modules missing (run npm install in durable home)"
  fi
  # Board must not be a tracked real tree; link-ship-board is the durable route.
  if [[ -L "$ROOT/tools/ship-board" ]]; then
    target="$(readlink "$ROOT/tools/ship-board")"
    printf 'ship-board link → %s\n' "$target"
  else
    printf 'WARNING: tools/ship-board is a real directory (legacy); prefer make link-ship-board\n' >&2
  fi
  (cd "$ROOT/tools/ship-board" && npm test)
  pass "ship-board vitest"
else
  fail "tools/ship-board missing — run: make link-ship-board"
fi

printf '\n======== shipping-method-smoke: ALL PASSED ========\n'
printf 'Boundary: hermetic method rehearsal only. Not product green.\n'
printf 'Not exercised: live --prepare-production, Apple notary, real GH/network.\n'
