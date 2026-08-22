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

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

# Exact candidate path required — no packaging/test-apps, packaging/artifacts,
# or /Applications fallback. Always fresh-extract the named DMG into smoke/.
[[ -z "${IRIN_SMOKE_APP:-}" ]] \
  || die "IRIN_SMOKE_APP is forbidden; pass IRIN_CANDIDATE_PATH and let smoke extract into candidate/smoke/"
irin_require_candidate_path
CANDIDATE="$IRIN_CANDIDATE_PATH"
SMOKE_ROOT="$CANDIDATE/smoke"
DEST_APP="$SMOKE_ROOT/$APP_NAME"
MOUNT="$SMOKE_ROOT/dmg-mount"
DMG="$(find "$CANDIDATE" -maxdepth 1 -type f -name '*.dmg' | head -1 || true)"
[[ -n "$DMG" && -f "$DMG" ]] || die "candidate DMG missing under $CANDIDATE"
HASHES_PATH="$CANDIDATE/HASHES.txt"
REPORT="$CANDIDATE/logs/FULL_APP_SMOKE.txt"
WEBVIEW_SHOT="$CANDIDATE/logs/webview-smoke.png"
WEBVIEW_SHOT_RELAUNCH="$CANDIDATE/logs/webview-smoke-relaunch.png"
PIDFILE="$SMOKE_ROOT/smoke-host.pid"
SIDECAR_PIDFILE="$SMOKE_ROOT/smoke-sidecar.pid"
TEST_HOME="$ROOT/packaging/test-home/smoke-$$"
FAKE_MARKER_NAME="XAI_API_KEY"
FAKE_MARKER_VALUE="irin-dmg-fake-marker-not-a-real-key"
DENIED_FAKE_NAME="GW_API_KEY"
DENIED_FAKE_VALUE="should-never-import-gateway-key"

if [[ -n "${IRIN_DMG_PATH:-}" ]]; then
  [[ "$(cd "$(dirname "$IRIN_DMG_PATH")" && pwd)/$(basename "$IRIN_DMG_PATH")" == "$(cd "$(dirname "$DMG")" && pwd)/$(basename "$DMG")" ]] \
    || die "IRIN_DMG_PATH must be the candidate DMG"
fi
if [[ -n "${IRIN_DMG_HASHES_PATH:-}" ]]; then
  [[ "$(cd "$(dirname "$IRIN_DMG_HASHES_PATH")" && pwd)/$(basename "$IRIN_DMG_HASHES_PATH")" == "$HASHES_PATH" ]] \
    || die "IRIN_DMG_HASHES_PATH must be the candidate HASHES.txt"
fi

log() { printf '%s\n' "$*" | tee -a "$REPORT"; }

# Bind provenance to the candidate HASHES.txt.
SOURCE_SHA_COUNT="$(awk -F= '$1 == "source_sha" { count++ } END { print count + 0 }' "$HASHES_PATH")"
[[ "$SOURCE_SHA_COUNT" == "1" ]] \
  || die "receipt must contain exactly one source_sha entry (found $SOURCE_SHA_COUNT): $HASHES_PATH"
RECEIPT_SOURCE_SHA="$(awk -v prefix='source_sha=' 'index($0, prefix) == 1 { print substr($0, length(prefix) + 1); exit }' "$HASHES_PATH")"
[[ "$RECEIPT_SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]] \
  || die "receipt source_sha must be one lowercase 40-character git SHA: $HASHES_PATH"
if [[ -n "${IRIN_TAURI_BUILD_GIT_SHA:-}" && "$IRIN_TAURI_BUILD_GIT_SHA" != "$RECEIPT_SOURCE_SHA" ]]; then
  die "IRIN_TAURI_BUILD_GIT_SHA does not match candidate receipt source_sha"
fi
export IRIN_TAURI_BUILD_GIT_SHA="$RECEIPT_SOURCE_SHA"
EXPECTED_SHA="$RECEIPT_SOURCE_SHA"
IRIN_RELEASE_VERSION="$(awk -v prefix='release_version=' 'index($0, prefix) == 1 { print substr($0, length(prefix) + 1); exit }' "$HASHES_PATH")"
[[ -n "$IRIN_RELEASE_VERSION" ]] || die "receipt missing release_version: $HASHES_PATH"

mkdir -p "$CANDIDATE/logs" "$SMOKE_ROOT" "$(dirname "$PIDFILE")"
: >"$REPORT"
log "=== smoke-full-app $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
log "ROOT=$ROOT"
log "CANDIDATE=$CANDIDATE"
log "PROMOTION=$PROMOTION"
log "IRIN_RELEASE_VERSION=$IRIN_RELEASE_VERSION"
log "expected_sha=${EXPECTED_SHA:-unknown}"

[[ "$(uname -s)" == "Darwin" ]] || die "macOS only"
[[ "$(uname -m)" == "arm64" ]] || die "arm64 only"

# Always fresh-extract the named DMG into the candidate's own smoke/.
# An existing extracted app cannot bypass extraction.
if mount | grep -q "$MOUNT"; then
  hdiutil detach "$MOUNT" -force 2>/dev/null || true
fi
rm -rf "$DEST_APP" "$MOUNT"
mkdir -p "$MOUNT"
hdiutil attach "$DMG" -mountpoint "$MOUNT" -readonly -nobrowse
# Detach on any failure after attach (same pattern as verify-dmg / install-verify).
trap 'hdiutil detach "$MOUNT" -force 2>/dev/null || true' EXIT
SRC_APP="$(find "$MOUNT" -maxdepth 2 -name "$APP_NAME" -type d | head -1 || true)"
[[ -d "$SRC_APP" ]] || die "app not found in DMG"
ditto "$SRC_APP" "$DEST_APP"
hdiutil detach "$MOUNT" -force 2>/dev/null || true
trap - EXIT
rm -rf "$MOUNT"
[[ -d "$DEST_APP" ]] || die "missing app after extract: $DEST_APP"
log "fresh_extract=true dest_app=$DEST_APP"

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
HERMES_ADAPTER="$BASE_DIR/scripts/hermes-seat-adapter.sh"
[[ -x "$HERMES_ADAPTER" ]] || die "hermes seat adapter missing or not executable: $HERMES_ADAPTER"
log "hermes_adapter=$HERMES_ADAPTER"
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

# Stop one unix process by PID only. Never targets by display name (which would
# also hit /Applications/IRIN.app when an isolated DMG copy is under test).
stop_unix_pid() {
  local p="${1:-}"
  [[ -n "$p" ]] || return 0
  kill -0 "$p" 2>/dev/null || return 0
  kill -TERM "$p" 2>/dev/null || true
  local i
  for i in $(seq 1 40); do
    if ! kill -0 "$p" 2>/dev/null; then
      return 0
    fi
    sleep 0.25
  done
  if kill -0 "$p" 2>/dev/null; then
    kill -KILL "$p" 2>/dev/null || true
  fi
}

# Bring the exact packaged host window forward by unix PID (System Events).
# Display-name "tell application \"IRIN\"" is forbidden — it can activate the
# installed /Applications copy instead of the extracted DMG test app.
activate_unix_pid() {
  local p="${1:-}"
  [[ -n "$p" ]] || return 0
  kill -0 "$p" 2>/dev/null || return 0
  osascript >/dev/null 2>&1 <<EOF || true
tell application "System Events"
  set matched to every process whose unix id is ${p}
  if (count of matched) > 0 then
    set frontmost of item 1 of matched to true
  end if
end tell
delay 1
EOF
}

# Stop host processes whose argv is the exact packaged test-app host binary.
stop_dest_app_hosts() {
  local host_bin="$DEST_APP/Contents/MacOS/council-warroom-tauri"
  [[ -x "$host_bin" ]] || return 0
  local pattern pid
  pattern="$(printf '%s\n' "$host_bin" | sed 's/[][\\.^$*+?{}()|]/\\&/g')"
  while read -r pid; do
    [[ -n "$pid" ]] || continue
    stop_unix_pid "$pid"
  done < <(pgrep -f -x "$pattern" 2>/dev/null || true)
}

# Gracefully stop the packaged host and any sidecar it left behind on :8765.
stop_packaged_host() {
  local p=""
  if [[ -f "$PIDFILE" ]]; then
    p="$(cat "$PIDFILE" 2>/dev/null || true)"
  fi
  if [[ -z "$p" && -n "${HOST_PID:-}" ]]; then
    p="$HOST_PID"
  fi
  # TERM the exact host PID first so Tauri Exit can reclaim the owned sidecar.
  stop_unix_pid "$p"
  rm -f "$PIDFILE"
  # Reclaim :8765 only if it is still held by our packaged path (never foreign).
  local i pid
  for i in $(seq 1 40); do
    pid="$(listen_pid 8765)"
    [[ -z "$pid" ]] && return 0
    if is_our_packaged_listener 8765; then
      stop_unix_pid "$pid"
    else
      # Foreign listener — do not kill.
      return 1
    fi
    sleep 0.25
  done
  [[ -z "$(listen_pid 8765)" ]]
}

# Launch the bundle through LaunchServices with the isolated environment and
# resolve the exact packaged host PID (sets HOST_PID, writes PIDFILE).
# Executing the Mach-O directly from a background shell can leave the initial
# Tauri window unordered and invisible, so every launch — including the
# relaunch — goes through here.
launch_packaged_host() {
  local host_log="$1" label="$2"
  if pgrep -f -x "$HOST_PATTERN" >/dev/null 2>&1; then
    die "exact packaged host is already running: $HOST"
  fi
  open -n -F -W \
    -o "$host_log" \
    --stderr "$host_log" \
    --env "HOME=$TEST_HOME" \
    --env "TMPDIR=$TEST_HOME/tmp" \
    "$DEST_APP" &
  local launcher_pid=$!
  HOST_PID=""
  local stable=0 pid
  for _ in $(seq 1 40); do
    pid="$(pgrep -f -x "$HOST_PATTERN" | head -1 || true)"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      HOST_PID="$pid"
      stable=$((stable + 1))
      (( stable >= 3 )) && break
    elif ! kill -0 "$launcher_pid" 2>/dev/null; then
      [[ -r "$host_log" ]] && tail -80 "$host_log" | tee -a "$REPORT"
      die "LaunchServices exited before the packaged host appeared ($label)"
    else
      HOST_PID=""
      stable=0
    fi
    sleep 0.25
  done
  (( stable >= 3 )) || die "packaged host process did not remain stable ($label)"
  echo "$HOST_PID" >"$PIDFILE"
  log "${label}_host_pid=$HOST_PID"
}

# Wait for the packaged host to answer health on :8765.
wait_packaged_health() {
  local host_log="$1" label="$2" out="$3"
  local ok=0
  # Health may take >1s (provider presence probes); allow 5s per attempt.
  for _ in $(seq 1 60); do
    if curl -fsS --max-time 5 "http://127.0.0.1:8765/api/health" >"$out" 2>/dev/null; then
      ok=1
      break
    fi
    sleep 0.5
  done
  [[ "$ok" == 1 ]] || {
    [[ -r "$host_log" ]] && tail -80 "$host_log" | tee -a "$REPORT"
    die "packaged host failed to bring up Council on :8765 ($label)"
  }
}

# The shell keeps running after a failed cold-launch Keychain preload; the
# smoke must not. This is the #0033 blank-window lead: the line goes into the
# report and the run is red.
PRELOAD_FAILURE_MARK="cold-launch secret preload failed"
assert_no_preload_failure() {
  local host_log="$1" label="$2"
  if grep -F -- "$PRELOAD_FAILURE_MARK" "$host_log" >/dev/null 2>&1; then
    grep -F -- "$PRELOAD_FAILURE_MARK" "$host_log" | sed "s/^/${label}_preload_failure: /" | tee -a "$REPORT"
    die "packaged host logged a cold-launch secret preload failure ($label)"
  fi
  log "${label}_preload_failure=none"
}

# First-run hydration (B-02): the webview reports to the shell once health
# and cabinets both loaded; the shell prints this line. Without it the War
# Room is the stale first launch that only a relaunch used to fix.
HYDRATED_MARK="[runtime-config] webview Council requests ready on :8765"
wait_webview_hydrated() {
  local host_log="$1" label="$2" i
  for i in $(seq 1 120); do
    if grep -F -- "$HYDRATED_MARK" "$host_log" >/dev/null 2>&1; then
      log "${label}_webview_hydrated=true"
      return 0
    fi
    sleep 0.5
  done
  [[ -r "$host_log" ]] && tail -80 "$host_log" | tee -a "$REPORT"
  die "packaged webview never reported Council health + cabinets hydrated ($label; no relaunch allowed)"
}

# Capture ONLY the packaged host's window (by PID + identity), then
# OCR-verify War Room markers. No full-desktop fallback; fail closed.
capture_webview_evidence() {
  local shot="$1" label="$2"
  log "=== webview evidence ($label) ==="
  kill -0 "$HOST_PID" 2>/dev/null || die "packaged host pid $HOST_PID not running for webview capture ($label)"
  # Best-effort activate the exact packaged host PID so the window is on-screen
  # (capture still keys off PID; never activate by display name).
  activate_unix_pid "$HOST_PID"
  rm -f "$shot"
  local err="$ROOT/packaging/build/webview-evidence-$label.err"
  # Capture + marker verify. stdout is machine-readable receipt lines only (no free OCR dump).
  if ! swift "$WEBVIEW_HELPER" capture --pid "$HOST_PID" --out "$shot" 2>"$err" \
    | sed "s/^/${label}_/" | tee -a "$REPORT"; then
    if [[ -s "$err" ]]; then
      # Helper errors are marker/window status only — never dump foreign OCR text.
      sed "s/^/${label}_webview_evidence_err: /" "$err" | tee -a "$REPORT"
    fi
    die "packaged War Room window capture/verify failed ($label; no desktop fallback)"
  fi
  [[ -f "$shot" && -s "$shot" ]] || die "webview screenshot missing after capture ($label)"
  log "${label}_webview_screenshot=$shot"
  log "${label}_webview_screenshot_bytes=$(wc -c <"$shot" | tr -d ' ')"
  # Dimension receipt (deterministic local metadata).
  if command -v sips >/dev/null 2>&1; then
    log "${label}_webview_pixels=$(sips -g pixelWidth -g pixelHeight "$shot" 2>/dev/null | awk '/pixelWidth|pixelHeight/{print $2}' | paste -sd 'x' -)"
  fi
}

FOREIGN_8765="$(listen_pid 8765)"
log "foreign_8765_before=${FOREIGN_8765:-none}"

cleanup() {
  local status=$?
  # Give the native host time to run its exit handlers and stop its Council
  # child. The shared helper remains ownership-scoped and never kills the
  # foreign listener recorded before this smoke.
  stop_packaged_host || true
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
  # Never kill FOREIGN_8765.
  if [[ -n "${TEST_HOME:-}" \
    && "$TEST_HOME" == "$ROOT/packaging/test-home/smoke-"* \
    && -d "$TEST_HOME" ]]; then
    # Durable receipts live under packaging/receipts. Remove only this run's
    # guarded isolated home so generated overlays and fake provider markers do
    # not accumulate or poison a later repository-wide secret scan.
    rm -rf -- "$TEST_HOME"
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
  # Quit only hosts whose binary path is the isolated DEST_APP — never by
  # display name, which would address /Applications/IRIN.app.
  stop_dest_app_hosts
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

HOST_PATTERN="$(printf '%s\n' "$HOST" | sed 's/[][\\.^$*+?{}()|]/\\&/g')"
HOST_LOG="$ROOT/packaging/build/smoke-host.log"
launch_packaged_host "$HOST_LOG" "launch"
log "host_pid=$HOST_PID"
wait_packaged_health "$HOST_LOG" "launch" "$ROOT/packaging/build/smoke-health.json"
assert_no_preload_failure "$HOST_LOG" "launch"
wait_webview_hydrated "$HOST_LOG" "launch"

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
[[ -x "$OVERLAY/scripts/hermes-seat-adapter.sh" ]] \
  || die "writable overlay missing executable hermes seat adapter"
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

WEBVIEW_HELPER="$ROOT/packaging/webview-evidence.swift"
[[ -f "$WEBVIEW_HELPER" ]] || die "missing webview evidence helper: $WEBVIEW_HELPER"
command -v swift >/dev/null 2>&1 || die "swift required for webview evidence"
capture_webview_evidence "$WEBVIEW_SHOT" "launch"

# Relaunch persistence: quit, restart, private config install_id unchanged —
# and the relaunched window must pass the same War Room proof as the first
# launch (a blank relaunch window used to pass because only health was checked).
INSTALL_ID="$(python3 -c "import json;print(json.load(open('$PRIV'))['install_id'])")"
log "install_id_before_relaunch=$INSTALL_ID"
if ! stop_packaged_host; then
  die "could not release :8765 without touching a foreign Council"
fi
[[ -z "$(listen_pid 8765)" ]] || die "sidecar did not release :8765 after host quit"
log "port_released_after_quit=true"

RELAUNCH_LOG="$ROOT/packaging/build/smoke-host-relaunch.log"
launch_packaged_host "$RELAUNCH_LOG" "relaunch"
wait_packaged_health "$RELAUNCH_LOG" "relaunch" "$ROOT/packaging/build/smoke-health-relaunch.json"
assert_no_preload_failure "$RELAUNCH_LOG" "relaunch"
wait_webview_hydrated "$RELAUNCH_LOG" "relaunch"
capture_webview_evidence "$WEBVIEW_SHOT_RELAUNCH" "relaunch"
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
