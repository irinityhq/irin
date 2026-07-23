#!/usr/bin/env bash
# Build and launch the exact native bundle; optionally prove visible core text.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ "$(uname -s)" == Darwin ]] || {
  printf 'ERROR: native Tauri smoke requires macOS\n' >&2
  exit 1
}
for command in make xcrun codesign env curl python3; do
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

if [[ "${IRIN_NATIVE_SKIP_BUILD:-0}" != "1" ]]; then
  make -C "$ROOT/council-rs" warroom-build
fi

app="${IRIN_NATIVE_APP:-$ROOT/council-rs/warroom-tauri/src-tauri/target/release/bundle/macos/Council War Room.app}"
binary="$app/Contents/MacOS/council-warroom-tauri"
[[ -x "$binary" ]] || { printf 'ERROR: native app binary missing: %s\n' "$binary" >&2; exit 1; }
codesign --verify --deep --strict "$app"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-native-smoke.XXXXXX")"
pid=""
council_pid=""
port_is_free() {
  python3 -c '
import socket, sys
with socket.socket() as sock:
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
cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  if [[ -n "$council_pid" ]] && kill -0 "$council_pid" 2>/dev/null; then
    kill "$council_pid" 2>/dev/null || true
    wait "$council_pid" 2>/dev/null || true
  fi
  released=0
  for _ in $(seq 1 30); do
    if port_is_released; then
      released=1
      break
    fi
    sleep 0.2
  done
  if [[ "$released" != 1 ]]; then
    printf 'ERROR: native Council sidecar survived app teardown on port %s\n' \
      "$IRIN_COUNCIL_PORT" >&2
    status=1
  fi
  rm -rf "$tmp"
  exit "$status"
}
trap cleanup EXIT INT TERM
mkdir -p "$tmp/home" "$ROOT/.irin-receipts"

port_is_free || {
  printf 'ERROR: isolated Council port is already occupied: %s\n' "$IRIN_COUNCIL_PORT" >&2
  exit 1
}
expected_sha="$(git -C "$ROOT" rev-parse HEAD)"
if [[ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]]; then
  expected_dirty=true
else
  expected_dirty=false
fi
council_binary="$ROOT/target/release/council"
[[ -x "$council_binary" ]] || {
  printf 'ERROR: exact Council release binary missing: %s\n' "$council_binary" >&2
  exit 1
}
env \
  -u ANTHROPIC_API_KEY -u DEEPSEEK_API_KEY -u GEMINI_API_KEY -u GOOGLE_API_KEY \
  -u GROQ_API_KEY -u MISTRAL_API_KEY -u NVIDIA_API_KEY -u NOUS_API_KEY \
  -u OPENAI_API_KEY -u OPENROUTER_API_KEY -u TOGETHER_API_KEY -u XAI_API_KEY \
  PATH=/usr/bin:/bin COUNCIL_DEV_NO_AUTH=1 COUNCIL_WS_SMOKE_ONLY=1 \
  "$council_binary" \
    --base-dir "$ROOT/council-rs" --serve --port "$IRIN_COUNCIL_PORT" \
    >"$tmp/council.log" 2>&1 &
council_pid=$!

env \
  -u ANTHROPIC_API_KEY -u DEEPSEEK_API_KEY -u GEMINI_API_KEY -u GOOGLE_API_KEY \
  -u GROQ_API_KEY -u MISTRAL_API_KEY -u NVIDIA_API_KEY -u NOUS_API_KEY \
  -u OPENAI_API_KEY -u OPENROUTER_API_KEY -u TOGETHER_API_KEY -u XAI_API_KEY \
  HOME="$tmp/home" COUNCIL_RS_DIR="$ROOT/council-rs" \
  COUNCIL_DEV_NO_AUTH=1 COUNCIL_WS_SMOKE_ONLY=1 \
  "$binary" >"$tmp/app.log" 2>&1 &
pid=$!

stable=0
for _ in $(seq 1 "${IRIN_NATIVE_PROCESS_CHECKS:-20}"); do
  if kill -0 "$pid" 2>/dev/null; then
    stable=$((stable + 1))
    (( stable >= 3 )) && break
  else
    printf 'ERROR: native Tauri process exited during launch\n' >&2
    tail -n 40 "$tmp/app.log" >&2 || true
    exit 1
  fi
  sleep 0.5
done
(( stable >= 3 )) || { printf 'ERROR: native Tauri process did not remain stable\n' >&2; exit 1; }
printf 'native process proof: PASS (pid %s)\n' "$pid"

health=""
for _ in $(seq 1 "${IRIN_NATIVE_HEALTH_CHECKS:-60}"); do
  health="$(curl -fsS --max-time 2 \
    "http://127.0.0.1:${IRIN_COUNCIL_PORT}/api/health" 2>/dev/null || true)"
  [[ -n "$health" ]] && break
  sleep 0.5
done
[[ -n "$health" ]] || {
  printf 'ERROR: native Council sidecar did not become healthy on isolated port %s\n' \
    "$IRIN_COUNCIL_PORT" >&2
  tail -n 40 "$tmp/app.log" >&2 || true
  tail -n 40 "$tmp/council.log" >&2 || true
  exit 1
}
python3 -c '
import json, sys
health = json.loads(sys.argv[1])
expected_sha = sys.argv[2]
expected_dirty = sys.argv[3] == "true"
assert health.get("council_version"), health
assert health.get("stream_version"), health
assert health.get("ws_smoke_only") is True, health
assert health.get("build_sha") == expected_sha, health
assert health.get("build_dirty") is expected_dirty, health
' "$health" "$expected_sha" "$expected_dirty"
printf 'native Council exact-build health: PASS (port %s, sha %s, dirty=%s)\n' \
  "$IRIN_COUNCIL_PORT" "$expected_sha" "$expected_dirty"

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
