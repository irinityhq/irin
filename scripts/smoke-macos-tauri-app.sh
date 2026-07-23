#!/usr/bin/env bash
# Build and launch the exact native bundle; optionally prove visible core text.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ "$(uname -s)" == Darwin ]] || {
  printf 'ERROR: native Tauri smoke requires macOS\n' >&2
  exit 1
}
for command in make xcrun codesign env; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'ERROR: missing native-smoke command: %s\n' "$command" >&2
    exit 1
  }
done

if [[ "${IRIN_NATIVE_SKIP_BUILD:-0}" != "1" ]]; then
  make -C "$ROOT/council-rs" warroom-build
fi

app="${IRIN_NATIVE_APP:-$ROOT/council-rs/warroom-tauri/src-tauri/target/release/bundle/macos/Council War Room.app}"
binary="$app/Contents/MacOS/council-warroom-tauri"
[[ -x "$binary" ]] || { printf 'ERROR: native app binary missing: %s\n' "$binary" >&2; exit 1; }
codesign --verify --deep --strict "$app"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-native-smoke.XXXXXX")"
pid=""
cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  rm -rf "$tmp"
  exit "$status"
}
trap cleanup EXIT INT TERM
mkdir -p "$tmp/home" "$ROOT/.irin-receipts"

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
