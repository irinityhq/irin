#!/usr/bin/env bash
# Temporary optional launcher for the existing Claude/Codex HTTP proxies only.
#
# Scope boundary (PR2 ownership-only):
#   - may start/stop/status the two Python proxy processes
#   - must NOT spawn, stop, inspect, or adopt Council
#   - must NOT manage Next.js, login LaunchAgent, MatchingBuild, or Settings
#   - not required for installed-app cold start
#
# Permanent governed transport migrates in PR 2a. Delete this shim after that.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATEWAY_DIR="$ROOT/gateway"
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
STATE_HOME="${XDG_STATE_HOME:-$HOME/.local/state}"
GATEWAY_ENV="${IRIN_GATEWAY_ENV:-$CONFIG_HOME/irin/gateway.env}"
STATE_DIR="${IRIN_CLI_PROXY_STATE_DIR:-$STATE_HOME/irin/cli-proxies}"
CLAUDE_PROXY_LOG="$STATE_DIR/claude-proxy.log"
CODEX_PROXY_LOG="$STATE_DIR/codex-proxy.log"
CLAUDE_PID_FILE="$STATE_DIR/claude-proxy.pid"
CODEX_PID_FILE="$STATE_DIR/codex-proxy.pid"
CLAUDE_PORT="${IRIN_CLAUDE_PROXY_PORT:-9090}"
CODEX_PORT="${IRIN_CODEX_PROXY_PORT:-9091}"

log() { printf '%s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

port_open() {
  python3 -c 'import socket,sys; s=socket.socket(); s.settimeout(0.2); raise SystemExit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1])))==0 else 1)' "$1" 2>/dev/null
}

proxy_ready() {
  local url="$1" token="$2"
  [[ -n "$token" ]] || return 1
  curl -fsS --max-time 2 -H "X-Proxy-Auth: ${token}" "$url" >/dev/null 2>&1
}

load_tokens() {
  if [[ -f "$GATEWAY_ENV" ]]; then
    # shellcheck disable=SC1090
    set -a
    # Load only the two proxy tokens; never print values.
    while IFS= read -r line || [[ -n "$line" ]]; do
      case "$line" in
        CLAUDE_PROXY_TOKEN=*|CODEX_PROXY_TOKEN=*) export "$line" ;;
      esac
    done < "$GATEWAY_ENV"
    set +a
  fi
  CLAUDE_PROXY_TOKEN="${CLAUDE_PROXY_TOKEN:-}"
  CODEX_PROXY_TOKEN="${CODEX_PROXY_TOKEN:-}"
}

pid_alive() {
  local pid="$1"
  [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null
}

read_pid() {
  local file="$1"
  [[ -f "$file" ]] || { printf ''; return; }
  tr -d ' \n' <"$file"
}

stop_pid_file() {
  local file="$1" label="$2"
  local pid
  pid="$(read_pid "$file")"
  if pid_alive "$pid"; then
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 30); do
      pid_alive "$pid" || break
      sleep 0.1
    done
    if pid_alive "$pid"; then
      kill -9 "$pid" 2>/dev/null || true
    fi
    log "stopped $label (pid $pid)"
  fi
  rm -f "$file"
}

cmd_start() {
  load_tokens
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"
  require_python=0
  if command -v claude >/dev/null 2>&1 || command -v codex >/dev/null 2>&1; then
    require_python=1
  fi
  if (( require_python == 1 )) && ! command -v python3 >/dev/null 2>&1; then
    die "python3 is required to launch CLI proxies"
  fi

  if command -v claude >/dev/null 2>&1; then
    if [[ -z "$CLAUDE_PROXY_TOKEN" ]]; then
      log "WARN: CLAUDE_PROXY_TOKEN missing; Claude proxy not started (prepare gateway/tools/prepare-local-config.sh)"
    elif port_open "$CLAUDE_PORT"; then
      if proxy_ready "http://127.0.0.1:${CLAUDE_PORT}/v1/models" "$CLAUDE_PROXY_TOKEN"; then
        log "Claude proxy already healthy on :${CLAUDE_PORT}"
      else
        die "port ${CLAUDE_PORT} occupied by a non-ready process"
      fi
    else
      : >"$CLAUDE_PROXY_LOG"
      (cd "$GATEWAY_DIR" && exec env CLAUDE_PROXY_TOKEN="$CLAUDE_PROXY_TOKEN" \
        python3 tools/claude-proxy.py --bind 0.0.0.0 --port "$CLAUDE_PORT") \
        >>"$CLAUDE_PROXY_LOG" 2>&1 &
      echo $! >"$CLAUDE_PID_FILE"
      ready=0
      for _ in $(seq 1 50); do
        if proxy_ready "http://127.0.0.1:${CLAUDE_PORT}/v1/models" "$CLAUDE_PROXY_TOKEN"; then
          ready=1
          break
        fi
        sleep 0.2
      done
      if (( ready == 1 )); then
        log "Claude proxy started on :${CLAUDE_PORT}"
      else
        stop_pid_file "$CLAUDE_PID_FILE" "Claude proxy"
        log "WARN: Claude proxy failed readiness; see $CLAUDE_PROXY_LOG"
      fi
    fi
  else
    log "Claude CLI not installed; skipping Claude proxy"
  fi

  if command -v codex >/dev/null 2>&1; then
    if [[ -z "$CODEX_PROXY_TOKEN" ]]; then
      log "WARN: CODEX_PROXY_TOKEN missing; Codex proxy not started (prepare gateway/tools/prepare-local-config.sh)"
    elif port_open "$CODEX_PORT"; then
      if proxy_ready "http://127.0.0.1:${CODEX_PORT}/v1/models" "$CODEX_PROXY_TOKEN"; then
        log "Codex proxy already healthy on :${CODEX_PORT}"
      else
        die "port ${CODEX_PORT} occupied by a non-ready process"
      fi
    else
      : >"$CODEX_PROXY_LOG"
      (cd "$GATEWAY_DIR" && exec env CODEX_PROXY_TOKEN="$CODEX_PROXY_TOKEN" \
        python3 tools/codex-proxy.py --bind 0.0.0.0 --port "$CODEX_PORT") \
        >>"$CODEX_PROXY_LOG" 2>&1 &
      echo $! >"$CODEX_PID_FILE"
      ready=0
      for _ in $(seq 1 50); do
        if proxy_ready "http://127.0.0.1:${CODEX_PORT}/v1/models" "$CODEX_PROXY_TOKEN"; then
          ready=1
          break
        fi
        sleep 0.2
      done
      if (( ready == 1 )); then
        log "Codex proxy started on :${CODEX_PORT}"
      else
        stop_pid_file "$CODEX_PID_FILE" "Codex proxy"
        log "WARN: Codex proxy failed readiness; see $CODEX_PROXY_LOG"
      fi
    fi
  else
    log "Codex CLI not installed; skipping Codex proxy"
  fi
}

cmd_stop() {
  stop_pid_file "$CLAUDE_PID_FILE" "Claude proxy"
  stop_pid_file "$CODEX_PID_FILE" "Codex proxy"
}

cmd_status() {
  load_tokens
  if command -v claude >/dev/null 2>&1; then
    if proxy_ready "http://127.0.0.1:${CLAUDE_PORT}/v1/models" "${CLAUDE_PROXY_TOKEN:-}"; then
      log "Claude proxy :${CLAUDE_PORT}: ready"
    else
      log "Claude proxy :${CLAUDE_PORT}: not ready"
    fi
  fi
  if command -v codex >/dev/null 2>&1; then
    if proxy_ready "http://127.0.0.1:${CODEX_PORT}/v1/models" "${CODEX_PROXY_TOKEN:-}"; then
      log "Codex proxy :${CODEX_PORT}: ready"
    else
      log "Codex proxy :${CODEX_PORT}: not ready"
    fi
  fi
}

usage() {
  cat <<'EOF'
usage: governed-cli-proxies.sh {start|stop|status}

Temporary optional launcher for Claude/Codex host proxies only.
Does not own Council, Next.js, login recovery, MatchingBuild, or Settings.
EOF
}

case "${1:-}" in
  start) cmd_start ;;
  stop) cmd_stop ;;
  status) cmd_status ;;
  -h|--help|help) usage ;;
  *) usage >&2; exit 2 ;;
esac
