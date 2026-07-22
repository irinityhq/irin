#!/usr/bin/env bash
# One operator runtime for Council, War Room Web, Gateway, and Tailscale Serve.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -f "$ROOT/.irin-worktree.env" ]]; then
  # shellcheck disable=SC1091
  source "$ROOT/.irin-worktree.env"
fi
if [[ -n "${IRIN_RUNTIME_PATH:-}" ]]; then
  PATH="$IRIN_RUNTIME_PATH"
else
  nvm_path=""
  for nvm_bin in "$HOME"/.nvm/versions/node/*/bin; do
    [[ -d "$nvm_bin" ]] && nvm_path="$nvm_bin${nvm_path:+:$nvm_path}"
  done
  # Operator-managed launchers belong ahead of package-manager leftovers.
  # Keep NVM ahead of system Node without letting an old global npm command
  # shadow a deliberate ~/.local/bin or Homebrew installation.
  PATH="$HOME/.local/bin${nvm_path:+:$nvm_path}:/Applications/Docker.app/Contents/Resources/bin:/opt/homebrew/bin:/usr/local/bin:$HOME/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin"
fi
export PATH

COUNCIL_DIR="$ROOT/council-rs"
WEB_DIR="$COUNCIL_DIR/warroom/web"
GATEWAY_DIR="$ROOT/gateway"

COUNCIL_PORT="${IRIN_COUNCIL_PORT:-8765}"
WEB_PORT="${IRIN_WEB_PORT:-3010}"
GATEWAY_PORT="${IRIN_GATEWAY_PORT:-18080}"
TAILSCALE_HTTPS_PORT="${IRIN_TAILSCALE_HTTPS_PORT:-443}"
RUNTIME_PROFILE="${IRIN_RUNTIME_PROFILE:-canonical}"

CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
STATE_HOME="${XDG_STATE_HOME:-$HOME/.local/state}"
GATEWAY_ENV="${IRIN_GATEWAY_ENV:-$CONFIG_HOME/irin/gateway.env}"
STATE_DIR="${IRIN_RUNTIME_STATE_DIR:-$STATE_HOME/irin/runtime}"
COUNCIL_LOG="$STATE_DIR/council.log"
WEB_LOG="$STATE_DIR/web.log"
SUPERVISOR_LOG="$STATE_DIR/supervisor.log"
CLAUDE_PROXY_LOG="$STATE_DIR/claude-proxy.log"
CODEX_PROXY_LOG="$STATE_DIR/codex-proxy.log"
CONTROL_LOCK_FILE="$STATE_DIR/control.lock"
LAUNCHD_LABEL="${IRIN_RUNTIME_LAUNCHD_LABEL:-com.irinity.irin-runtime}"
# Session serve supervisor (launchctl submit). Survives until logout/stop, not reboot.
SERVE_LAUNCHD_LABEL="${IRIN_RUNTIME_SERVE_LABEL:-${LAUNCHD_LABEL}}"
# Persistent Login LaunchAgent — boots stack at login (survives reboot).
LOGIN_LAUNCHD_LABEL="${IRIN_RUNTIME_LOGIN_LABEL:-com.irinity.irin-runtime.login}"
LAUNCHD_DOMAIN="gui/$(id -u)"
LOGIN_PLIST="${IRIN_RUNTIME_LOGIN_PLIST:-$HOME/Library/LaunchAgents/${LOGIN_LAUNCHD_LABEL}.plist}"
LOGIN_BOOT_LOG="$STATE_DIR/login-boot.log"
CONTROL_LOCK_HELD=0
PARTIAL_START=0
COMPOSE_PROJECT="${IRIN_COMPOSE_PROJECT:-gateway}"
export IRIN_GATEWAY_PORT="$GATEWAY_PORT"

COMPOSE=(
  docker compose
  # Suppress Compose's implicit .env loading. Runtime settings and IRIN-owned
  # credentials are exported explicitly by load_runtime_env; provider settings
  # arrive only through the login shell.
  --env-file /dev/null
  -p "$COMPOSE_PROJECT"
  -f "$GATEWAY_DIR/docker-compose.yml"
  -f "$GATEWAY_DIR/docker-compose.canary.yml"
)

log() { printf '%s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

git_remote() {
  git -C "$ROOT" remote get-url origin 2>/dev/null || true
}

git_sha() {
  git -C "$ROOT" rev-parse HEAD 2>/dev/null || true
}

git_branch() {
  git -C "$ROOT" branch --show-current 2>/dev/null || true
}

source_dirty() {
  [[ -n "$(git -C "$ROOT" status --porcelain --untracked-files=normal 2>/dev/null)" ]]
}

# Tracked changes only — untracked junk must not block login boot.
source_dirty_tracked() {
  [[ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no 2>/dev/null)" ]]
}

assert_source_identity() {
  local remote branch soft="${1:-}"
  git -C "$ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1 \
    || die "runtime source is not a Git checkout: $ROOT"
  remote="$(git_remote)"
  case "$remote" in
    https://github.com/irinityhq/irin.git|git@github.com:irinityhq/irin.git|ssh://git@github.com/irinityhq/irin.git) ;;
    *) die "runtime origin is not irinityhq/irin: ${remote:-<missing>}" ;;
  esac
  branch="$(git_branch)"
  if [[ "$RUNTIME_PROFILE" == "canonical" ]]; then
    [[ "$branch" == "main" ]] || die "canonical runtime must launch from main, not ${branch:-detached HEAD}"
    if [[ "$soft" == "soft" ]]; then
      if source_dirty_tracked; then
        log "WARN: tracked dirty files on main — boot continues (login mode)"
      fi
    else
      ! source_dirty || die "canonical runtime checkout is dirty; commit in a worktree, then update main"
    fi
  elif [[ "$RUNTIME_PROFILE" == "worktree" ]]; then
    [[ "$branch" != "main" && -n "$branch" ]] \
      || die "worktree runtime requires a non-main branch"
    [[ "${IRIN_TAILSCALE_SERVE:-0}" == "0" ]] \
      || die "Tailscale Serve must remain disabled for worktree runtimes"
  else
    die "unknown IRIN_RUNTIME_PROFILE: $RUNTIME_PROFILE"
  fi
}

wait_for_docker() {
  local i max="${IRIN_DOCKER_WAIT_SECS:-180}"
  local steps=$(( (max + 1) / 2 ))
  (( steps >= 1 )) || steps=1
  if docker info >/dev/null 2>&1; then
    log "OK: Docker already ready"
    return 0
  fi
  if [[ -d /Applications/Docker.app ]]; then
    log "Starting Docker Desktop (wait up to ${max}s)…"
    open -a Docker 2>/dev/null || true
  else
    log "Waiting for Docker daemon (up to ${max}s)…"
  fi
  for ((i = 1; i <= steps; i++)); do
    if docker info >/dev/null 2>&1; then
      log "OK: Docker ready"
      return 0
    fi
    sleep 2
  done
  die "Docker not ready after ${max}s — open Docker Desktop and retry"
}

stack_healthy() {
  service_ready "http://127.0.0.1:${COUNCIL_PORT}/api/health" \
    && service_ready "http://127.0.0.1:${WEB_PORT}/" \
    && service_ready "http://127.0.0.1:${GATEWAY_PORT}/health" \
    && service_ready "http://127.0.0.1:${GATEWAY_PORT}/health/sidecar"
}

print_source_identity() {
  local dirty=clean
  source_dirty_tracked && dirty=dirty
  printf 'CHECKOUT profile=%s branch=%s sha=%s tree=%s\n' \
    "$RUNTIME_PROFILE" "$(git_branch)" "$(git_sha)" "$dirty"
  printf 'CHECKOUT root=%s\n' "$ROOT"
  printf 'CHECKOUT origin=%s\n' "$(git_remote)"
}

print_runtime_identity() {
  local receipt="$STATE_DIR/source.json" receipt_root receipt_sha
  if [[ ! -f "$receipt" ]]; then
    printf 'LAST_START source-receipt=missing\n'
    return 0
  fi
  if ! jq -e 'type == "object" and (.root | type == "string") and (.sha | type == "string")' \
    "$receipt" >/dev/null 2>&1; then
    printf 'LAST_START source-receipt=invalid path=%s\n' "$receipt"
    return 0
  fi
  jq -r '"LAST_START profile=\(.profile) branch=\(.branch) sha=\(.sha) tree=" + (if .dirty then "dirty" else "clean" end), "LAST_START root=\(.root)", "LAST_START origin=\(.origin)", "LAST_START compose_project=\(.compose_project)"' "$receipt"
  receipt_root="$(jq -r '.root' "$receipt")"
  receipt_sha="$(jq -r '.sha' "$receipt")"
  if [[ "$receipt_root" != "$ROOT" || "$receipt_sha" != "$(git_sha)" ]]; then
    printf 'SOURCE_MISMATCH checkout and last-start receipt differ\n'
  fi
}

fetch_build_identity() {
  local url="$1" payload
  payload="$(runtime_http_get "$url" 2>/dev/null)" || return 1
  jq -er '
    select(
      type == "object"
      and (.build_sha | type == "string" and test("^[0-9a-f]{40}$"))
      and (.build_dirty | type == "boolean")
    )
    | [.build_sha, (if .build_dirty then "dirty" else "clean" end)]
    | @tsv
  ' <<<"$payload"
}

assert_live_builds_match_checkout() {
  local checkout_sha checkout_tree=clean
  local council_identity sidecar_identity council_sha council_tree sidecar_sha sidecar_tree

  checkout_sha="$(git_sha)"
  source_dirty_tracked && checkout_tree=dirty
  council_identity="$(fetch_build_identity "http://127.0.0.1:${COUNCIL_PORT}/api/health")" \
    || die "Council health does not expose a valid embedded build identity"
  sidecar_identity="$(fetch_build_identity "http://127.0.0.1:${GATEWAY_PORT}/health/sidecar")" \
    || die "Gateway sidecar health does not expose a valid embedded build identity"
  IFS=$'\t' read -r council_sha council_tree <<<"$council_identity"
  IFS=$'\t' read -r sidecar_sha sidecar_tree <<<"$sidecar_identity"

  [[ -n "$checkout_sha" \
    && "$council_sha" == "$checkout_sha" \
    && "$sidecar_sha" == "$checkout_sha" \
    && "$council_tree" == "$checkout_tree" \
    && "$sidecar_tree" == "$checkout_tree" ]] \
    || die "refusing source receipt: checkout=${checkout_sha:-missing}:$checkout_tree council=$council_sha:$council_tree gateway_sidecar=$sidecar_sha:$sidecar_tree"
}

runtime_matches_checkout_and_receipt() {
  local receipt="$STATE_DIR/source.json"
  local checkout_sha checkout_tree=clean receipt_tree
  local council_identity sidecar_identity council_sha council_tree sidecar_sha sidecar_tree

  [[ -f "$receipt" ]] \
    && jq -e 'type == "object" and (.root | type == "string") and (.sha | type == "string") and (.dirty | type == "boolean")' \
      "$receipt" >/dev/null 2>&1 \
    || return 1

  checkout_sha="$(git_sha)"
  source_dirty_tracked && checkout_tree=dirty
  receipt_tree="$(jq -r 'if .dirty then "dirty" else "clean" end' "$receipt")"
  [[ "$(jq -r '.root' "$receipt")" == "$ROOT" \
    && "$(jq -r '.sha' "$receipt")" == "$checkout_sha" \
    && "$receipt_tree" == "$checkout_tree" ]] \
    || return 1

  council_identity="$(fetch_build_identity "http://127.0.0.1:${COUNCIL_PORT}/api/health")" \
    || return 1
  sidecar_identity="$(fetch_build_identity "http://127.0.0.1:${GATEWAY_PORT}/health/sidecar")" \
    || return 1
  IFS=$'\t' read -r council_sha council_tree <<<"$council_identity"
  IFS=$'\t' read -r sidecar_sha sidecar_tree <<<"$sidecar_identity"

  [[ -n "$checkout_sha" \
    && "$council_sha" == "$checkout_sha" \
    && "$sidecar_sha" == "$checkout_sha" \
    && "$council_tree" == "$checkout_tree" \
    && "$sidecar_tree" == "$checkout_tree" ]]
}

runtime_identity_status() {
  local receipt="$STATE_DIR/source.json"
  local checkout_sha checkout_tree last_start_sha=missing last_start_tree=missing
  local council_sha=unavailable council_tree=unavailable
  local sidecar_sha=unavailable sidecar_tree=unavailable
  local council_identity sidecar_identity

  checkout_sha="$(git_sha)"
  checkout_tree=clean
  source_dirty_tracked && checkout_tree=dirty

  if [[ -f "$receipt" ]] \
    && jq -e 'type == "object" and (.sha | type == "string") and (.dirty | type == "boolean")' \
      "$receipt" >/dev/null 2>&1; then
    last_start_sha="$(jq -r '.sha' "$receipt")"
    last_start_tree="$(jq -r 'if .dirty then "dirty" else "clean" end' "$receipt")"
  fi

  if council_identity="$(fetch_build_identity "http://127.0.0.1:${COUNCIL_PORT}/api/health")"; then
    IFS=$'\t' read -r council_sha council_tree <<<"$council_identity"
    printf 'RUNNING Council sha=%s tree=%s\n' "$council_sha" "$council_tree"
  else
    printf 'RUNNING Council identity=unavailable\n'
  fi

  if sidecar_identity="$(fetch_build_identity "http://127.0.0.1:${GATEWAY_PORT}/health/sidecar")"; then
    IFS=$'\t' read -r sidecar_sha sidecar_tree <<<"$sidecar_identity"
    printf 'RUNNING Gateway-sidecar sha=%s tree=%s\n' "$sidecar_sha" "$sidecar_tree"
  else
    printf 'RUNNING Gateway-sidecar identity=unavailable\n'
  fi

  if [[ -n "$checkout_sha" \
    && "$checkout_sha" == "$last_start_sha" \
    && "$checkout_sha" == "$council_sha" \
    && "$checkout_sha" == "$sidecar_sha" \
    && "$checkout_tree" == "$last_start_tree" \
    && "$checkout_tree" == "$council_tree" \
    && "$checkout_tree" == "$sidecar_tree" ]]; then
    printf 'RUNTIME_PARITY ok sha=%s tree=%s\n' "$checkout_sha" "$checkout_tree"
    return 0
  fi

  printf 'RUNTIME_MISMATCH checkout=%s:%s last_start=%s:%s council=%s:%s gateway_sidecar=%s:%s\n' \
    "${checkout_sha:-missing}" "$checkout_tree" \
    "$last_start_sha" "$last_start_tree" \
    "$council_sha" "$council_tree" \
    "$sidecar_sha" "$sidecar_tree"
  return 1
}

write_source_receipt() {
  local dirty=false tmp
  assert_live_builds_match_checkout
  source_dirty_tracked && dirty=true
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"
  tmp="$(mktemp "$STATE_DIR/source.json.XXXXXX")"
  jq -n \
    --arg profile "$RUNTIME_PROFILE" \
    --arg root "$ROOT" \
    --arg origin "$(git_remote)" \
    --arg branch "$(git_branch)" \
    --arg sha "$(git_sha)" \
    --arg compose_project "$COMPOSE_PROJECT" \
    --argjson dirty "$dirty" \
    --argjson council_port "$COUNCIL_PORT" \
    --argjson web_port "$WEB_PORT" \
    --argjson gateway_port "$GATEWAY_PORT" \
    '{profile:$profile,root:$root,origin:$origin,branch:$branch,sha:$sha,dirty:$dirty,compose_project:$compose_project,ports:{council:$council_port,web:$web_port,gateway:$gateway_port}}' \
    >"$tmp"
  chmod 600 "$tmp"
  mv "$tmp" "$STATE_DIR/source.json"
}

http_ready() {
  runtime_http_get "$1" >/dev/null 2>&1
}

runtime_http_get() {
  local url="$1"
  local args=(-fsS --max-time 5)
  if [[ "$url" == "http://127.0.0.1:${COUNCIL_PORT}/"* \
    && -n "${COUNCIL_AUTH_TOKEN:-}" ]]; then
    args+=(--header "Authorization: Bearer ${COUNCIL_AUTH_TOKEN}")
  fi
  curl "${args[@]}" "$url"
}

service_ready() {
  local url="$1"
  for _ in 1 2 3; do
    http_ready "$url" && return 0
    sleep 0.2
  done
  return 1
}

proxy_ready() {
  local url="$1" token="$2"
  [[ -n "$token" ]] || return 1
  curl -fsS --max-time 2 \
    --header "X-Proxy-Auth: Bearer ${token}" \
    "$url" >/dev/null 2>&1
}

wait_for_proxy_pid() {
  local url="$1" token="$2" label="$3" pid="$4" timeout_secs="${5:-20}"
  local deadline=$((SECONDS + timeout_secs))
  while (( SECONDS < deadline )); do
    if proxy_ready "$url" "$token"; then
      log "OK: $label"
      return 0
    fi
    kill -0 "$pid" 2>/dev/null || return 1
    sleep 0.25
  done
  return 1
}

port_open() {
  (exec 3<>"/dev/tcp/127.0.0.1/$1") >/dev/null 2>&1
}

wait_for_url() {
  local url="$1" label="$2" timeout_secs="${3:-20}"
  local deadline=$((SECONDS + timeout_secs))
  while (( SECONDS < deadline )); do
    if http_ready "$url"; then
      log "OK: $label"
      return 0
    fi
    sleep 0.25
  done
  return 1
}

release_control_lock() {
  (( CONTROL_LOCK_HELD == 1 )) || return 0
  exec 9>&-
  CONTROL_LOCK_HELD=0
}

acquire_control_lock() {
  local wait_secs="${IRIN_CONTROL_LOCK_WAIT_SECS:-30}"
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"
  require_command lockf
  [[ "$wait_secs" =~ ^[0-9]+$ ]] \
    || die "IRIN_CONTROL_LOCK_WAIT_SECS must be a non-negative integer"
  exec 9>"$CONTROL_LOCK_FILE"
  chmod 600 "$CONTROL_LOCK_FILE"
  if ! lockf -s -t "$wait_secs" 9; then
    exec 9>&-
    die "another IRIN runtime command is still active after ${wait_secs}s"
  fi
  CONTROL_LOCK_HELD=1
}

shell_owns_provider_setting() {
  case "$1" in
    GW_API_KEY)
      # IRIN's generated Council-to-Gateway credential, not a provider key.
      return 1 ;;
    *_API_KEY|OPENAI_ADMIN_KEY|\
    VERTEX_PROJECT|VERTEX_LOCATION|VERTEX_GEMINI_MODEL|\
    GOOGLE_CLOUD_PROJECT|GOOGLE_CLOUD_LOCATION)
      return 0 ;;
    *) return 1 ;;
  esac
}

# Load IRIN's private runtime configuration. Provider credentials and Vertex
# routing remain owned by the login shell; generated IRIN credentials (for
# example GW_API_KEY and proxy tokens) and runtime settings are authoritative
# in gateway.env so a stale shell export cannot break the local control plane.
source_gateway_runtime_env() {
  local file="$1" line key val
  [[ -f "$file" ]] || return 0
  while IFS= read -r line || [[ -n "$line" ]]; do
    # strip CR, comments, blanks
    line="${line%$'\r'}"
    [[ -z "${line//[[:space:]]/}" ]] && continue
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" == export\ * ]] && line="${line#export }"
    [[ "$line" == *=* ]] || continue
    key="${line%%=*}"
    val="${line#*=}"
    # trim surrounding whitespace/quotes on key only; keep val as-is after strip
    key="${key#"${key%%[![:space:]]*}"}"
    key="${key%"${key##*[![:space:]]}"}"
    [[ -n "$key" ]] || continue
    # strip optional matching single/double quotes around val
    if [[ "$val" == \"*\" && "$val" == *\" ]]; then
      val="${val:1:${#val}-2}"
    elif [[ "$val" == \'*\' && "$val" == *\' ]]; then
      val="${val:1:${#val}-2}"
    fi
    # Provider settings have one durable source: the operator's login shell.
    # Never revive a copied/stale provider value from gateway.env, even when
    # the shell currently has no value for that provider.
    if shell_owns_provider_setting "$key"; then
      continue
    fi
    if [[ -z "$val" \
      || "$val" == __GENERATED_*__ \
      || "$val" == "your-gcp-project" \
      || "$val" == "your-project-id" \
      || "$val" == "change-me" \
      || "$val" == "changeme" ]]; then
      # Empty/template assignment: only set if variable is currently unset.
      if [[ -z "${!key+x}" ]]; then
        export "$key="
      fi
      continue
    fi
    export "$key=$val"
  done <"$file"
}

load_runtime_env() {
  # Provider credentials are inherited only from the login shell. This private
  # file owns IRIN-generated tokens and runtime settings; any legacy provider
  # lines it still contains are ignored.
  source_gateway_runtime_env "$GATEWAY_ENV"
  export COUNCIL_BASE_URL="${IRIN_COUNCIL_BASE_URL:-http://host.docker.internal:${COUNCIL_PORT}}"
  export COUNCIL_CORS_ORIGINS="${COUNCIL_CORS_ORIGINS:-tauri://localhost,https://tauri.localhost,http://localhost:${WEB_PORT},http://127.0.0.1:${WEB_PORT},http://localhost:${COUNCIL_PORT},http://127.0.0.1:${COUNCIL_PORT}}"
}

build_runtime() {
  log "Building Council from $ROOT"
  cargo build --manifest-path "$ROOT/Cargo.toml" --release -p council-rs --bin council

  if [[ ! -d "$WEB_DIR/node_modules" ]]; then
    log "Installing War Room Web dependencies"
    (cd "$WEB_DIR" && npm ci)
  fi
  log "Building the production War Room Web surface"
  (cd "$WEB_DIR" && env \
    NEXT_PUBLIC_API_BASE="http://127.0.0.1:${COUNCIL_PORT}" \
    NEXT_PUBLIC_WS_BASE="ws://127.0.0.1:${COUNCIL_PORT}" \
    NEXT_PUBLIC_GATEWAY_BASE="http://127.0.0.1:${GATEWAY_PORT}" \
    npm run build:hosted)

  log "Building Gateway and Sidecar from the same checkout"
  "${COMPOSE[@]}" build gateway sidecar
}

run_local_stack() {
  local bin="$ROOT/target/release/council"
  local council_pid="" web_pid="" claude_proxy_pid="" codex_proxy_pid=""
  [[ -x "$bin" ]] || die "Council binary missing after build: $bin"
  [[ -x "$WEB_DIR/node_modules/.bin/next" ]] || die "Next binary missing: $WEB_DIR/node_modules/.bin/next"

  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"
  load_runtime_env

  if port_open "$COUNCIL_PORT"; then
    die "Council port ${COUNCIL_PORT} is already occupied"
  fi
  if port_open "$WEB_PORT"; then
    die "Web port ${WEB_PORT} is already occupied"
  fi

  cleanup_children() {
    [[ -z "$codex_proxy_pid" ]] || kill "$codex_proxy_pid" 2>/dev/null || true
    [[ -z "$claude_proxy_pid" ]] || kill "$claude_proxy_pid" 2>/dev/null || true
    [[ -z "$web_pid" ]] || kill "$web_pid" 2>/dev/null || true
    [[ -z "$council_pid" ]] || kill "$council_pid" 2>/dev/null || true
    [[ -z "$codex_proxy_pid" ]] || wait "$codex_proxy_pid" 2>/dev/null || true
    [[ -z "$claude_proxy_pid" ]] || wait "$claude_proxy_pid" 2>/dev/null || true
    [[ -z "$web_pid" ]] || wait "$web_pid" 2>/dev/null || true
    [[ -z "$council_pid" ]] || wait "$council_pid" 2>/dev/null || true
  }
  trap cleanup_children EXIT INT TERM

  : >"$CLAUDE_PROXY_LOG"
  : >"$CODEX_PROXY_LOG"
  if command -v claude >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
    if [[ -z "${CLAUDE_PROXY_TOKEN:-}" ]]; then
      log "WARN: CLAUDE_PROXY_TOKEN missing; Claude models will be unready (rerun make setup)"
    elif port_open 9090; then
      if ! proxy_ready "http://127.0.0.1:9090/v1/models" "$CLAUDE_PROXY_TOKEN"; then
        log "WARN: Claude Gateway adapter unavailable on :9090; Claude models will be unready"
      fi
    else
      (cd "$GATEWAY_DIR" && exec python3 tools/claude-proxy.py --bind 0.0.0.0 --port 9090) \
        >>"$CLAUDE_PROXY_LOG" 2>&1 &
      claude_proxy_pid=$!
    fi
  fi
  if command -v codex >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
    if [[ -z "${CODEX_PROXY_TOKEN:-}" ]]; then
      log "WARN: CODEX_PROXY_TOKEN missing; Codex models will be unready (rerun make setup)"
    elif port_open 9091; then
      if ! proxy_ready "http://127.0.0.1:9091/v1/models" "$CODEX_PROXY_TOKEN"; then
        log "WARN: Codex Gateway adapter unavailable on :9091; Codex models will be unready"
      fi
    else
      (cd "$GATEWAY_DIR" && exec python3 tools/codex-proxy.py --bind 0.0.0.0 --port 9091) \
        >>"$CODEX_PROXY_LOG" 2>&1 &
      codex_proxy_pid=$!
    fi
  fi

  # Start optional adapters together: their zero-spend CLI auth probes can
  # each take several seconds, and serial waits can exhaust the supervisor's
  # readiness window even though both providers are healthy.
  if [[ -n "$claude_proxy_pid" ]] \
    && ! wait_for_proxy_pid "http://127.0.0.1:9090/v1/models" "$CLAUDE_PROXY_TOKEN" \
      "Claude Gateway adapter :9090" "$claude_proxy_pid" 50; then
    kill "$claude_proxy_pid" 2>/dev/null || true
    wait "$claude_proxy_pid" 2>/dev/null || true
    claude_proxy_pid=""
    log "WARN: Claude CLI is unavailable or logged out; Claude models will be unready"
  fi
  if [[ -n "$codex_proxy_pid" ]] \
    && ! wait_for_proxy_pid "http://127.0.0.1:9091/v1/models" "$CODEX_PROXY_TOKEN" \
      "Codex Gateway adapter :9091" "$codex_proxy_pid" 50; then
    kill "$codex_proxy_pid" 2>/dev/null || true
    wait "$codex_proxy_pid" 2>/dev/null || true
    codex_proxy_pid=""
    log "WARN: Codex CLI is unavailable or logged out; Codex models will be unready"
  fi

  : >"$COUNCIL_LOG"
  : >"$WEB_LOG"
  (cd "$COUNCIL_DIR" && exec "$bin" --base-dir "$COUNCIL_DIR" --serve --port "$COUNCIL_PORT") \
    >>"$COUNCIL_LOG" 2>&1 &
  council_pid=$!

  wait_for_url "http://127.0.0.1:${COUNCIL_PORT}/api/health" "Council :${COUNCIL_PORT}" 60 \
    || die "Council failed to start; see $COUNCIL_LOG"

  (cd "$WEB_DIR" && exec env -i \
    HOME="$HOME" \
    LANG="${LANG:-C}" \
    NODE_ENV=production \
    PATH="$PATH" \
    TMPDIR="${TMPDIR:-/tmp}" \
    "$WEB_DIR/node_modules/.bin/next" start \
    --hostname 127.0.0.1 --port "$WEB_PORT") >>"$WEB_LOG" 2>&1 &
  web_pid=$!
  wait_for_url "http://127.0.0.1:${WEB_PORT}/" "War Room Web :${WEB_PORT}" \
    || die "War Room Web failed to start; see $WEB_LOG"

  while kill -0 "$council_pid" 2>/dev/null \
    && kill -0 "$web_pid" 2>/dev/null \
    && { [[ -z "$claude_proxy_pid" ]] || kill -0 "$claude_proxy_pid" 2>/dev/null; } \
    && { [[ -z "$codex_proxy_pid" ]] || kill -0 "$codex_proxy_pid" 2>/dev/null; }; do
    sleep 1
  done
  die "Council, War Room Web, or a managed Gateway adapter exited; see runtime logs in $STATE_DIR"
}

stop_local_job() {
  local serve_job_gone=0
  # Session serve job only — do not touch the persistent login agent.
  launchctl bootout "${LAUNCHD_DOMAIN}/${SERVE_LAUNCHD_LABEL}" >/dev/null 2>&1 \
    || launchctl remove "$SERVE_LAUNCHD_LABEL" >/dev/null 2>&1 \
    || true
  for _ in $(seq 1 50); do
    if launchctl print "${LAUNCHD_DOMAIN}/${SERVE_LAUNCHD_LABEL}" >/dev/null 2>&1; then
      serve_job_gone=0
    else
      serve_job_gone=1
    fi
    if (( serve_job_gone == 1 )) \
      && ! port_open "$COUNCIL_PORT" \
      && ! port_open "$WEB_PORT"; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

start_local_job() {
  local serve_command
  stop_local_job \
    || die "Council or Web did not stop after removing ${SERVE_LAUNCHD_LABEL}"
  if port_open "$COUNCIL_PORT"; then
    die "Council port ${COUNCIL_PORT} is owned by another process. Close the desktop app or stop the old launcher, then retry."
  fi
  if port_open "$WEB_PORT"; then
    die "Web port ${WEB_PORT} is owned by another process. Stop the old dev server, then retry."
  fi
  : >"$SUPERVISOR_LOG"
  printf -v serve_command 'exec %q serve' "$ROOT/scripts/irin-runtime.sh"
  launchctl submit -l "$SERVE_LAUNCHD_LABEL" -o "$SUPERVISOR_LOG" -e "$SUPERVISOR_LOG" -- \
    /bin/zsh -lic "$serve_command"
  wait_for_url "http://127.0.0.1:${COUNCIL_PORT}/api/health" "Council :${COUNCIL_PORT}" 60 \
    || die "Council supervisor failed; see $SUPERVISOR_LOG"
  wait_for_url "http://127.0.0.1:${WEB_PORT}/" "War Room Web :${WEB_PORT}" \
    || die "Web supervisor failed; see $SUPERVISOR_LOG"
}

start_gateway() {
  "${COMPOSE[@]}" up -d
  wait_for_url "http://127.0.0.1:${GATEWAY_PORT}/health" "Gateway :${GATEWAY_PORT}" \
    || die "Gateway failed to become healthy"
  wait_for_url "http://127.0.0.1:${GATEWAY_PORT}/health/sidecar" "Gateway sidecar provenance" \
    || die "Gateway sidecar failed to expose build identity"
}

configure_tailscale() {
  [[ "${IRIN_TAILSCALE_SERVE:-auto}" != "0" ]] || return 0
  command -v tailscale >/dev/null 2>&1 || {
    [[ "${IRIN_TAILSCALE_SERVE:-auto}" == "1" ]] && die "tailscale is required but unavailable"
    log "SKIP: Tailscale Serve unavailable"
    return 0
  }
  tailscale status >/dev/null 2>&1 || {
    [[ "${IRIN_TAILSCALE_SERVE:-auto}" == "1" ]] && die "Tailscale is not connected"
    log "SKIP: Tailscale is not connected"
    return 0
  }

  tailscale serve --yes --bg --https="$TAILSCALE_HTTPS_PORT" "http://127.0.0.1:${WEB_PORT}" >/dev/null
  tailscale serve --yes --bg --https="$TAILSCALE_HTTPS_PORT" --set-path=/api "http://127.0.0.1:${COUNCIL_PORT}/api" >/dev/null
  tailscale serve --yes --bg --https="$TAILSCALE_HTTPS_PORT" --set-path=/ws "http://127.0.0.1:${COUNCIL_PORT}/ws" >/dev/null
  tailscale serve --yes --bg --https="$TAILSCALE_HTTPS_PORT" --set-path=/watch "http://127.0.0.1:${GATEWAY_PORT}/watch" >/dev/null
  tailscale serve --yes --bg --https="$TAILSCALE_HTTPS_PORT" --set-path=/health "http://127.0.0.1:${GATEWAY_PORT}/health" >/dev/null
  log "OK: Tailscale Serve"
}

tailscale_phone_url() {
  local dns_name serve_key serve_json url
  [[ "${IRIN_TAILSCALE_SERVE:-auto}" != "0" ]] || return 1
  command -v tailscale >/dev/null 2>&1 || return 1
  tailscale status >/dev/null 2>&1 || return 1
  dns_name="$(tailscale status --json 2>/dev/null \
    | jq -er '.Self.DNSName // empty' 2>/dev/null)" || return 1
  dns_name="${dns_name%.}"
  [[ -n "$dns_name" ]] || return 1
  serve_key="${dns_name}:${TAILSCALE_HTTPS_PORT}"
  serve_json="$(tailscale serve status --json 2>/dev/null)" || return 1
  jq -e \
    --arg key "$serve_key" \
    --arg web "http://127.0.0.1:${WEB_PORT}" \
    --arg api "http://127.0.0.1:${COUNCIL_PORT}/api" \
    --arg ws "http://127.0.0.1:${COUNCIL_PORT}/ws" \
    --arg watch "http://127.0.0.1:${GATEWAY_PORT}/watch" \
    --arg health "http://127.0.0.1:${GATEWAY_PORT}/health" \
    '.Web[$key].Handlers as $handlers
      | ($handlers["/"].Proxy == $web)
      and ($handlers["/api"].Proxy == $api)
      and ($handlers["/ws"].Proxy == $ws)
      and ($handlers["/watch"].Proxy == $watch)
      and ($handlers["/health"].Proxy == $health)' \
    <<<"$serve_json" >/dev/null 2>&1 || return 1
  url="https://${dns_name}"
  [[ "$TAILSCALE_HTTPS_PORT" == "443" ]] || url+=":${TAILSCALE_HTTPS_PORT}"
  printf '%s\n' "$url"
}

status_line() {
  local label="$1" url="$2" port="$3"
  if service_ready "$url"; then
    printf 'UP    %s\n' "$label"
  elif port_open "$port"; then
    printf 'DEGRADED %s\n' "$label"
  else
    printf 'DOWN  %s\n' "$label"
  fi
}

proxy_status_line() {
  local label="$1" url="$2" token="$3" port="$4"
  if proxy_ready "$url" "$token"; then
    printf 'UP    %s\n' "$label"
  elif port_open "$port"; then
    printf 'DEGRADED %s\n' "$label"
  else
    printf 'DOWN  %s\n' "$label"
  fi
}

runtime_status() {
  local identity_status=0 private_phone_url=""
  load_runtime_env
  print_source_identity
  print_runtime_identity
  runtime_identity_status || identity_status=$?
  status_line "Council http://127.0.0.1:${COUNCIL_PORT}" "http://127.0.0.1:${COUNCIL_PORT}/api/health" "$COUNCIL_PORT"
  status_line "War Room Web http://127.0.0.1:${WEB_PORT}" "http://127.0.0.1:${WEB_PORT}/" "$WEB_PORT"
  status_line "Gateway http://127.0.0.1:${GATEWAY_PORT}" "http://127.0.0.1:${GATEWAY_PORT}/health" "$GATEWAY_PORT"
  if command -v claude >/dev/null 2>&1; then
    proxy_status_line "Claude Gateway adapter http://127.0.0.1:9090" \
      "http://127.0.0.1:9090/v1/models" "${CLAUDE_PROXY_TOKEN:-}" 9090
  fi
  if command -v codex >/dev/null 2>&1; then
    proxy_status_line "Codex Gateway adapter http://127.0.0.1:9091" \
      "http://127.0.0.1:9091/v1/models" "${CODEX_PROXY_TOKEN:-}" 9091
  fi
  if launchctl print "${LAUNCHD_DOMAIN}/${SERVE_LAUNCHD_LABEL}" >/dev/null 2>&1; then
    printf 'OWNER %s (serve)\n' "$SERVE_LAUNCHD_LABEL"
  fi
  if launchctl print "${LAUNCHD_DOMAIN}/${LOGIN_LAUNCHD_LABEL}" >/dev/null 2>&1; then
    printf 'LOGIN %s (RunAtLoad)\n' "$LOGIN_LAUNCHD_LABEL"
  else
    printf 'LOGIN %s missing — run: %s install-login\n' \
      "$LOGIN_LAUNCHD_LABEL" "$ROOT/scripts/irin-runtime.sh"
  fi
  if [[ "${IRIN_TAILSCALE_SERVE:-auto}" != "0" ]] \
    && command -v tailscale >/dev/null 2>&1; then
    private_phone_url="$(tailscale_phone_url || true)"
    if [[ -n "$private_phone_url" ]]; then
      printf 'PRIVATE_PHONE %s\n' "$private_phone_url"
    fi
    tailscale serve status 2>/dev/null | sed -n '1,8p' || true
  fi
  return "$identity_status"
}

runtime_start() {
  assert_source_identity
  require_command cargo
  require_command npm
  require_command docker
  require_command curl
  [[ -f "$GATEWAY_ENV" ]] || die "Gateway config missing: $GATEWAY_ENV"
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"
  wait_for_docker
  load_runtime_env
  build_runtime
  PARTIAL_START=1
  start_local_job
  start_gateway
  configure_tailscale
  write_source_receipt
  runtime_status
  PARTIAL_START=0
}

# Login / post-reboot path: keep matching healthy services fast; otherwise
# rebuild and restart before updating provenance. Soft dirty check is tracked-only.
runtime_boot() {
  assert_source_identity soft
  require_command docker
  require_command curl
  [[ -f "$GATEWAY_ENV" ]] || die "Gateway config missing: $GATEWAY_ENV"
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"
  wait_for_docker
  load_runtime_env

  if stack_healthy && runtime_matches_checkout_and_receipt; then
    log "OK: stack already healthy — re-applying Tailscale Serve"
    configure_tailscale
    write_source_receipt
    runtime_status
    return 0
  fi

  log "Runtime incomplete or provenance drifted — rebuilding from checkout"
  require_command cargo
  require_command npm
  build_runtime
  PARTIAL_START=1
  start_local_job
  start_gateway
  configure_tailscale
  write_source_receipt
  runtime_status
  PARTIAL_START=0
}

xml_escape() {
  printf '%s' "$1" | sed \
    -e 's/&/\&amp;/g' \
    -e 's/</\&lt;/g' \
    -e 's/>/\&gt;/g'
}

write_login_plist() {
  local script_path="$ROOT/scripts/irin-runtime.sh" boot_command
  local label_xml command_xml boot_log_xml
  printf -v boot_command 'exec %q boot' "$script_path"
  label_xml="$(xml_escape "$LOGIN_LAUNCHD_LABEL")"
  command_xml="$(xml_escape "$boot_command")"
  boot_log_xml="$(xml_escape "$LOGIN_BOOT_LOG")"
  mkdir -p "$(dirname "$LOGIN_PLIST")"
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"
  cat >"$LOGIN_PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>${label_xml}</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/zsh</string>
		<string>-lic</string>
		<string>${command_xml}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>StandardOutPath</key>
	<string>${boot_log_xml}</string>
	<key>StandardErrorPath</key>
	<string>${boot_log_xml}</string>
	<key>ProcessType</key>
	<string>Background</string>
	<key>ThrottleInterval</key>
	<integer>30</integer>
</dict>
</plist>
EOF
  chmod 644 "$LOGIN_PLIST"
}

install_login_agent() {
  write_login_plist
  launchctl bootout "${LAUNCHD_DOMAIN}/${LOGIN_LAUNCHD_LABEL}" >/dev/null 2>&1 || true
  # uninstall-login deliberately disables the label. Re-enable it before
  # bootstrap so a later setup/reinstall does not fail with launchd error 5.
  launchctl enable "${LAUNCHD_DOMAIN}/${LOGIN_LAUNCHD_LABEL}" 2>/dev/null \
    || die "launchctl enable failed for ${LOGIN_LAUNCHD_LABEL}"
  launchctl bootstrap "$LAUNCHD_DOMAIN" "$LOGIN_PLIST" \
    || die "launchctl bootstrap failed for $LOGIN_PLIST"
  log "OK: installed login agent $LOGIN_LAUNCHD_LABEL"
  log "     plist: $LOGIN_PLIST"
  log "     boot log: $LOGIN_BOOT_LOG"
  log "     loads at login → irin-runtime.sh boot (waits for Docker, starts stack)"
  if launchctl print "${LAUNCHD_DOMAIN}/${LOGIN_LAUNCHD_LABEL}" >/dev/null 2>&1; then
    log "OK: agent loaded in $LAUNCHD_DOMAIN"
  else
    die "agent not visible after bootstrap"
  fi
}

uninstall_login_agent() {
  launchctl bootout "${LAUNCHD_DOMAIN}/${LOGIN_LAUNCHD_LABEL}" >/dev/null 2>&1 || true
  launchctl disable "${LAUNCHD_DOMAIN}/${LOGIN_LAUNCHD_LABEL}" 2>/dev/null || true
  if [[ -f "$LOGIN_PLIST" ]]; then
    rm -f "$LOGIN_PLIST"
    log "OK: removed $LOGIN_PLIST"
  else
    log "OK: plist already absent"
  fi
  log "OK: login agent uninstalled (manual start still works)"
}

runtime_stop() {
  local local_stopped=0
  if [[ "${IRIN_TAILSCALE_SERVE:-auto}" != "0" ]] \
    && command -v tailscale >/dev/null 2>&1; then
    tailscale serve --yes --https="$TAILSCALE_HTTPS_PORT" off >/dev/null 2>&1 || true
  fi
  if stop_local_job; then
    local_stopped=1
  fi
  if [[ -f "$GATEWAY_ENV" ]]; then
    require_command docker
    "${COMPOSE[@]}" down
  fi
  (( local_stopped == 1 )) \
    || die "Council or Web is still running after removing ${SERVE_LAUNCHD_LABEL}"
  ! port_open "$COUNCIL_PORT" \
    || die "Council is still running on port ${COUNCIL_PORT} after launchd teardown"
  ! port_open "$WEB_PORT" \
    || die "War Room Web is still running on port ${WEB_PORT} after launchd teardown"
  ! port_open "$GATEWAY_PORT" \
    || die "Gateway is still running on port ${GATEWAY_PORT} after Docker teardown"
  runtime_status || true
}

rollback_partial_start() {
  log "Rolling back partial IRIN runtime startup"
  if [[ "${IRIN_TAILSCALE_SERVE:-auto}" != "0" ]] \
    && command -v tailscale >/dev/null 2>&1; then
    tailscale serve --yes --https="$TAILSCALE_HTTPS_PORT" off >/dev/null 2>&1 || true
  fi
  stop_local_job || true
  if [[ -f "$GATEWAY_ENV" ]] && command -v docker >/dev/null 2>&1; then
    "${COMPOSE[@]}" down >/dev/null 2>&1 || true
  fi
}

controller_exit() {
  local status="$1"
  trap - EXIT INT TERM
  if (( PARTIAL_START == 1 && status != 0 )); then
    rollback_partial_start
  fi
  release_control_lock
  exit "$status"
}

run_control() {
  acquire_control_lock
  trap 'controller_exit $?' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  "$@"
}

runtime_restart() {
  runtime_stop
  runtime_start
}

# Reload private environment for Council, War Room, and managed CLI adapters
# without rebuilding source or disturbing the already-running Gateway.
# Used by setup immediately after it provisions the local Gateway client key.
runtime_reload_local_config() {
  assert_source_identity
  require_command curl
  [[ -f "$GATEWAY_ENV" ]] || die "Gateway config missing: $GATEWAY_ENV"
  load_runtime_env
  start_local_job
}

# Recreate only the Gateway edge container after generated, non-provider
# runtime settings change. The sidecar and its persisted auth registry stay up.
runtime_reload_gateway_config() {
  assert_source_identity
  require_command docker
  require_command curl
  [[ -f "$GATEWAY_ENV" ]] || die "Gateway config missing: $GATEWAY_ENV"
  load_runtime_env
  "${COMPOSE[@]}" up -d gateway
  wait_for_url "http://127.0.0.1:${GATEWAY_PORT}/health" "Gateway :${GATEWAY_PORT}" \
    || die "Gateway failed after local configuration reload"
}

case "${1:-}" in
  start) run_control runtime_start ;;
  stop) run_control runtime_stop ;;
  restart) run_control runtime_restart ;;
  reload-gateway-config) run_control runtime_reload_gateway_config ;;
  reload-local-config) run_control runtime_reload_local_config ;;
  boot) run_control runtime_boot ;;
  install-login) install_login_agent ;;
  uninstall-login) uninstall_login_agent ;;
  status) runtime_status ;;
  serve) run_local_stack ;;
  *)
    die "usage: $0 {start|stop|restart|reload-gateway-config|reload-local-config|boot|install-login|uninstall-login|status}"
    ;;
esac
