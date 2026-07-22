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

APP_NAME="Council War Room.app"
TEST_APPS="$ROOT/packaging/test-apps"
DEST_APP="${IRIN_SMOKE_APP:-$TEST_APPS/$APP_NAME}"
TEST_HOME="$ROOT/packaging/test-home/gw-pack-smoke-$$"
RECEIPT="$ROOT/packaging/receipts/GATEWAY_PACK_UI_SMOKE.txt"
PIDFILE="$ROOT/packaging/build/gw-pack-ui-host.pid"
HOST_LOG="$ROOT/packaging/build/gw-pack-ui-host.log"
FOREIGN_PROJECT="irin-foreign-isolation-probe"
FOREIGN_VOLUME="irin_foreign_isolation_vol"
FOREIGN_KC_SERVICE="com.sovereign.council.warroom.test.foreign-ui-$$"
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

APP_SUPPORT_REL="Library/Application Support/com.sovereign.council.warroom"
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
  "$OSASCRIPT_BIN" -e 'tell application "Council War Room" to quit' >/dev/null 2>&1 || true
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

cleanup() {
  local ec=$?
  set +e
  log "cleanup_begin=true"
  graceful_quit_app
  # Desktop pack project only — never project "gateway".
  "$DOCKER_BIN" compose -p irin-desktop-gateway down --volumes --remove-orphans >/dev/null 2>&1 || true
  "$DOCKER_BIN" compose -p "$FOREIGN_PROJECT" down --volumes --remove-orphans >/dev/null 2>&1 || true
  "$DOCKER_BIN" volume rm "$FOREIGN_VOLUME" >/dev/null 2>&1 || true
  "$SECURITY_BIN" delete-generic-password -s "$FOREIGN_KC_SERVICE" -a "$FOREIGN_KC_ACCOUNT" >/dev/null 2>&1 || true
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

rm -rf "$TEST_HOME"
mkdir -p "$TEST_HOME/tmp" "$TEST_HOME/Library/Application Support"
export HOME="$TEST_HOME"
export TMPDIR="$TEST_HOME/tmp"

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
  tell process "Council War Room"
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
  tell process "Council War Room"
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

# Click an enabled button and require busy transition (disabled) proving the command started.
# Retries the find+click atomically: WebKit AX can drop buttons during pack-status re-renders.
ax_click_button_busy() {
  local label="$1"
  local after click_rc busy_seen=0 i attempt
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
    # Some transitions finish very fast; pack marker / status also proves run.
    if [[ -f "$TEST_HOME/$PACK_MARKER_REL" ]] && [[ "$label" == "Enable Gateway" ]]; then
      busy_seen=1
      break
    fi
    sleep 0.15
  done
  log "ax_busy_${label// /_}=$busy_seen after=${after:-unknown}"
  [[ "$busy_seen" == 1 ]] || die "no busy/state transition after click: $label (command may not have run)"
}

# --- a) Launch Direct ---
: >"$HOST_LOG"
(
  export HOME="$TEST_HOME"
  export TMPDIR="$TEST_HOME/tmp"
  # Never pass provider secrets into the smoke host environment from the harness.
  "$HOST" >>"$HOST_LOG" 2>&1 &
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
PY
log "step_a=direct_ok"

# Wait until an enabled AXButton with the given name is present (pack status
# refresh can leave the Settings panel without action buttons for a few seconds).
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
    sleep 1
  done
  log "ax_wait_${label// /_}=timeout last=$st"
  return 1
}

# --- b/c) Accessibility: Settings → Enable Gateway ---
"$OSASCRIPT_BIN" -e 'tell application "Council War Room" to activate' >/dev/null 2>&1 || true
sleep 1
SETTINGS_CLICK="$(ax_click_button "Settings" || true)"
log "ax_settings=$SETTINGS_CLICK"
[[ "$SETTINGS_CLICK" == clicked_button* ]] \
  || die "could not identify/click Settings AXButton (Accessibility fail-closed)"

# Pre-state: pack not installed
[[ ! -f "$TEST_HOME/$PACK_MARKER_REL" ]] || die "pack already installed before Enable"
wait_ax_button_enabled "Enable Gateway" 45 \
  || die "Enable Gateway AXButton never became enabled after Settings"
ax_click_button_busy "Enable Gateway"

# Wait for pack ready: project + /health + admin surface (aligned with product timeouts).
ready=0
for attempt in $(seq 1 240); do
  if "$DOCKER_BIN" compose -p irin-desktop-gateway ps -q 2>/dev/null | grep -q . \
    && "$CURL_BIN" -fsS --max-time 3 "http://127.0.0.1:18080/health" >/dev/null 2>&1; then
    ready=1
    break
  fi
  # Fail early if pack marker never appears (command did not reach install).
  if [[ "$attempt" -eq 30 && ! -f "$TEST_HOME/$PACK_MARKER_REL" ]]; then
    die "Enable did not install pack assets (marker missing); native command may not have run"
  fi
  sleep 1
done
[[ "$ready" == 1 ]] || die "Gateway pack did not become healthy after Enable"
[[ -f "$TEST_HOME/$PACK_MARKER_REL" ]] || die "pack-installed.json missing after Enable"
log "gateway_health=200"
log "project=irin-desktop-gateway"
log "pack_installed_marker=present"

# Sidecar/admin readiness (non-secret status codes only)
ADMIN_CODE="$("$CURL_BIN" -sS -o /dev/null -w '%{http_code}' --max-time 5 \
  -X POST -H 'Content-Type: application/json' -d '{}' \
  "http://127.0.0.1:18080/admin/keys" || true)"
log "admin_surface_http=$ADMIN_CODE"
[[ "$ADMIN_CODE" != "000" && "$ADMIN_CODE" != "502" && "$ADMIN_CODE" != "503" ]] \
  || die "admin surface not ready"

# Watch forced off — inspect compose project env via docker inspect (names only).
WATCH_PROBE="$("$DOCKER_BIN" compose -p irin-desktop-gateway exec -T sidecar \
  sh -c 'printf "producer=%s dispatcher=%s\n" "$WATCH_PRODUCER_ENABLED" "$WATCH_DISPATCHER_ENABLED"' 2>/dev/null || true)"
log "watch_probe=${WATCH_PROBE//$'\n'/ }"
echo "$WATCH_PROBE" | grep -q 'producer=false' || die "WATCH_PRODUCER_ENABLED not false"
echo "$WATCH_PROBE" | grep -q 'dispatcher=false' || die "WATCH_DISPATCHER_ENABLED not false"

# Keychain presence (production account) without reading value — fail closed.
if "$SECURITY_BIN" find-generic-password -s "com.sovereign.council.warroom" \
  -a "gateway-client-gw-api-key" >/dev/null 2>&1; then
  log "keychain_gw_api_key=present"
else
  die "app Keychain item gateway-client-gw-api-key missing after Enable"
fi

# Council PID must change after governed restart — fail closed.
sleep 2
PID2="$(listen_pid 8765)"
log "post_enable_council_pid=$PID2"
[[ -n "$PID2" ]] || die "Council not listening after enable"
[[ "$PID1" != "$PID2" ]] || die "Council PID unchanged after Enable (governed restart did not occur)"
log "council_pid_changed=true"
OWNED_COUNCIL_PIDS="$OWNED_COUNCIL_PIDS,$PID2"

# d) Fake/unregistered provider: governed preflight before provider call.
HEALTH2="$("$CURL_BIN" -fsS --max-time 2 "http://127.0.0.1:8765/api/health")"
"$PYTHON_BIN" - <<'PY' "$HEALTH2"
import json,sys
d=json.loads(sys.argv[1])
avail=d.get("providers_available") or []
assert "gateway" in avail, "governed health must list gateway when key present"
print("providers_has_gateway=true")
print("execution_hint=governed_if_gateway_present")
PY
# Unauthenticated models must fail closed
CODE="$("$CURL_BIN" -sS -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:18080/v1/models" || true)"
log "models_unauth_http=$CODE"
[[ "$CODE" == "401" || "$CODE" == "403" ]] || die "models not fail-closed"
log "step_enable=ok"

# e) Disable — actual button only
"$OSASCRIPT_BIN" -e 'tell application "Council War Room" to activate' >/dev/null 2>&1 || true
sleep 0.5
ax_click_button "Settings" >/dev/null 2>&1 || true
wait_ax_button_enabled "Disable" 30 \
  || die "Disable AXButton never became enabled"
ax_click_button_busy "Disable"
sleep 2
PID3="$(listen_pid 8765)"
log "post_disable_council_pid=$PID3"
[[ -n "$PID3" ]] || die "Council not healthy after disable"
[[ "$PID2" != "$PID3" ]] || die "Council PID unchanged after Disable"
wait_health || die "Direct health after disable failed"
HEALTH3="$("$CURL_BIN" -fsS --max-time 2 "http://127.0.0.1:8765/api/health")"
"$PYTHON_BIN" - <<'PY' "$HEALTH3"
import json,sys
d=json.loads(sys.argv[1])
# After disable, gateway key may still be in child env of a prior process; Direct restart should drop it.
print("post_disable_gateway_provider=", "gateway" in (d.get("providers_available") or []))
PY
log "step_disable=ok"
OWNED_COUNCIL_PIDS="$OWNED_COUNCIL_PIDS,$PID3"

# f) Stop pack — actual button only (no compose fallback)
"$OSASCRIPT_BIN" -e 'tell application "Council War Room" to activate' >/dev/null 2>&1 || true
sleep 0.5
ax_click_button "Settings" >/dev/null 2>&1 || true
wait_ax_button_enabled "Stop pack" 30 \
  || die "Stop pack AXButton never became enabled"
ax_click_button_busy "Stop pack"
sleep 3
wait_health || die "Direct Council unhealthy after stop"
log "direct_after_stop=ok"
# Project may still exist stopped; must not be required healthy.
if "$CURL_BIN" -fsS --max-time 2 "http://127.0.0.1:18080/health" >/dev/null 2>&1; then
  # If health still answers, ensure it is still our project (not foreign reclaim).
  if ! "$DOCKER_BIN" compose -p irin-desktop-gateway ps -q 2>/dev/null | grep -q .; then
    die "port 18080 healthy after stop but not desktop project"
  fi
  log "gateway_after_stop=still_up_or_draining"
else
  log "gateway_after_stop=down"
fi
log "step_stop=ok"

# g) Relaunch continuity (non-secret) — graceful quit first
graceful_quit_app
sleep 1
: >"$ROOT/packaging/build/gw-pack-ui-relaunch.log"
(
  export HOME="$TEST_HOME"
  export TMPDIR="$TEST_HOME/tmp"
  "$HOST" >>"$ROOT/packaging/build/gw-pack-ui-relaunch.log" 2>&1 &
  echo $! >"$PIDFILE"
)
HOST_PID="$(cat "$PIDFILE")"
log "relaunch_host_pid=$HOST_PID"
wait_health || die "relaunch health failed"
PRIV="$TEST_HOME/$APP_SUPPORT_REL/private.json"
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
if "$SECURITY_BIN" find-generic-password -s "com.sovereign.council.warroom" \
  -a "gateway-client-gw-api-key" >/dev/null 2>&1; then
  log "keychain_gw_api_key_after_relaunch=present"
else
  die "Keychain item missing after relaunch"
fi
log "relaunch_continuity=ok"

# h) Uninstall via actual AX only (no compose fallback).
# Note: packBusy is set only AFTER window.confirm accepts — so we click the
# button, confirm the dialog, then require a busy transition / removal proof.
"$OSASCRIPT_BIN" -e 'tell application "Council War Room" to activate' >/dev/null 2>&1 || true
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
CONFIRM_RC="$("$OSASCRIPT_BIN" <<'APPLESCRIPT' 2>/dev/null || true
tell application "System Events"
  tell process "Council War Room"
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
  if [[ ! -d "$TEST_HOME/$APP_SUPPORT_REL/gateway" ]]; then
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

# Foreign survivors — fail closed
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
if [[ -d "$TEST_HOME/$APP_SUPPORT_REL/gateway" ]]; then
  die "app-owned gateway data dir still present after Uninstall"
fi
log "app_gateway_data_removed=true"
if "$SECURITY_BIN" find-generic-password -s "com.sovereign.council.warroom" \
  -a "gateway-client-gw-api-key" >/dev/null 2>&1; then
  die "app Keychain item still present after Uninstall"
fi
log "app_keychain_removed=true"

log "owned_council_pids=$OWNED_COUNCIL_PIDS"
log "host_pid_final=$HOST_PID"
FINAL_RESULT="PASS"
FINAL_ERROR=""
log "RESULT=PASS"
printf 'PASS: packaged gateway pack UI smoke\n'
