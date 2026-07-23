#!/usr/bin/env bash
# Packaged-app Gateway Pack Enable/Disable/Stop/relaunch/uninstall proof.
#
# Drives the actual installed-release UI controls via Accessibility (System Events).
# Fail-closed: only enabled AXButton controls; no nested static-text click; no
# direct Compose/native fallbacks for Stop/Uninstall or any UI predicate.
# Receipts are non-secret only (categories, PIDs, project, hashes, status codes).
#
# Prerequisites:
#   - Local-dev DMG/app already built and copied (packaging/test-apps or IRIN_SMOKE_APP)
#   - Local-dev gateway images present (make gateway-pack-dev-images)
#   - Port 18080 free (stop canonical Gateway first; restore after)
#   - Accessibility permission for Terminal/automation host
set -euo pipefail

# Deterministic path/selection: no color, no aliases when selection becomes data.
export LANG=C
export LC_ALL=C
export NO_COLOR=1
export TERM=dumb
export CLICOLOR=0
export CLICOLOR_FORCE=0
export GREP_OPTIONS=
unalias -a 2>/dev/null || true

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/packaging/env.sh"

APP_NAME="IRIN.app"
TEST_APPS="$ROOT/packaging/test-apps"
DEST_APP="${IRIN_SMOKE_APP:-$TEST_APPS/$APP_NAME}"
TEST_HOME="$ROOT/packaging/test-home/gw-pack-smoke-$$"
RECEIPT="$ROOT/packaging/receipts/GATEWAY_PACK_UI_SMOKE.txt"
PIDFILE="$ROOT/packaging/build/gw-pack-ui-host.pid"
HOST_LOG="$ROOT/packaging/build/gw-pack-ui-host.log"
FOREIGN_PROJECT="irin-foreign-isolation-probe"
FOREIGN_VOLUME="irin_foreign_isolation_vol"
FOREIGN_KC_SERVICE="com.irinity.irin.test.foreign-ui-$$"
FOREIGN_KC_ACCOUNT="foreign-item"
DOCKER_BIN="/usr/local/bin/docker"
[[ -x "$DOCKER_BIN" ]] || DOCKER_BIN="/Applications/Docker.app/Contents/Resources/bin/docker"
CURL_BIN="/usr/bin/curl"
LSOF_BIN="/usr/sbin/lsof"
SECURITY_BIN="/usr/bin/security"
OSASCRIPT_BIN="/usr/bin/osascript"
PYTHON_BIN="/usr/bin/python3"

FINAL_RESULT="FAIL"
FINAL_ERROR=""
HOST_PID=""
OWNED_COUNCIL_PIDS=""
# Only this run may `compose down --volumes` the fixed desktop project.
DESKTOP_PREEXISTING=false
OWNED_DESKTOP_TEARDOWN=false

die() {
  FINAL_ERROR="$*"
  FINAL_RESULT="FAIL"
  {
    printf 'ERROR: %s\n' "$*"
    printf 'RESULT=FAIL\n'
    printf 'final_error_category=%s\n' "$(error_category "$*")"
  } | tee -a "$RECEIPT" >&2
  exit 1
}

log() { printf '%s\n' "$*" | tee -a "$RECEIPT"; }

error_category() {
  case "$1" in
    *"Accessibility"*|*"ax_"*|*"could not identify"*|*"button"*) printf 'ax_control' ;;
    *"Direct Council"*|*"health failed"*|*"not listening"*) printf 'council_health' ;;
    *"Gateway pack did not"*|*"gateway"*|*"18080"*) printf 'gateway_ready' ;;
    *"Keychain"*|*"keychain"*) printf 'keychain' ;;
    *"PID"*|*"pid"*) printf 'pid_transition' ;;
    *"foreign"*) printf 'isolation' ;;
    *"Docker"*|*"docker"*) printf 'docker' ;;
    *"missing app"*|*"host missing"*) printf 'app_missing' ;;
    *) printf 'other' ;;
  esac
}

mkdir -p "$ROOT/packaging/receipts" "$(dirname "$PIDFILE")"
: >"$RECEIPT"
log "=== smoke-gateway-pack $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
log "app=$DEST_APP"
log "docker_bin=$DOCKER_BIN"

[[ "$(uname -s)" == "Darwin" ]] || die "macOS only"
[[ -d "$DEST_APP" ]] || die "missing app; run verify-dmg / copy from DMG first"
[[ -f "$ROOT/packaging/build/gateway-pack/image-manifest.local.json" ]] \
  || die "missing local-dev image manifest; run make gateway-pack-dev-images"
[[ -x "$DOCKER_BIN" ]] || die "Docker CLI missing at allow-listed paths"
[[ -x "$CURL_BIN" ]] || die "curl missing"
[[ -x "$OSASCRIPT_BIN" ]] || die "osascript missing"
[[ -x "$PYTHON_BIN" ]] || die "python3 missing"

HOST="$DEST_APP/Contents/MacOS/council-warroom-tauri"
[[ -x "$HOST" ]] || die "host missing"

APP_SUPPORT_REL="Library/Application Support/com.irinity.irin"
PACK_MARKER_REL="$APP_SUPPORT_REL/gateway/pack-installed.json"

reclaim_owned_listeners() {
  # Only reclaim :8765 listeners whose command path is the copied app or test home.
  local p cmd
  for p in $("$LSOF_BIN" -nP -iTCP:8765 -sTCP:LISTEN -t 2>/dev/null || true); do
    cmd="$(ps -p "$p" -o command= 2>/dev/null || true)"
    if [[ "$cmd" == *"$DEST_APP"* ]] || [[ "$cmd" == *test-home/gw-pack-smoke* ]]; then
      log "reclaim_owned_listener_pid=$p"
      kill -TERM "$p" 2>/dev/null || true
      sleep 0.2
      kill -KILL "$p" 2>/dev/null || true
    fi
  done
}

graceful_quit_app() {
  set +e
  "$OSASCRIPT_BIN" -e 'tell application "IRIN" to quit' >/dev/null 2>&1 || true
  # Wait for orderly RunEvent cleanup (owned Council stop).
  local i
  for i in $(seq 1 30); do
    if ! pgrep -f "$HOST" >/dev/null 2>&1; then
      break
    fi
    sleep 0.2
  done
  if [[ -f "$PIDFILE" ]]; then
    local p
    p="$(cat "$PIDFILE" 2>/dev/null || true)"
    if [[ -n "$p" ]] && kill -0 "$p" 2>/dev/null; then
      kill -TERM "$p" 2>/dev/null || true
      sleep 0.5
      if kill -0 "$p" 2>/dev/null; then
        log "host_force_kill_after_graceful=true"
        kill -KILL "$p" 2>/dev/null || true
      fi
    fi
    rm -f "$PIDFILE"
  fi
  # Reclaim only proven owned listeners (never foreign/canonical).
  reclaim_owned_listeners
  set -e
}

desktop_project_present() {
  # Any container (running or stopped) under the fixed desktop project name.
  if "$DOCKER_BIN" compose -p irin-desktop-gateway ps -a -q 2>/dev/null | grep -q .; then
    return 0
  fi
  # compose ls covers named projects that still hold networks/volumes.
  if "$DOCKER_BIN" compose ls -a 2>/dev/null | grep -q 'irin-desktop-gateway'; then
    return 0
  fi
  return 1
}

cleanup() {
  local ec=$?
  set +e
  log "cleanup_begin=true"
  graceful_quit_app
  # Desktop pack project only when this run created it — never project "gateway",
  # never a pre-existing operator desktop pack.
  if [[ "$OWNED_DESKTOP_TEARDOWN" == "true" ]]; then
    "$DOCKER_BIN" compose -p irin-desktop-gateway down --volumes --remove-orphans >/dev/null 2>&1 || true
    log "owned_desktop_teardown=true"
  else
    log "owned_desktop_teardown=false"
  fi
  "$DOCKER_BIN" compose -p "$FOREIGN_PROJECT" down --volumes --remove-orphans >/dev/null 2>&1 || true
  "$DOCKER_BIN" volume rm "$FOREIGN_VOLUME" >/dev/null 2>&1 || true
  # Never delete Keychain items from the harness (including the foreign fixture).
  # Presence-only checks during the run; operator/manual cleanup only.
  log "cleanup_done=true"
  if [[ -z "${FINAL_ERROR}" && "$ec" -ne 0 && "$FINAL_RESULT" != "PASS" ]]; then
    FINAL_RESULT="FAIL"
    log "RESULT=FAIL"
    log "final_error_category=exit_${ec}"
  fi
  if [[ "$FINAL_RESULT" == "PASS" ]]; then
    log "RESULT=PASS"
  fi
  # Do not dump TEST_HOME (may contain non-secret private.json only).
}
trap cleanup EXIT

# Refuse before Enable if the fixed desktop project already exists (do not
# destroy a pre-existing operator pack with down --volumes).
if desktop_project_present; then
  DESKTOP_PREEXISTING=true
  log "desktop_preexisting=true"
  die "refuse: irin-desktop-gateway already exists; stop/remove it before smoke (will not down --volumes a foreign desktop pack)"
fi
DESKTOP_PREEXISTING=false
log "desktop_preexisting=false"

# Port 18080 must not be owned by canonical/foreign project.
if "$CURL_BIN" -fsS --max-time 2 "http://127.0.0.1:18080/health" >/dev/null 2>&1; then
  if ! "$DOCKER_BIN" compose -p irin-desktop-gateway ps -q 2>/dev/null | grep -q .; then
    die "port 18080 busy outside irin-desktop-gateway; stop canonical Gateway first"
  fi
fi

# Foreign fixtures
"$DOCKER_BIN" volume create "$FOREIGN_VOLUME" >/dev/null
cat >"$ROOT/packaging/build/foreign-compose.yml" <<EOF
name: $FOREIGN_PROJECT
services:
  keep:
    image: busybox:1.36
    command: ["sleep", "3600"]
    volumes:
      - foreign_data:/data
volumes:
  foreign_data:
    name: $FOREIGN_VOLUME
    external: true
EOF
"$DOCKER_BIN" compose -p "$FOREIGN_PROJECT" -f "$ROOT/packaging/build/foreign-compose.yml" up -d >/dev/null
"$SECURITY_BIN" add-generic-password -U -s "$FOREIGN_KC_SERVICE" -a "$FOREIGN_KC_ACCOUNT" -w "not-a-secret-marker" >/dev/null
log "foreign_project=$FOREIGN_PROJECT"
log "foreign_volume=$FOREIGN_VOLUME"
log "foreign_keychain_service=$FOREIGN_KC_SERVICE"

# Isolate app data only. Keep real HOME so Security.framework retains the
# operator login keychain (remapping HOME causes Keychain Not Found modal).
rm -rf "$TEST_HOME"
APP_SUPPORT_ROOT="$TEST_HOME/$APP_SUPPORT_REL"
mkdir -p "$TEST_HOME/tmp" "$APP_SUPPORT_ROOT"
export IRIN_APP_SUPPORT_ROOT="$APP_SUPPORT_ROOT"
export TMPDIR="$TEST_HOME/tmp"
# Do not export HOME=$TEST_HOME — Keychain must use the current login session.
log "app_support_root_override=set"
log "home_remapped=false"
# Never inherit a parent-shell DOCKER_CONFIG (debug/harness pollution).
# The packaged host injects its own managed plugin config under app support.
unset DOCKER_CONFIG

listen_pid() { "$LSOF_BIN" -nP -iTCP:"$1" -sTCP:LISTEN -t 2>/dev/null | head -1 || true; }

wait_health() {
  local ok=0
  local i
  for i in $(seq 1 90); do
    if "$CURL_BIN" -fsS --max-time 2 "http://127.0.0.1:8765/api/health" >/dev/null 2>&1; then
      ok=1
      break
    fi
    sleep 0.5
  done
  [[ "$ok" == 1 ]]
}

# Strict AX: only enabled buttons whose accessible name matches exactly.
# No nested static-text / generic UI element click.
ax_click_button() {
  local label="$1"
  "$OSASCRIPT_BIN" <<APPLESCRIPT
tell application "System Events"
  tell process "IRIN"
    set frontmost to true
    delay 0.4
    -- Top-level window buttons first
    try
      set b to first button of window 1 whose name is "${label}"
      if enabled of b is false then return "disabled"
      click b
      return "clicked_button"
    end try
    -- Entire-contents search, but only button role
    try
      set matches to entire contents of window 1
      repeat with e in matches
        try
          if (role of e is "AXButton" or class of e is button) and (name of e is "${label}") then
            if enabled of e is false then return "disabled"
            click e
            return "clicked_button_nested_role"
          end if
        end try
      end repeat
    end try
  end tell
end tell
return "not_found"
APPLESCRIPT
}

ax_button_enabled() {
  local label="$1"
  "$OSASCRIPT_BIN" <<APPLESCRIPT
tell application "System Events"
  tell process "IRIN"
    try
      set b to first button of window 1 whose name is "${label}"
      if enabled of b then
        return "enabled"
      else
        return "disabled"
      end if
    end try
    try
      set matches to entire contents of window 1
      repeat with e in matches
        try
          if (role of e is "AXButton" or class of e is button) and (name of e is "${label}") then
            if enabled of e then
              return "enabled"
            else
              return "disabled"
            end if
          end if
        end try
      end repeat
    end try
  end tell
end tell
return "not_found"
APPLESCRIPT
}

# Click an enabled button. Prefer an observable busy (disabled) transition.
# Retries the find+click atomically: WebKit AX can drop buttons during pack-status re-renders.
#
# Fast Disable/Stop may omit a disabled/busy hold (button vanishes on re-render).
# A vanished button alone is NOT final proof — callers must assert independent
# postconditions (Disable: new Council PID + Direct + no gateway provider;
# Stop: pack stopped while Direct remains healthy).
ax_click_button_busy() {
  local label="$1"
  local after click_rc busy_seen=0 i attempt
  local allow_fast=0
  if [[ "$label" == "Disable" ]] || [[ "$label" == "Stop pack" ]]; then
    allow_fast=1
  fi
  click_rc="not_found"
  for attempt in $(seq 1 40); do
    click_rc="$(ax_click_button "$label" || true)"
    if [[ "$click_rc" == clicked_button* ]]; then
      log "ax_click_${label// /_}=$click_rc attempt=$attempt"
      break
    fi
    if [[ "$click_rc" == "disabled" ]]; then
      log "ax_click_${label// /_}=disabled attempt=$attempt (waiting)"
    fi
    sleep 0.5
  done
  [[ "$click_rc" == clicked_button* ]] || die "could not click enabled AXButton: $label ($click_rc)"
  for i in $(seq 1 40); do
    after="$(ax_button_enabled "$label" || true)"
    if [[ "$after" == "disabled" ]]; then
      busy_seen=1
      break
    fi
    # Enable: pack marker proves the native install path started.
    if [[ -f "$APP_SUPPORT_ROOT/gateway/pack-installed.json" ]] && [[ "$label" == "Enable Gateway" ]]; then
      busy_seen=1
      break
    fi
    sleep 0.15
  done
  log "ax_busy_${label// /_}=$busy_seen after=${after:-unknown}"
  if [[ "$busy_seen" == 1 ]]; then
    return 0
  fi
  if [[ "$allow_fast" == 1 ]]; then
    # Intermediate only — independent postconditions after return are authoritative.
    log "ax_busy_${label// /_}_fast_transition=true (postconditions_required)"
    return 0
  fi
  die "no busy/state transition after click: $label (command may not have run)"
}

# --- a) Launch Direct ---
: >"$HOST_LOG"
(
  export IRIN_APP_SUPPORT_ROOT="$APP_SUPPORT_ROOT"
  export TMPDIR="$TEST_HOME/tmp"
  unset DOCKER_CONFIG
  # Never pass provider secrets into the smoke host environment from the harness.
  # Keep real HOME for login Keychain; app data goes to IRIN_APP_SUPPORT_ROOT.
  env -u DOCKER_CONFIG "$HOST" >>"$HOST_LOG" 2>&1 &
  echo $! >"$PIDFILE"
)
HOST_PID="$(cat "$PIDFILE")"
log "host_pid=$HOST_PID"
wait_health || die "Direct Council health failed"
PID1="$(listen_pid 8765)"
[[ -n "$PID1" ]] || die "Direct Council not listening"
OWNED_COUNCIL_PIDS="$PID1"
log "direct_council_pid=$PID1"
HEALTH1="$("$CURL_BIN" -fsS --max-time 2 "http://127.0.0.1:8765/api/health")"
"$PYTHON_BIN" - <<'PY' "$HEALTH1"
import json,sys
d=json.loads(sys.argv[1])
assert d.get("build_dirty") in (False, None, "false", False)
print("direct_health_ok build_sha=", (d.get("build_sha") or "")[:12])
print("direct_gateway_provider=", "gateway" in (d.get("providers_available") or []))
assert "providers_available" in d and "providers_missing" in d
assert "council_version" in d
assert d.get("council_version"), "council_version empty"
PY
# Genuine War Room surface: bundled cabinets are listable (not bare health only).
CABINETS_JSON="$("$CURL_BIN" -fsS --max-time 5 "http://127.0.0.1:8765/api/cabinets" || true)"
[[ -n "$CABINETS_JSON" ]] || die "Direct /api/cabinets empty (War Room surface missing)"
"$PYTHON_BIN" - <<'PY' "$CABINETS_JSON"
import json,sys
raw=sys.argv[1]
d=json.loads(raw)
# Accept list or {cabinets:[...]} shapes.
if isinstance(d, list):
    n=len(d)
elif isinstance(d, dict):
    n=len(d.get("cabinets") or d.get("items") or [])
    if n==0 and d:
        n=len(d)
else:
    raise SystemExit("unexpected cabinets payload")
assert n>=1, "no cabinets in Direct War Room"
print("direct_cabinets_count=", n)
PY
log "step_a=direct_ok"

# Wait until an enabled AXButton with the given name is present (pack status
# refresh can leave the Settings panel without action buttons for a few seconds).
# Re-activate + re-open Settings periodically so a closed panel does not starve
# the poll for Enable/Disable/Stop/Uninstall.
wait_ax_button_enabled() {
  local label="$1"
  local timeout_s="${2:-60}"
  local i st
  for i in $(seq 1 "$timeout_s"); do
    st="$(ax_button_enabled "$label" || true)"
    if [[ "$st" == "enabled" ]]; then
      log "ax_wait_${label// /_}=enabled_after_${i}s"
      return 0
    fi
    if (( i % 5 == 0 )); then
      "$OSASCRIPT_BIN" -e 'tell application "IRIN" to activate' >/dev/null 2>&1 || true
      # Keep Settings open without requiring the Settings button every tick.
      ax_click_button "Settings" >/dev/null 2>&1 || true
    fi
    sleep 1
  done
  log "ax_wait_${label// /_}=timeout last=$st"
  return 1
}

# --- b/c) Accessibility: Settings -> Enable Gateway ---
"$OSASCRIPT_BIN" -e 'tell application "IRIN" to activate' >/dev/null 2>&1 || true
sleep 1
SETTINGS_CLICK="not_found"
for i in $(seq 1 30); do
  SETTINGS_CLICK="$(ax_click_button "Settings" || true)"
  if [[ "$SETTINGS_CLICK" == clicked_button* ]]; then
    log "ax_settings_ready_after=${i}s"
    break
  fi
  "$OSASCRIPT_BIN" -e 'tell application "IRIN" to activate' >/dev/null 2>&1 || true
  sleep 1
done
log "ax_settings=$SETTINGS_CLICK"
[[ "$SETTINGS_CLICK" == clicked_button* ]] \
  || die "could not identify/click Settings AXButton (Accessibility fail-closed)"

# Pre-state: pack not installed
[[ ! -f "$APP_SUPPORT_ROOT/gateway/pack-installed.json" ]] || die "pack already installed before Enable"
wait_ax_button_enabled "Enable Gateway" 45 \
  || die "Enable Gateway AXButton never became enabled after Settings"
ax_click_button_busy "Enable Gateway"

# Wait for pack ready: project + /health + admin surface + Keychain presence.
# Do not treat bare /health as Enable-complete — product still provisions after
# health answers (wait_control_plane + admin keys + Keychain store).
ready=0
ADMIN_CODE=""
for attempt in $(seq 1 240); do
  if "$DOCKER_BIN" compose -p irin-desktop-gateway ps -q 2>/dev/null | grep -q .; then
    OWNED_DESKTOP_TEARDOWN=true
  fi
  if "$DOCKER_BIN" compose -p irin-desktop-gateway ps -q 2>/dev/null | grep -q . \
    && "$CURL_BIN" -fsS --max-time 3 "http://127.0.0.1:18080/health" >/dev/null 2>&1; then
    ADMIN_CODE="$("$CURL_BIN" -sS -o /dev/null -w '%{http_code}' --max-time 5 \
      -X POST -H 'Content-Type: application/json' -d '{}' \
      "http://127.0.0.1:18080/admin/keys" || true)"
    kc_key=0
    kc_pepper=0
    if "$SECURITY_BIN" find-generic-password -s "com.irinity.irin" \
      -a "gateway-client-gw-api-key" >/dev/null 2>&1; then
      kc_key=1
    fi
    if "$SECURITY_BIN" find-generic-password -s "com.irinity.irin" \
      -a "gateway-pack-auth-pepper" >/dev/null 2>&1; then
      kc_pepper=1
    fi
    # Same admin predicate as product admin_surface_ready (not 502/503/504/000).
    # Also wait for enable_complete — Keychain presence alone can race ahead of
    # blank-bootstrap recreate + Council governed restart.
    enable_done=0
    LIFE="$APP_SUPPORT_ROOT/gateway/lifecycle.log"
    if [[ -f "$LIFE" ]] && grep -q 'stage=enable_complete detail=authenticated' "$LIFE" 2>/dev/null; then
      enable_done=1
    fi
    if [[ "$ADMIN_CODE" != "000" && "$ADMIN_CODE" != "502" \
      && "$ADMIN_CODE" != "503" && "$ADMIN_CODE" != "504" ]] \
      && [[ "$kc_key" == 1 && "$kc_pepper" == 1 ]] \
      && [[ "$enable_done" == 1 ]]; then
      ready=1
      break
    fi
  fi
  # Fail early if pack marker never appears (command did not reach install).
  if [[ "$attempt" -eq 30 && ! -f "$APP_SUPPORT_ROOT/gateway/pack-installed.json" ]]; then
    die "Enable did not install pack assets (marker missing); native command may not have run"
  fi
  sleep 1
done
if [[ "$ready" != 1 ]]; then
  # Non-secret lifecycle stages only (product writes lifecycle.log).
  LIFE="$APP_SUPPORT_ROOT/gateway/lifecycle.log"
  if [[ -f "$LIFE" ]]; then
    log "lifecycle_log_present=true"
    # Only stage= and detail= tokens; never raw dumps of unknown content beyond fixed lines.
    while IFS= read -r line; do
      if [[ "$line" == *" stage="* ]]; then
        log "lifecycle=${line##* stage=}"
      fi
    done <"$LIFE"
  else
    log "lifecycle_log_present=false"
  fi
  log "admin_surface_http_last=${ADMIN_CODE:-none}"
  die "Gateway pack did not become healthy after Enable"
fi
[[ -f "$APP_SUPPORT_ROOT/gateway/pack-installed.json" ]] || die "pack-installed.json missing after Enable"
OWNED_DESKTOP_TEARDOWN=true
log "owned_desktop_teardown_armed=true"
log "gateway_health=200"
log "project=irin-desktop-gateway"
log "pack_installed_marker=present"
# Isolated app-data root must never grow a login keychain (Keychain stays session).
if [[ -f "$APP_SUPPORT_ROOT/Library/Keychains/login.keychain-db" ]] \
  || [[ -f "$APP_SUPPORT_ROOT/Keychains/login.keychain-db" ]] \
  || [[ -f "$APP_SUPPORT_ROOT/login.keychain-db" ]] \
  || [[ -f "$TEST_HOME/Library/Keychains/login.keychain-db" ]]; then
  die "login.keychain-db created under isolated app-data root (Keychain isolation leak)"
fi
log "isolated_login_keychain_absent=true"

# Sidecar/admin readiness (non-secret status codes only) — re-probe after wait.
ADMIN_CODE="$("$CURL_BIN" -sS -o /dev/null -w '%{http_code}' --max-time 5 \
  -X POST -H 'Content-Type: application/json' -d '{}' \
  "http://127.0.0.1:18080/admin/keys" || true)"
log "admin_surface_http=$ADMIN_CODE"
[[ "$ADMIN_CODE" != "000" && "$ADMIN_CODE" != "502" && "$ADMIN_CODE" != "503" && "$ADMIN_CODE" != "504" ]] \
  || die "admin surface not ready"

# Watch forced off - inspect compose project env via docker inspect (names only).
WATCH_PROBE="$("$DOCKER_BIN" compose -p irin-desktop-gateway exec -T sidecar \
  sh -c 'printf "producer=%s dispatcher=%s\n" "$WATCH_PRODUCER_ENABLED" "$WATCH_DISPATCHER_ENABLED"' 2>/dev/null || true)"
log "watch_probe=${WATCH_PROBE//$'\n'/ }"
echo "$WATCH_PROBE" | grep -q 'producer=false' || die "WATCH_PRODUCER_ENABLED not false"
echo "$WATCH_PROBE" | grep -q 'dispatcher=false' || die "WATCH_DISPATCHER_ENABLED not false"

# Keychain presence (production accounts) without reading values - fail closed.
if "$SECURITY_BIN" find-generic-password -s "com.irinity.irin" \
  -a "gateway-client-gw-api-key" >/dev/null 2>&1; then
  log "keychain_gw_api_key=present"
else
  die "app Keychain item gateway-client-gw-api-key missing after Enable"
fi
if "$SECURITY_BIN" find-generic-password -s "com.irinity.irin" \
  -a "gateway-pack-auth-pepper" >/dev/null 2>&1; then
  log "keychain_auth_pepper=present"
else
  die "app Keychain item gateway-pack-auth-pepper missing after Enable"
fi

# Council PID must change after governed restart - fail closed.
sleep 2
PID2="$(listen_pid 8765)"
log "post_enable_council_pid=$PID2"
[[ -n "$PID2" ]] || die "Council not listening after enable"
[[ "$PID1" != "$PID2" ]] || die "Council PID unchanged after Enable (governed restart did not occur)"
log "council_pid_changed=true"
OWNED_COUNCIL_PIDS="$OWNED_COUNCIL_PIDS,$PID2"

# d) Governed discovery + fake/unregistered route fails before provider inference.
HEALTH2="$("$CURL_BIN" -fsS --max-time 2 "http://127.0.0.1:8765/api/health")"
"$PYTHON_BIN" - <<'PY' "$HEALTH2"
import json,sys
d=json.loads(sys.argv[1])
avail=d.get("providers_available") or []
assert "gateway" in avail, "governed health must list gateway when key present"
print("providers_has_gateway=true")
print("execution_hint=governed_if_gateway_present")
PY
# Unauthenticated models must fail closed (no provider inference without a key).
CODE="$("$CURL_BIN" -sS -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:18080/v1/models" || true)"
log "models_unauth_http=$CODE"
[[ "$CODE" == "401" || "$CODE" == "403" ]] || die "models not fail-closed"
# Unauthenticated chat with a fake model must also fail closed at the Gateway
# auth/route boundary (never reach a provider). Body is non-secret.
FAKE_CODE="$("$CURL_BIN" -sS -o /dev/null -w '%{http_code}' --max-time 5 \
  -X POST -H 'Content-Type: application/json' \
  -d '{"model":"irin-fake-unregistered-model-xyz","messages":[{"role":"user","content":"ping"}]}' \
  "http://127.0.0.1:18080/v1/chat/completions" || true)"
log "fake_route_unauth_http=$FAKE_CODE"
[[ "$FAKE_CODE" == "401" || "$FAKE_CODE" == "403" || "$FAKE_CODE" == "404" || "$FAKE_CODE" == "400" ]] \
  || die "fake unregistered route did not fail closed before provider inference ($FAKE_CODE)"
log "fake_route_fail_closed=true"
log "step_enable=ok"

# e) Disable - actual button only
"$OSASCRIPT_BIN" -e 'tell application "IRIN" to activate' >/dev/null 2>&1 || true
sleep 0.5
ax_click_button "Settings" >/dev/null 2>&1 || true
wait_ax_button_enabled "Disable" 30 \
  || die "Disable AXButton never became enabled"
ax_click_button_busy "Disable"
# Independent Disable postconditions (authoritative; busy hold is optional).
# 1) Council PID must change (governed -> Direct restart).
# 2) Direct mode restored (via_gateway_default false).
# 3) Governed gateway provider absent from discovery.
ready_disable=0
for i in $(seq 1 60); do
  wait_health || true
  PID3="$(listen_pid 8765)"
  if [[ -n "$PID3" && "$PID2" != "$PID3" ]]; then
    HEALTH3="$("$CURL_BIN" -fsS --max-time 2 "http://127.0.0.1:8765/api/health" || true)"
    PRIV_DIS="$APP_SUPPORT_ROOT/private.json"
    if [[ -n "$HEALTH3" && -f "$PRIV_DIS" ]]; then
      if "$PYTHON_BIN" - <<'PY' "$HEALTH3" "$PRIV_DIS"
import json,sys
h=json.loads(sys.argv[1])
p=json.load(open(sys.argv[2]))
avail=h.get("providers_available") or []
# Direct mode: governed flag off and gateway provider not advertised.
if p.get("via_gateway_default") is not False:
    raise SystemExit(1)
if "gateway" in avail:
    raise SystemExit(2)
print("post_disable_gateway_provider=False")
print("via_gateway_default=False")
PY
      then
        ready_disable=1
        break
      fi
    fi
  fi
  sleep 0.5
done
PID3="$(listen_pid 8765)"
log "post_disable_council_pid=$PID3"
[[ -n "$PID3" ]] || die "Council not healthy after disable"
[[ "$PID2" != "$PID3" ]] || die "Council PID unchanged after Disable (command may not have run)"
[[ "$ready_disable" == 1 ]] || die "Disable postconditions failed (Direct mode / gateway provider still present)"
wait_health || die "Direct health after disable failed"
log "step_disable=ok"
OWNED_COUNCIL_PIDS="$OWNED_COUNCIL_PIDS,$PID3"

# f) Stop pack - actual button only (no compose fallback)
"$OSASCRIPT_BIN" -e 'tell application "IRIN" to activate' >/dev/null 2>&1 || true
sleep 0.5
ax_click_button "Settings" >/dev/null 2>&1 || true
wait_ax_button_enabled "Stop pack" 30 \
  || die "Stop pack AXButton never became enabled"
LIFECYCLE_LOG="$APP_SUPPORT_ROOT/gateway/lifecycle.log"
stop_begin_before="$(grep -c ' stage=stop_begin detail=ok$' "$LIFECYCLE_LOG" 2>/dev/null || true)"
stop_complete_before="$(grep -c ' stage=stop_complete detail=ok$' "$LIFECYCLE_LOG" 2>/dev/null || true)"
stop_handler_before="$(grep -c ' stage=stop_handler_complete detail=ok$' "$LIFECYCLE_LOG" 2>/dev/null || true)"
ax_click_button_busy "Stop pack"
# Prove that the WebView click crossed the native command boundary. Stop is
# intentionally instrumented with categories only; no command output or values.
stop_begin_seen=0
for i in $(seq 1 180); do
  stop_begin_after="$(grep -c ' stage=stop_begin detail=ok$' "$LIFECYCLE_LOG" 2>/dev/null || true)"
  if (( stop_begin_after > stop_begin_before )); then
    stop_begin_seen=1
    break
  fi
  sleep 0.25
done
[[ "$stop_begin_seen" == 1 ]] || die "Stop pack native command did not begin"
log "stop_native_begin=ok"
# Independent Stop postconditions: pack must stop; Direct must stay healthy.
# Project rows may remain stopped; no healthy listener on :18080; no running
# desktop containers. Two native Docker attempts may each consume the bounded
# 45-second command timeout. The outer deadline also covers the Council restart
# and port-release wait, and every loop probe is single-shot so it cannot nest
# another long health deadline.
ready_stop=0
stop_deadline=$((SECONDS + 180))
while (( SECONDS < stop_deadline )); do
  direct_up=0
  if "$CURL_BIN" -fsS --max-time 2 "http://127.0.0.1:8765/api/health" >/dev/null 2>&1; then
    direct_up=1
  fi
  gw_up=0
  if "$CURL_BIN" -fsS --max-time 2 "http://127.0.0.1:18080/health" >/dev/null 2>&1; then
    gw_up=1
  fi
  running_q="$("$DOCKER_BIN" ps -q \
    --filter label=com.docker.compose.project=irin-desktop-gateway \
    --filter status=running 2>/dev/null || true)"
  stop_complete_after="$(grep -c ' stage=stop_complete detail=ok$' "$LIFECYCLE_LOG" 2>/dev/null || true)"
  stop_handler_after="$(grep -c ' stage=stop_handler_complete detail=ok$' "$LIFECYCLE_LOG" 2>/dev/null || true)"
  if [[ "$direct_up" == 1 && "$gw_up" == 0 && -z "$running_q" ]] \
    && (( stop_complete_after > stop_complete_before )) \
    && (( stop_handler_after > stop_handler_before )); then
    ready_stop=1
    break
  fi
  sleep 0.5
done
if [[ "$ready_stop" != 1 ]]; then
  log "stop_timeout_gateway_up=${gw_up:-unknown}"
  log "stop_timeout_direct_up=${direct_up:-unknown}"
  log "stop_timeout_running_containers=$(printf '%s\n' "${running_q:-}" | sed '/^$/d' | wc -l | tr -d ' ')"
  log "stop_timeout_pack_complete_delta=$(( ${stop_complete_after:-0} - stop_complete_before ))"
  log "stop_timeout_handler_complete_delta=$(( ${stop_handler_after:-0} - stop_handler_before ))"
fi
wait_health || die "Direct Council unhealthy after stop"
log "direct_after_stop=ok"
[[ "$ready_stop" == 1 ]] || die "Stop postconditions failed (Gateway still up or containers still running)"
# stop_complete means the pack stop finished. stop_handler_complete, the
# re-enabled button, and the PID transition prove the full Tauri command
# returned after restarting its owned Council child.
wait_ax_button_enabled "Stop pack" 30 \
  || die "Stop pack native command did not return"
PID4="$(listen_pid 8765)"
log "post_stop_council_pid=$PID4"
[[ -n "$PID4" ]] || die "Council not healthy after Stop"
[[ "$PID3" != "$PID4" ]] || die "Council PID unchanged after Stop (native command may not have returned)"
log "gateway_after_stop=down"
log "step_stop=ok"
OWNED_COUNCIL_PIDS="$OWNED_COUNCIL_PIDS,$PID4"

# g) Relaunch continuity (non-secret) - graceful quit first
graceful_quit_app
sleep 1
: >"$ROOT/packaging/build/gw-pack-ui-relaunch.log"
(
  export IRIN_APP_SUPPORT_ROOT="$APP_SUPPORT_ROOT"
  export TMPDIR="$TEST_HOME/tmp"
  unset DOCKER_CONFIG
  env -u DOCKER_CONFIG "$HOST" >>"$ROOT/packaging/build/gw-pack-ui-relaunch.log" 2>&1 &
  echo $! >"$PIDFILE"
)
HOST_PID="$(cat "$PIDFILE")"
log "relaunch_host_pid=$HOST_PID"
wait_health || die "relaunch health failed"
PRIV="$APP_SUPPORT_ROOT/private.json"
[[ -f "$PRIV" ]] || die "private.json missing after relaunch"
"$PYTHON_BIN" - <<'PY' "$PRIV"
import json,sys
d=json.load(open(sys.argv[1]))
assert "install_id" in d
# Never print secrets; only non-secret keys
print("private_keys=", sorted(d.keys()))
print("via_gateway_default=", d.get("via_gateway_default"))
print("has_key_id=", bool(d.get("gateway_key_id")))
PY
if "$SECURITY_BIN" find-generic-password -s "com.irinity.irin" \
  -a "gateway-client-gw-api-key" >/dev/null 2>&1; then
  log "keychain_gw_api_key_after_relaunch=present"
else
  die "Keychain item missing after relaunch"
fi
log "relaunch_continuity=ok"

# h) Uninstall via actual AX only (no compose fallback).
# Note: packBusy is set only AFTER window.confirm accepts - so we click the
# button, confirm the dialog, then require a busy transition / removal proof.
"$OSASCRIPT_BIN" -e 'tell application "IRIN" to activate' >/dev/null 2>&1 || true
sleep 0.5
ax_click_button "Settings" >/dev/null 2>&1 || true
wait_ax_button_enabled "Uninstall pack" 30 \
  || die "Uninstall pack AXButton never became enabled"
UNINSTALL_BEFORE="$(ax_button_enabled "Uninstall pack" || true)"
log "ax_before_Uninstall_pack=$UNINSTALL_BEFORE"
[[ "$UNINSTALL_BEFORE" == "enabled" ]] || die "Uninstall pack button not enabled ($UNINSTALL_BEFORE)"
UNINSTALL_CLICK="$(ax_click_button "Uninstall pack" || true)"
log "ax_click_Uninstall_pack=$UNINSTALL_CLICK"
[[ "$UNINSTALL_CLICK" == clicked_button* ]] || die "could not click Uninstall pack AXButton"
sleep 0.4
# Confirm dialog (WKWebView confirm sheet)
CONFIRM_RC="$(
  "$OSASCRIPT_BIN" 2>/dev/null <<'APPLESCRIPT' || true
tell application "System Events"
  tell process "IRIN"
    -- Prefer explicit confirm buttons
    try
      click button "OK" of window 1
      return "clicked_ok"
    end try
    try
      click button "Uninstall" of window 1
      return "clicked_uninstall"
    end try
    try
      set sheets to every sheet of window 1
      repeat with s in sheets
        try
          click button "OK" of s
          return "clicked_sheet_ok"
        end try
      end repeat
    end try
  end tell
end tell
return "no_dialog"
APPLESCRIPT
)"
log "ax_uninstall_confirm=$CONFIRM_RC"
# Require either confirm click or immediate busy (some stacks auto-accept in test).
busy_seen=0
for i in $(seq 1 40); do
  after="$(ax_button_enabled "Uninstall pack" || true)"
  if [[ "$after" == "disabled" ]]; then
    busy_seen=1
    break
  fi
  # Command ran: gateway data removed or project gone.
  if [[ ! -d "$APP_SUPPORT_ROOT/gateway" ]]; then
    busy_seen=1
    break
  fi
  if ! "$DOCKER_BIN" compose -p irin-desktop-gateway ps -q 2>/dev/null | grep -q .; then
    # May have been stopped earlier; still wait a bit for uninstall path.
    :
  fi
  sleep 0.2
done
log "ax_busy_Uninstall_pack=$busy_seen"
[[ "$busy_seen" == 1 || "$CONFIRM_RC" == clicked_* ]] \
  || die "Uninstall command did not run (no confirm/busy)"
sleep 4

# Foreign survivors - fail closed
"$DOCKER_BIN" compose -p "$FOREIGN_PROJECT" -f "$ROOT/packaging/build/foreign-compose.yml" ps -q | grep -q . \
  || die "foreign project destroyed"
"$DOCKER_BIN" volume inspect "$FOREIGN_VOLUME" >/dev/null || die "foreign volume destroyed"
"$SECURITY_BIN" find-generic-password -s "$FOREIGN_KC_SERVICE" -a "$FOREIGN_KC_ACCOUNT" >/dev/null \
  || die "foreign keychain destroyed"
log "foreign_project_survived=true"
log "foreign_volume_survived=true"
log "foreign_keychain_survived=true"

# Desktop project + app data + keychain must be gone
if "$DOCKER_BIN" compose -p irin-desktop-gateway ps -q 2>/dev/null | grep -q .; then
  die "desktop project still present after Uninstall"
fi
log "desktop_project_removed=true"
if [[ -d "$APP_SUPPORT_ROOT/gateway" ]]; then
  die "app-owned gateway data dir still present after Uninstall"
fi
log "app_gateway_data_removed=true"
if "$SECURITY_BIN" find-generic-password -s "com.irinity.irin" \
  -a "gateway-client-gw-api-key" >/dev/null 2>&1; then
  die "app Keychain item gateway-client-gw-api-key still present after Uninstall"
fi
if "$SECURITY_BIN" find-generic-password -s "com.irinity.irin" \
  -a "gateway-pack-auth-pepper" >/dev/null 2>&1; then
  die "app Keychain item gateway-pack-auth-pepper still present after Uninstall"
fi
log "app_keychain_removed=true"
# Uninstall owns teardown of the project this run created.
OWNED_DESKTOP_TEARDOWN=false

log "owned_council_pids=$OWNED_COUNCIL_PIDS"
log "host_pid_final=$HOST_PID"
FINAL_RESULT="PASS"
FINAL_ERROR=""
log "RESULT=PASS"
printf 'PASS: packaged gateway pack UI smoke\n'
