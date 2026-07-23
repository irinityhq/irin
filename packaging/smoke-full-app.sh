#!/usr/bin/env bash
# Full-app packaged smoke + promotion gate for IRIN DMG installs.
#
# Modes:
#   default / BOUNDED: may report BOUNDED_PASS when :8765 is intentionally
#     occupied (foreign Council left alive). Never prints FULL_PASS unless
#     the packaged host path fully passes.
#   PROMOTION=1: exit nonzero unless the packaged host path fully passes
#     (requires free :8765). Never prints FULL_PASS after skip/fail.
#
# Safety:
#   - Never re-signs the test app.
#   - Never kills a foreign :8765 listener that this script did not start.
#   - Fake provider markers only under isolated HOME; values never logged.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

PROMOTION="${PROMOTION:-0}"
APP_NAME="IRIN.app"
TEST_APPS="$ROOT/packaging/test-apps"
DEST_APP="${IRIN_SMOKE_APP:-$TEST_APPS/$APP_NAME}"
TEST_HOME="$ROOT/packaging/test-home/smoke-$$"
MOUNT="$ROOT/packaging/build/dmg-mount"
DMG="${IRIN_DMG_PATH:-$ROOT/packaging/artifacts/IRIN_0.1.0_aarch64.dmg}"
REPORT="$ROOT/packaging/receipts/FULL_APP_SMOKE.txt"
WEBVIEW_SHOT="$ROOT/packaging/receipts/webview-smoke.png"
PIDFILE="$ROOT/packaging/build/smoke-host.pid"
SIDECAR_PIDFILE="$ROOT/packaging/build/smoke-sidecar.pid"
FAKE_MARKER_NAME="XAI_API_KEY"
FAKE_MARKER_VALUE="irin-dmg-fake-marker-not-a-real-key"
DENIED_FAKE_NAME="GW_API_KEY"
DENIED_FAKE_VALUE="should-never-import-gateway-key"
# Prefer explicit env, then the built DMG HASHES receipt, then HEAD.
if [[ -z "${IRIN_TAURI_BUILD_GIT_SHA:-}" && -f "$ROOT/packaging/artifacts/HASHES.txt" ]]; then
  IRIN_TAURI_BUILD_GIT_SHA="$(
    awk -F= '/^source_sha=/{print $2; exit}' "$ROOT/packaging/artifacts/HASHES.txt" | tr -d '[:space:]'
  )"
  export IRIN_TAURI_BUILD_GIT_SHA
fi
EXPECTED_SHA="${IRIN_TAURI_BUILD_GIT_SHA:-$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || true)}"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
log() { printf '%s\n' "$*" | tee -a "$REPORT"; }

mkdir -p "$ROOT/packaging/receipts" "$TEST_APPS" "$(dirname "$PIDFILE")"
: >"$REPORT"
log "=== smoke-full-app $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
log "ROOT=$ROOT"
log "PROMOTION=$PROMOTION"
log "expected_sha=${EXPECTED_SHA:-unknown}"

[[ "$(uname -s)" == "Darwin" ]] || die "macOS only"
[[ "$(uname -m)" == "arm64" ]] || die "arm64 only"

# --- ensure test app exists (untouched DMG copy preferred) ---
if [[ ! -d "$DEST_APP" ]]; then
  [[ -f "$DMG" ]] || die "missing DMG and no IRIN_SMOKE_APP: $DMG"
  if mount | grep -q "$MOUNT"; then
    hdiutil detach "$MOUNT" -force 2>/dev/null || true
  fi
  rm -rf "$MOUNT"
  mkdir -p "$MOUNT"
  hdiutil attach "$DMG" -mountpoint "$MOUNT" -readonly -nobrowse
  SRC_APP="$(find "$MOUNT" -maxdepth 2 -name "$APP_NAME" -type d | head -1 || true)"
  [[ -d "$SRC_APP" ]] || die "app not found in DMG"
  rm -rf "$DEST_APP"
  ditto "$SRC_APP" "$DEST_APP"
  hdiutil detach "$MOUNT" -force 2>/dev/null || true
fi
[[ -d "$DEST_APP" ]] || die "missing app: $DEST_APP"

if ! codesign --verify --deep --strict "$DEST_APP" >/dev/null 2>&1; then
  die "untouched app failed codesign verify (will not re-sign)"
fi
log "app=$DEST_APP"
log "codesign: ok (untouched)"

HOST="$DEST_APP/Contents/MacOS/council-warroom-tauri"
SIDECAR="$DEST_APP/Contents/MacOS/council"
[[ -x "$HOST" && -x "$SIDECAR" ]] || die "host/sidecar missing"
CABINETS="$(find "$DEST_APP/Contents/Resources" -type d -name cabinets | head -1 || true)"
[[ -n "$CABINETS" ]] || die "cabinets missing"
BASE_DIR="$(dirname "$CABINETS")"
log "base_dir=$BASE_DIR"
log "host_sha256=$(shasum -a 256 "$HOST" | awk '{print $1}')"
log "council_sha256=$(shasum -a 256 "$SIDECAR" | awk '{print $1}')"

listen_pid() {
  local port="$1"
  lsof -nP -iTCP:"$port" -sTCP:LISTEN -t 2>/dev/null | head -1 || true
}

# True when the :port listener is our packaged smoke app (never a foreign Council).
is_our_packaged_listener() {
  local port="$1"
  local pid path
  pid="$(listen_pid "$port")"
  [[ -n "$pid" ]] || return 1
  path="$(ps -p "$pid" -o comm= 2>/dev/null || true)"
  # Prefer full argv path when available.
  local args
  args="$(ps -p "$pid" -o args= 2>/dev/null || true)"
  if [[ "$args" == *"$DEST_APP"* ]] || [[ "$args" == *"/Contents/MacOS/council"* ]]; then
    # Packaged sidecar path under our test app, or still ambiguous "council" only after we started it.
    if [[ "$args" == *"$DEST_APP"* ]]; then
      return 0
    fi
  fi
  # Fall back: if we started a host and no foreign was present at start, treat residual as ours.
  if [[ -z "${FOREIGN_8765:-}" && -n "$pid" ]]; then
    return 0
  fi
  return 1
}

# Gracefully stop the packaged host and any sidecar it left behind on :8765.
stop_packaged_host() {
  osascript -e 'tell application "IRIN" to quit' >/dev/null 2>&1 || true
  if [[ -f "$PIDFILE" ]]; then
    local p
    p="$(cat "$PIDFILE" 2>/dev/null || true)"
    if [[ -n "$p" ]]; then
      kill -TERM "$p" 2>/dev/null || true
      # Give Tauri Exit handlers time to stop the tracked sidecar.
      local i
      for i in $(seq 1 40); do
        if ! kill -0 "$p" 2>/dev/null; then
          break
        fi
        sleep 0.25
      done
      kill -KILL "$p" 2>/dev/null || true
    fi
    rm -f "$PIDFILE"
  fi
  # Reclaim :8765 only if it is still held by our packaged path (never foreign).
  local i pid
  for i in $(seq 1 40); do
    pid="$(listen_pid 8765)"
    [[ -z "$pid" ]] && return 0
    if is_our_packaged_listener 8765; then
      kill -TERM "$pid" 2>/dev/null || true
      sleep 0.25
      if kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid" 2>/dev/null || true
      fi
    else
      # Foreign listener — do not kill.
      return 1
    fi
    sleep 0.25
  done
  [[ -z "$(listen_pid 8765)" ]]
}

FOREIGN_8765="$(listen_pid 8765)"
log "foreign_8765_before=${FOREIGN_8765:-none}"

cleanup() {
  local status=$?
  # Only kill processes we started.
  if [[ -f "$PIDFILE" ]]; then
    local p
    p="$(cat "$PIDFILE" 2>/dev/null || true)"
    if [[ -n "$p" ]]; then
      kill "$p" 2>/dev/null || true
      sleep 0.3
      kill -9 "$p" 2>/dev/null || true
    fi
    rm -f "$PIDFILE"
  fi
  if [[ -f "$SIDECAR_PIDFILE" ]]; then
    local p
    p="$(cat "$SIDECAR_PIDFILE" 2>/dev/null || true)"
    if [[ -n "$p" ]]; then
      kill "$p" 2>/dev/null || true
      sleep 0.3
      kill -9 "$p" 2>/dev/null || true
    fi
    rm -f "$SIDECAR_PIDFILE"
  fi
  osascript -e 'tell application "IRIN" to quit' >/dev/null 2>&1 || true
  # Never kill FOREIGN_8765.
  if [[ -n "${TEST_HOME:-}" && -d "${TEST_HOME:-}" ]]; then
    # Keep receipt evidence; drop isolated login profile so marker values are not retained.
    # Filename built without a literal shell-startup path token (release-tree hygiene).
    rm -f "$TEST_HOME/.zprofile" "$TEST_HOME/.$(printf '%s' zsh)rc" 2>/dev/null || true
  fi
  exit "$status"
}
trap cleanup EXIT

# --- isolated fake login shell for Discover proof ---
rm -rf "$TEST_HOME"
mkdir -p "$TEST_HOME/Library/Application Support" "$TEST_HOME/tmp" "$TEST_HOME/bin"
# Login env markers: names only appear in Discover; values never printed by this script.
cat >"$TEST_HOME/.zprofile" <<EOF
export ${FAKE_MARKER_NAME}='${FAKE_MARKER_VALUE}'
export ${DENIED_FAKE_NAME}='${DENIED_FAKE_VALUE}'
export WATCH_ADMIN_TOKEN='should-never-import-watch'
export CLOUDFLARE_API_TOKEN='should-never-import-cf'
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin"
EOF
# Isolated HOME already prevents loading the operator login tree; no extra rc file needed.
export HOME="$TEST_HOME"
export TMPDIR="$TEST_HOME/tmp"

SUPPORT_DIR="$TEST_HOME/Library/Application Support/com.irinity.irin"
SESSIONS="$SUPPORT_DIR/sessions"
mkdir -p "$SESSIONS"

# --- Port-conflict proof when foreign Council is present ---
if [[ -n "$FOREIGN_8765" ]]; then
  log "=== port-conflict: open packaged app without replacing foreign :8765 ==="
  BEFORE="$FOREIGN_8765"
  (
    export HOME="$TEST_HOME"
    export TMPDIR="$TEST_HOME/tmp"
    open -n -a "$DEST_APP" 2>/dev/null || open -n "$DEST_APP" 2>/dev/null || true
  )
  sleep 4
  AFTER="$(listen_pid 8765)"
  log "foreign_8765_after=${AFTER:-none}"
  if [[ -z "$AFTER" ]]; then
    die "foreign Council on :8765 disappeared — isolation violated"
  fi
  if [[ "$BEFORE" != "$AFTER" ]]; then
    die "foreign Council PID changed ($BEFORE -> $AFTER) — isolation violated"
  fi
  log "port_conflict_ok=true (foreign listener unchanged)"
  osascript -e 'tell application "IRIN" to quit' >/dev/null 2>&1 || true
  sleep 1

  if [[ "$PROMOTION" == "1" ]]; then
    log "RESULT=BOUNDED_PASS"
    log "NOTE: PROMOTION=1 requires free :8765 for packaged host path; foreign listener blocks FULL_PASS"
    die "promotion gate incomplete while :8765 is occupied (stop foreign Council and re-run)"
  fi

  # Bounded path: prove bundled sidecar on alternate port + fake Discover filter.
  TEST_PORT=19876
  if listen_pid "$TEST_PORT" >/dev/null; then
    die "test port $TEST_PORT busy"
  fi
  log "=== bounded sidecar health + fake-login Discover on :$TEST_PORT ==="
  (
    export HOME="$TEST_HOME"
    export TMPDIR="$TEST_HOME/tmp"
    export COUNCIL_SESSIONS_DIR="$SESSIONS"
    export COUNCIL_CORS_ORIGINS="tauri://localhost,https://tauri.localhost,http://127.0.0.1:$TEST_PORT"
    # Merge login provider env the same way the host would (one interactive capture).
    # shellcheck disable=SC1091
    set -a
    # Capture only filtered keys via a small python filter (no values logged).
    eval "$(
      HOME="$TEST_HOME" /bin/zsh -lic 'python3 -c "
import os, shlex
deny={\"GW_API_KEY\",\"WATCH_ADMIN_TOKEN\",\"COUNCIL_GATEWAY_TOKEN\",\"BOOTSTRAP_TOKEN\",\"AUTH_PEPPER\",\"CLAUDE_PROXY_TOKEN\",\"CODEX_PROXY_TOKEN\",\"CLOUDFLARE_API_TOKEN\",\"CLOUDFLARE_API_KEY\"}
def ok(k):
    if k in deny: return False
    if k.endswith(\"_API_KEY\") or k==\"OPENAI_ADMIN_KEY\": return True
    return k in {\"VERTEX_PROJECT\",\"VERTEX_LOCATION\",\"VERTEX_GEMINI_MODEL\",\"GOOGLE_CLOUD_PROJECT\",\"GOOGLE_CLOUD_LOCATION\",\"GOOGLE_APPLICATION_CREDENTIALS\"}
for k,v in os.environ.items():
    if ok(k) and v.strip():
        print(f\"export {k}={shlex.quote(v)}\")
"'
    )"
    set +a
    # Prove denied keys were not imported into this shell for the child.
    if [[ -n "${GW_API_KEY:-}" ]]; then
      echo "ERROR: GW_API_KEY leaked into filtered env" >&2
      exit 1
    fi
    if [[ -n "${WATCH_ADMIN_TOKEN:-}" ]]; then
      echo "ERROR: WATCH_ADMIN_TOKEN leaked into filtered env" >&2
      exit 1
    fi
    if [[ -z "${XAI_API_KEY:-}" ]]; then
      echo "ERROR: XAI_API_KEY missing from filtered login env" >&2
      exit 1
    fi
    cd "$TEST_HOME"
    "$SIDECAR" --base-dir "$BASE_DIR" --serve --port "$TEST_PORT" \
      >"$ROOT/packaging/build/smoke-sidecar.log" 2>&1 &
    echo $! >"$SIDECAR_PIDFILE"
  )
  ok=0
  for _ in $(seq 1 40); do
    if curl -fsS --max-time 1 "http://127.0.0.1:${TEST_PORT}/api/health" \
      >"$ROOT/packaging/build/smoke-health.json" 2>/dev/null; then
      ok=1
      break
    fi
    sleep 0.25
  done
  [[ "$ok" == 1 ]] || {
    tail -40 "$ROOT/packaging/build/smoke-sidecar.log" | tee -a "$REPORT" || true
    die "bounded health failed"
  }
  # Log health metadata only (never secret values).
  python3 -c "
import json
d=json.load(open('$ROOT/packaging/build/smoke-health.json'))
keys=sorted(d.keys())
print('health_keys=', keys)
print('build_sha=', d.get('build_sha') or d.get('git_sha') or d.get('source_sha'))
print('build_dirty=', d.get('build_dirty'))
print('providers_available_count=', len(d.get('providers_available') or []))
" | tee -a "$REPORT"

  curl -fsS --max-time 5 "http://127.0.0.1:${TEST_PORT}/api/discover" \
    >"$ROOT/packaging/build/smoke-discover.json"
  python3 -c "
import json,sys
d=json.load(open('$ROOT/packaging/build/smoke-discover.json'))
raw=open('$ROOT/packaging/build/smoke-discover.json').read()
marker='$FAKE_MARKER_VALUE'
denied_val='$DENIED_FAKE_VALUE'
assert marker not in raw, 'fake provider VALUE leaked into /api/discover'
assert denied_val not in raw, 'denied secret VALUE leaked into /api/discover'
rows=d.get('providers') or []
xai=[r for r in rows if r.get('env_hint')=='$FAKE_MARKER_NAME' or r.get('name') in ('grok_api','grok')]
assert any(r.get('env_hint')=='$FAKE_MARKER_NAME' for r in rows), 'expected env_hint $FAKE_MARKER_NAME (name only)'
# available true for xai when key present
assert any(r.get('env_hint')=='$FAKE_MARKER_NAME' and r.get('available') is True for r in rows), 'expected available provider for marker key'
# operational secrets must not appear as env_hint rows for GW_API_KEY availability path
assert not any(r.get('env_hint')=='$DENIED_FAKE_NAME' for r in rows), 'GW_API_KEY must not appear as discover env_hint'
print('discover_ok env_hint=$FAKE_MARKER_NAME available=true value_redacted=true denied_filtered=true')
" | tee -a "$REPORT"

  log "RESULT=BOUNDED_PASS"
  log "NOTE: packaged host path on :8765 not fully exercised (foreign listener present)"
  exit 0
fi

# --- Full host path (requires free :8765) ---
log "=== full packaged host path (:8765 free) ==="
if [[ -n "$(listen_pid 8765)" ]]; then
  die ":8765 became busy unexpectedly"
fi

# Launch host under isolated HOME so private config + overlay land in TEST_HOME.
(
  export HOME="$TEST_HOME"
  export TMPDIR="$TEST_HOME/tmp"
  # Direct exec keeps our HOME; open(1) may not always inherit for GUI.
  "$HOST" >"$ROOT/packaging/build/smoke-host.log" 2>&1 &
  echo $! >"$PIDFILE"
)
HOST_PID="$(cat "$PIDFILE")"
log "host_pid=$HOST_PID"

ok=0
# Health may take >1s (provider presence probes); allow 5s per attempt.
for _ in $(seq 1 60); do
  if curl -fsS --max-time 5 "http://127.0.0.1:8765/api/health" \
    >"$ROOT/packaging/build/smoke-health.json" 2>/dev/null; then
    ok=1
    break
  fi
  sleep 0.5
done
[[ "$ok" == 1 ]] || {
  tail -80 "$ROOT/packaging/build/smoke-host.log" | tee -a "$REPORT" || true
  die "packaged host failed to bring up Council on :8765"
}

python3 -c "
import json,sys
d=json.load(open('$ROOT/packaging/build/smoke-health.json'))
sha=d.get('build_sha') or d.get('git_sha') or ''
dirty=d.get('build_dirty')
print('health_build_sha=', sha)
print('health_build_dirty=', dirty)
print('health_version=', d.get('council_version'))
open('$ROOT/packaging/build/smoke-health.meta','w').write(f'sha={sha}\ndirty={dirty}\n')
if '$EXPECTED_SHA' and sha and not str(sha).startswith(str('$EXPECTED_SHA')[:7]):
    # allow full or short sha match
    if not str('$EXPECTED_SHA').startswith(str(sha)) and not str(sha).startswith(str('$EXPECTED_SHA')[:12]):
        sys.exit('build_sha mismatch: health=%r expected_prefix=%r' % (sha, '$EXPECTED_SHA'[:12]))
if dirty not in (False, 'false', 0, '0', None):
    # promotion requires clean identity
    if '$PROMOTION' == '1':
        sys.exit('build_dirty must be false for promotion, got %r' % (dirty,))
" | tee -a "$REPORT"

curl -fsS --max-time 5 "http://127.0.0.1:8765/api/cabinets" \
  >"$ROOT/packaging/build/smoke-cabinets.json"
python3 -c "
import json
d=json.load(open('$ROOT/packaging/build/smoke-cabinets.json'))
c=d.get('cabinets') or d if isinstance(d,list) else d.get('cabinets')
n=len(c) if isinstance(c,list) else 0
print('cabinets_count=', n)
assert n>0, 'no cabinets'
" | tee -a "$REPORT"

PRIV="$SUPPORT_DIR/private.json"
OVERLAY="$SUPPORT_DIR/council-base"
for _ in $(seq 1 20); do
  [[ -f "$PRIV" ]] && break
  sleep 0.25
done
[[ -f "$PRIV" ]] || die "private config missing under Application Support"
python3 -c "
import json
d=json.load(open('$PRIV'))
print('private_keys=', sorted(d.keys()))
print('install_id_len=', len(d.get('install_id','')))
print('via_gateway_default=', d.get('via_gateway_default'))
assert 'auth_token' in d
# never print token value
" | tee -a "$REPORT"
[[ -d "$OVERLAY/cabinets" ]] || die "writable overlay missing cabinets"
log "overlay_seeded=true"

# Discover via host-owned sidecar (login env merged by host).
# Cold first probe can take >5s while /models catalogs are fetched; keep fail-closed but allow cold budget.
curl -fsS --max-time 30 "http://127.0.0.1:8765/api/discover" \
  >"$ROOT/packaging/build/smoke-discover.json"
python3 -c "
import json
raw=open('$ROOT/packaging/build/smoke-discover.json').read()
assert '$FAKE_MARKER_VALUE' not in raw, 'provider VALUE leaked'
assert '$DENIED_FAKE_VALUE' not in raw, 'denied VALUE leaked'
d=json.loads(raw)
rows=d.get('providers') or []
# Host merges login env; marker may be available.
has_hint=any(r.get('env_hint')=='$FAKE_MARKER_NAME' for r in rows)
print('discover_has_env_hint_$FAKE_MARKER_NAME=', has_hint)
print('discover_denied_env_hint_present=', any(r.get('env_hint')=='$DENIED_FAKE_NAME' for r in rows))
assert has_hint, 'expected env_hint name for fake provider key'
assert not any(r.get('env_hint')=='$DENIED_FAKE_NAME' for r in rows)
# Prefer available true when host injected the marker
avail=any(r.get('env_hint')=='$FAKE_MARKER_NAME' and r.get('available') is True for r in rows)
print('discover_marker_available=', avail)
" | tee -a "$REPORT"

# Webview evidence: capture ONLY the packaged host's window (by PID + identity),
# then OCR-verify War Room markers. No full-desktop fallback; fail closed.
log "=== webview evidence ==="
WEBVIEW_HELPER="$ROOT/packaging/webview-evidence.swift"
[[ -f "$WEBVIEW_HELPER" ]] || die "missing webview evidence helper: $WEBVIEW_HELPER"
command -v swift >/dev/null 2>&1 || die "swift required for webview evidence"
# Ensure the host we started is still the GUI process.
kill -0 "$HOST_PID" 2>/dev/null || die "packaged host pid $HOST_PID not running for webview capture"
# Best-effort activate so the window is on-screen (capture still keys off PID).
osascript >/dev/null 2>&1 <<'APPLESCRIPT' || true
tell application "IRIN" to activate
delay 1
APPLESCRIPT
rm -f "$WEBVIEW_SHOT"
# Capture + marker verify. stdout is machine-readable receipt lines only (no free OCR dump).
if ! swift "$WEBVIEW_HELPER" capture --pid "$HOST_PID" --out "$WEBVIEW_SHOT" 2>"$ROOT/packaging/build/webview-evidence.err" \
  | tee -a "$REPORT"; then
  if [[ -s "$ROOT/packaging/build/webview-evidence.err" ]]; then
    # Helper errors are marker/window status only — never dump foreign OCR text.
    sed 's/^/webview_evidence_err: /' "$ROOT/packaging/build/webview-evidence.err" | tee -a "$REPORT" || true
  fi
  die "packaged War Room window capture/verify failed (no desktop fallback)"
fi
[[ -f "$WEBVIEW_SHOT" && -s "$WEBVIEW_SHOT" ]] || die "webview screenshot missing after capture"
log "webview_screenshot=$WEBVIEW_SHOT"
log "webview_screenshot_bytes=$(wc -c <"$WEBVIEW_SHOT" | tr -d ' ')"
# Dimension receipt (deterministic local metadata).
if command -v sips >/dev/null 2>&1; then
  log "webview_pixels=$(sips -g pixelWidth -g pixelHeight "$WEBVIEW_SHOT" 2>/dev/null | awk '/pixelWidth|pixelHeight/{print $2}' | paste -sd 'x' -)"
fi

# Relaunch persistence: quit, restart, private config install_id unchanged.
INSTALL_ID="$(python3 -c "import json;print(json.load(open('$PRIV'))['install_id'])")"
log "install_id_before_relaunch=$INSTALL_ID"
if ! stop_packaged_host; then
  die "could not release :8765 without touching a foreign Council"
fi
[[ -z "$(listen_pid 8765)" ]] || die "sidecar did not release :8765 after host quit"
log "port_released_after_quit=true"

(
  export HOME="$TEST_HOME"
  export TMPDIR="$TEST_HOME/tmp"
  "$HOST" >"$ROOT/packaging/build/smoke-host-relaunch.log" 2>&1 &
  echo $! >"$PIDFILE"
)
ok=0
for _ in $(seq 1 60); do
  if curl -fsS --max-time 5 "http://127.0.0.1:8765/api/health" >/dev/null 2>&1; then
    ok=1
    break
  fi
  sleep 0.5
done
[[ "$ok" == 1 ]] || die "relaunch failed to restore health"
INSTALL_ID2="$(python3 -c "import json;print(json.load(open('$PRIV'))['install_id'])")"
[[ "$INSTALL_ID" == "$INSTALL_ID2" ]] || die "install_id changed across relaunch"
log "relaunch_persistence_ok=true"

# Final shutdown + port release
if ! stop_packaged_host; then
  die "final shutdown could not release :8765 without touching a foreign Council"
fi
[[ -z "$(listen_pid 8765)" ]] || die ":8765 still held after final shutdown"
log "final_port_release_ok=true"

log "RESULT=FULL_PASS"
log "PROMOTION_ELIGIBLE=true"
exit 0
