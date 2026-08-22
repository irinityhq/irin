#!/usr/bin/env bash
# Build and launch the exact native bundle; prove app-owned Council spawn.
# Prefer packaging/smoke-full-app.sh for full packaged ownership proof; this
# harness covers the shared app-bundle primitive with an isolated smoke-inert
# Gateway fixture (non-promotable by construction).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ "$(uname -s)" == Darwin ]] || {
  printf 'ERROR: native Tauri smoke requires macOS\n' >&2
  exit 1
}
for command in make xcrun codesign env curl open pgrep python3; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'ERROR: missing native-smoke command: %s\n' "$command" >&2
    exit 1
  }
done

if [[ -f "$ROOT/.irin-worktree.env" ]]; then
  set -a
  # Generated worktree routing only; this file contains no operator secrets.
  . "$ROOT/.irin-worktree.env"
  set +a
fi
if [[ -z "${IRIN_COUNCIL_PORT:-}" ]]; then
  IRIN_COUNCIL_PORT="$(python3 -c '
import socket
while True:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    if port != 8765:
        print(port)
        break
')"
  export IRIN_COUNCIL_PORT
fi
[[ "$IRIN_COUNCIL_PORT" =~ ^[0-9]+$ && "$IRIN_COUNCIL_PORT" -gt 0 && "$IRIN_COUNCIL_PORT" -le 65535 ]] || {
  printf 'ERROR: invalid isolated Council port: %s\n' "$IRIN_COUNCIL_PORT" >&2
  exit 1
}
[[ "$IRIN_COUNCIL_PORT" != 8765 ]] || {
  printf 'ERROR: native smoke refuses the canonical Council port 8765\n' >&2
  exit 1
}

tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-native-smoke.XXXXXX")"
pid=""
launcher_pid=""
binary_pattern=""
# Mirror the Council listener (tokio TcpListener::bind sets SO_REUSEADDR): a
# port left only with TIME_WAIT sockets from a previous launch is free for the
# app, and must read as free here. A live listener still fails this bind.
port_is_free() {
  python3 -c '
import socket, sys
with socket.socket() as sock:
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        sock.bind(("127.0.0.1", int(sys.argv[1])))
    except OSError:
        raise SystemExit(1)
' "$IRIN_COUNCIL_PORT"
}
port_is_released() {
  python3 -c '
import socket, sys
with socket.socket() as sock:
    sock.settimeout(0.2)
    raise SystemExit(0 if sock.connect_ex(("127.0.0.1", int(sys.argv[1]))) != 0 else 1)
' "$IRIN_COUNCIL_PORT"
}
wait_for_port_release() {
  for _ in $(seq 1 30); do
    port_is_released && return 0
    sleep 0.2
  done
  return 1
}
listen_pid() {
  lsof -nP -iTCP:"$IRIN_COUNCIL_PORT" -sTCP:LISTEN -t 2>/dev/null | head -1 || true
}
# True when the isolated port is held by this smoke's bundled Council sidecar.
# Compare against the realpath app root (worktree target may be a symlink into
# the shared cargo cache while the process argv shows the realpath).
is_our_bundled_council_listener() {
  local listen_pid path
  listen_pid="$(listen_pid)"
  [[ -n "$listen_pid" && -n "$app_real" ]] || return 1
  path="$(ps -p "$listen_pid" -o args= 2>/dev/null || true)"
  [[ "$path" == *"$app_real"*"/Contents/MacOS/council"* ]]
}
# Graceful host stop so Tauri RunEvent::Exit can kill the tracked Council child.
# Smoke builds use a unique bundle id (com.irinity.irin.smoke$PORT); quit by id.
# Does not kill the sidecar — ownership proof requires the host to reclaim it.
stop_app_host() {
  local host_pid="${1:-}"
  local bundle_id="com.irinity.irin.smoke${IRIN_COUNCIL_PORT}"
  osascript -e "tell application id \"$bundle_id\" to quit" >/dev/null 2>&1 || true
  if [[ -n "$host_pid" ]] && kill -0 "$host_pid" 2>/dev/null; then
    local i
    # Give the application-targeted quit time to run Tauri's Exit handler
    # before falling back to TERM. Sending TERM immediately races and can
    # orphan the app-owned Council child.
    for i in $(seq 1 40); do
      if ! kill -0 "$host_pid" 2>/dev/null; then
        break
      fi
      sleep 0.25
    done
    if kill -0 "$host_pid" 2>/dev/null; then
      kill -TERM "$host_pid" 2>/dev/null || true
      for i in $(seq 1 40); do
        if ! kill -0 "$host_pid" 2>/dev/null; then
          break
        fi
        sleep 0.25
      done
      if kill -0 "$host_pid" 2>/dev/null; then
        kill -KILL "$host_pid" 2>/dev/null || true
      fi
    fi
  fi
  # Give Exit handlers time to reclaim the owned sidecar after host death.
  sleep 0.5
  wait_for_port_release
}
# Harness-only reclaim of this smoke's bundled council on the isolated port.
reclaim_our_bundled_council() {
  local i sidecar_pid
  for i in $(seq 1 20); do
    port_is_released && return 0
    if is_our_bundled_council_listener; then
      sidecar_pid="$(listen_pid)"
      kill -TERM "$sidecar_pid" 2>/dev/null || true
      sleep 0.25
      if kill -0 "$sidecar_pid" 2>/dev/null; then
        kill -KILL "$sidecar_pid" 2>/dev/null || true
      fi
    else
      return 1
    fi
    sleep 0.2
  done
  port_is_released
}
app=""
app_real=""
cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [[ -n "$launcher_pid" ]] && kill -0 "$launcher_pid" 2>/dev/null; then
    kill "$launcher_pid" 2>/dev/null || true
    wait "$launcher_pid" 2>/dev/null || true
  fi
  if [[ -z "$pid" && -n "$binary_pattern" ]]; then
    pid="$(pgrep -f -x "$binary_pattern" | head -n 1 || true)"
  fi
  stop_app_host "${pid:-}" || true
  pid=""
  reclaim_our_bundled_council || true
  if ! wait_for_port_release; then
    printf 'ERROR: isolated Council port remained occupied after harness cleanup: %s\n' \
      "$IRIN_COUNCIL_PORT" >&2
    status=1
  fi
  if [[ "$status" -ne 0 ]]; then
    mkdir -p "$ROOT/.irin-receipts"
    receipt_prefix="$ROOT/.irin-receipts/native-smoke-failure-$(date '+%Y%m%dT%H%M%S')"
    [[ ! -f "$tmp/app.log" ]] || cp "$tmp/app.log" "${receipt_prefix}-app.log"
    printf 'native smoke failure logs: %s-*.log\n' "$receipt_prefix" >&2
  fi
  rm -rf "$tmp"
  exit "$status"
}
trap cleanup EXIT INT TERM

# The production bundle keeps an exact loopback CSP. This test-only overlay
# adds only the single isolated Council port selected for this smoke build.
smoke_tauri_config="$tmp/tauri.smoke.conf.json"
python3 - "$smoke_tauri_config" "$IRIN_COUNCIL_PORT" <<'PY'
import json
import sys

path, port = sys.argv[1:]
csp = (
    "default-src 'self' tauri://localhost https://tauri.localhost; "
    "connect-src 'self' tauri://localhost https://tauri.localhost "
    f"http://127.0.0.1:{port} ws://127.0.0.1:{port} "
    f"http://localhost:{port} ws://localhost:{port} "
    "http://127.0.0.1:18080 http://localhost:18080 "
    "http://127.0.0.1:8080 http://localhost:8080 "
    "ipc: http://ipc.localhost; "
    "img-src 'self' asset: https://asset.localhost blob: data:; "
    "style-src 'self' 'unsafe-inline'; font-src 'self' data:; "
    "script-src 'self' 'unsafe-inline'; object-src 'none'; "
    "base-uri 'none'; frame-ancestors 'none'"
)
with open(path, "w", encoding="utf-8") as handle:
    json.dump(
        {
            "identifier": f"com.irinity.irin.smoke{port}",
            "app": {"security": {"csp": csp}},
        },
        handle,
    )
PY

smoke_target_dir="${IRIN_NATIVE_TARGET_DIR:-$tmp/cargo-target}"
mkdir -p "$smoke_target_dir"
gateway_stage="$ROOT/council-rs/warroom-tauri/src-tauri/resources/gateway-pack"

if [[ "${IRIN_NATIVE_SKIP_BUILD:-0}" != "1" ]]; then
  # Isolated Cargo target + exclusive app-bundle lock + smoke-inert Gateway.
  # Primitive scrub + this harness cleanup must not leave inert content in the
  # shared production staging tree.
  IRIN_APP_TARGET_DIR="$smoke_target_dir" \
  IRIN_TAURI_CONFIG_OVERLAY="$smoke_tauri_config" \
  IRIN_GATEWAY_PACK_MODE=smoke-inert \
  IRIN_TAURI_BUNDLES=app \
    bash "$ROOT/packaging/build-app-bundle.sh"
  # Explicit ad-hoc sign + verify (consumer-owned; primitive does not sign).
  app="$smoke_target_dir/release/bundle/macos/IRIN.app"
  [[ -d "$app" ]] || {
    printf 'ERROR: smoke app bundle missing after primitive: %s\n' "$app" >&2
    exit 1
  }
  codesign --force --deep --sign - "$app"
  codesign --verify --deep --strict "$app"
else
  app="${IRIN_NATIVE_APP:-$smoke_target_dir/release/bundle/macos/IRIN.app}"
fi

# Drop any leftover smoke-inert staging from the shared tree (defense in depth
# beyond the primitive's EXIT scrub).
if [[ -f "$gateway_stage/SMOKE_INERT" ]] \
  || { [[ -f "$gateway_stage/STAGED_MODE.txt" ]] \
    && grep -q 'mode=smoke-inert' "$gateway_stage/STAGED_MODE.txt" 2>/dev/null; }; then
  rm -rf "$gateway_stage"
fi

app="${IRIN_NATIVE_APP:-$app}"
binary="$app/Contents/MacOS/council-warroom-tauri"
[[ -x "$binary" ]] || { printf 'ERROR: native app binary missing: %s\n' "$binary" >&2; exit 1; }
# Resolve through worktree target symlinks so pgrep matches the LaunchServices
# process path (realpath under the shared cargo cache).
binary="$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$binary")"
app_real="$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$app")"
[[ -x "$binary" ]] || { printf 'ERROR: resolved native app binary missing: %s\n' "$binary" >&2; exit 1; }
binary_pattern="$(printf '%s\n' "$binary" | sed 's/[][\\.^$*+?{}()|]/\\&/g')"
codesign --verify --deep --strict "$app"
# Non-promotable: unique smoke bundle id must be present.
bundle_plist="$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$bundle_plist" 2>/dev/null \
  | grep -Fq "com.irinity.irin.smoke${IRIN_COUNCIL_PORT}" \
  || {
    printf 'ERROR: smoke app missing isolated bundle id com.irinity.irin.smoke%s\n' \
      "$IRIN_COUNCIL_PORT" >&2
    exit 1
  }
if [[ "${IRIN_NATIVE_SKIP_BUILD:-0}" != "1" ]]; then
  # Fresh smoke builds must embed the inert fixture; shared staging was scrubbed.
  grep -Rql 'smoke-inert' "$app" 2>/dev/null \
    || {
      printf 'ERROR: smoke app missing smoke-inert marker (fixture not bundled)\n' >&2
      exit 1
    }
fi

# Packaged ownership requires the staged bundled Council binary.
bundled_council="$app/Contents/MacOS/council"
[[ -x "$bundled_council" ]] || {
  printf 'ERROR: packaged Council sidecar missing from bundle: %s\n' "$bundled_council" >&2
  exit 1
}

mkdir -p "$tmp/home" "$ROOT/.irin-receipts"

port_is_free || {
  printf 'ERROR: isolated Council port is already occupied: %s\n' "$IRIN_COUNCIL_PORT" >&2
  exit 1
}

if pgrep -f -x "$binary_pattern" >/dev/null 2>&1; then
  printf 'ERROR: exact native app is already running: %s\n' "$binary" >&2
  exit 1
fi

# Launch the bundle through LaunchServices. The packaged app must own and spawn
# its bundled Council; MatchingBuild adoption is not supported.
env \
  -u ANTHROPIC_API_KEY -u DEEPSEEK_API_KEY -u GEMINI_API_KEY -u GOOGLE_API_KEY \
  -u GROQ_API_KEY -u MISTRAL_API_KEY -u NVIDIA_API_KEY -u NOUS_API_KEY \
  -u OPENAI_API_KEY -u OPENROUTER_API_KEY -u TOGETHER_API_KEY -u XAI_API_KEY \
  open -n -F -W -o "$tmp/app.log" --stderr "$tmp/app.log" \
    --env "HOME=$tmp/home" \
    --env "IRIN_ISOLATED_KEYCHAIN=$tmp/home/Library/Keychains/irin-smoke.keychain-db" \
    --env COUNCIL_WS_SMOKE_ONLY=1 \
    "$app" &
launcher_pid=$!

stable=0
for _ in $(seq 1 "${IRIN_NATIVE_PROCESS_CHECKS:-20}"); do
  pid="$(pgrep -f -x "$binary_pattern" | head -n 1 || true)"
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    stable=$((stable + 1))
    (( stable >= 3 )) && break
  elif ! kill -0 "$launcher_pid" 2>/dev/null; then
    printf 'ERROR: LaunchServices exited before the native app appeared\n' >&2
    tail -n 40 "$tmp/app.log" >&2 || true
    exit 1
  else
    stable=0
  fi
  sleep 0.5
done
(( stable >= 3 )) || { printf 'ERROR: native Tauri process did not remain stable\n' >&2; exit 1; }
printf 'native process proof: PASS (pid %s)\n' "$pid"

hydrated=0
for _ in $(seq 1 30); do
  if grep -Fq "[runtime-config] selected Council port: $IRIN_COUNCIL_PORT" "$tmp/app.log"; then
    hydrated=1
    break
  fi
  sleep 0.2
done
[[ "$hydrated" == 1 ]] || {
  printf 'ERROR: native webview did not request the selected Council port\n' >&2
  tail -n 40 "$tmp/app.log" >&2 || true
  exit 1
}
printf 'native runtime hydration proof: PASS (port %s)\n' "$IRIN_COUNCIL_PORT"

owned=0
for _ in $(seq 1 120); do
  if grep -Fq "council --serve started on :$IRIN_COUNCIL_PORT" "$tmp/app.log"; then
    owned=1
    break
  fi
  sleep 0.25
done
[[ "$owned" == 1 ]] || {
  printf 'ERROR: native app did not spawn an app-owned Council process\n' >&2
  tail -n 80 "$tmp/app.log" >&2 || true
  exit 1
}
printf 'native app-owned Council spawn proof: PASS (port %s)\n' "$IRIN_COUNCIL_PORT"

# Council health must answer on the isolated port from the app-owned child.
health=""
for _ in $(seq 1 "${IRIN_NATIVE_HEALTH_CHECKS:-60}"); do
  health="$(curl -fsS --max-time 2 \
    "http://127.0.0.1:${IRIN_COUNCIL_PORT}/api/health" 2>/dev/null || true)"
  [[ -n "$health" ]] && break
  sleep 0.5
done
[[ -n "$health" ]] || {
  printf 'ERROR: app-owned Council did not become healthy on isolated port %s\n' \
    "$IRIN_COUNCIL_PORT" >&2
  tail -n 40 "$tmp/app.log" >&2 || true
  exit 1
}
printf 'native Council health proof: PASS (port %s)\n' "$IRIN_COUNCIL_PORT"

webview_ready=0
for _ in $(seq 1 120); do
  if grep -Fq \
    "[runtime-config] webview Council requests ready on :$IRIN_COUNCIL_PORT" \
    "$tmp/app.log"; then
    webview_ready=1
    break
  fi
  sleep 0.25
done
[[ "$webview_ready" == 1 ]] || {
  printf 'ERROR: native webview did not complete Council health and cabinets requests\n' >&2
  tail -n 40 "$tmp/app.log" >&2 || true
  exit 1
}
printf 'native webview Council request proof: PASS (port %s)\n' \
  "$IRIN_COUNCIL_PORT"

# The shell keeps running after a failed cold-launch Keychain preload (the
# #0033 blank-window lead); the gate must not. The line lands in the failure
# receipt via app.log. Checked after the webview has finished its Council
# requests, so the setup task that runs the preload has completed.
if grep -Fq "cold-launch secret preload failed" "$tmp/app.log"; then
  printf 'ERROR: cold-launch secret preload failed (see app.log receipt)\n' >&2
  grep -F "cold-launch secret preload failed" "$tmp/app.log" >&2
  exit 1
fi

if [[ "${IRIN_NATIVE_VISUAL:-1}" == "1" ]]; then
  proof_bin="$tmp/window-proof"
  xcrun swiftc "$ROOT/scripts/macos-window-proof.swift" -o "$proof_bin"
  image="$ROOT/.irin-receipts/native-window-$(date '+%Y%m%dT%H%M%S').png"
  visual=0
  for _ in $(seq 1 "${IRIN_NATIVE_WINDOW_CHECKS:-30}"); do
    if "$proof_bin" --pid "$pid" --output "$image" \
      --contains Discover --contains Deliberate --contains Settings >/dev/null 2>&1; then
      visual=1
      break
    fi
    sleep 1
  done
  if [[ "$visual" != 1 ]]; then
    "$proof_bin" --pid "$pid" --output "$image" \
      --contains Discover --contains Deliberate --contains Settings
    exit 1
  fi
  printf 'native visual proof: PASS (%s)\n' "$image"
else
  printf 'native visual proof: SKIPPED (headless CI process lane)\n'
fi

# Closing the app must terminate only its owned Council child and free the port.
# Use graceful quit so Tauri RunEvent::Exit runs stop_tracked_council_server
# (bare SIGKILL/fast TERM can reparent the sidecar under launchd/PID 1).
# Ownership proof does not harness-kill the sidecar — port release must come
# from the app's Exit path.
host_pid_for_stop="$pid"
if ! stop_app_host "$host_pid_for_stop"; then
  printf 'ERROR: app-owned Council port did not release after app exit: %s\n' \
    "$IRIN_COUNCIL_PORT" >&2
  exit 1
fi
pid=""
printf 'app-owned Council shutdown proof: PASS (port %s released)\n' "$IRIN_COUNCIL_PORT"
printf 'harness teardown proof: PASS\n'
