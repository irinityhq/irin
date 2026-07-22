# bin/lib/fido2.sh — shared FIDO2 python resolution for the arm ceremonies.
# Sourced by bin/arm and bin/arm-enroll-fido2 (both cd to the repo root
# before sourcing). POSIX sh.
#
# Resolution order — first interpreter that actually imports fido2 wins:
#   1. FIDO2_PYTHON env override (validated too — a stale override fails
#      loudly here instead of at ceremony time)
#   2. python3 on PATH
#   3. pyenv interpreters under ~/.pyenv/versions (the historical default
#      on the arming Mac, where PATH python3 lacks the module)
# Prints the interpreter path on stdout; returns 1 with an install hint on
# stderr. Validation lives HERE, on every branch — callers must not re-check.
resolve_fido2_python() {
  if [ -n "${FIDO2_PYTHON:-}" ]; then
    if "$FIDO2_PYTHON" -c "import fido2" 2>/dev/null; then
      printf '%s\n' "$FIDO2_PYTHON"
      return 0
    fi
    echo "fido2 python module not found at FIDO2_PYTHON=$FIDO2_PYTHON" >&2
    echo "install: $FIDO2_PYTHON -m pip install fido2" >&2
    return 1
  fi
  if command -v python3 >/dev/null 2>&1 && python3 -c "import fido2" 2>/dev/null; then
    command -v python3
    return 0
  fi
  for _fido2_candidate in "$HOME"/.pyenv/versions/*/bin/python3; do
    if [ -x "$_fido2_candidate" ] && "$_fido2_candidate" -c "import fido2" 2>/dev/null; then
      printf '%s\n' "$_fido2_candidate"
      return 0
    fi
  done
  echo "no python3 with the fido2 module found (probed PATH python3 + ~/.pyenv/versions)" >&2
  echo "set FIDO2_PYTHON to a python3 with fido2 installed (pip install fido2)" >&2
  return 1
}
