#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

FAKE_BIN="$TMP/bin"
STATE_HOME="$TMP/state"
STATE_DIR="$STATE_HOME/irin/runtime"
mkdir -p "$FAKE_BIN" "$STATE_DIR"
GATEWAY_ENV="$TMP/gateway.env"
COUNCIL_TOKEN="test-council-token"
printf 'COUNCIL_AUTH_TOKEN=%s\n' "$COUNCIL_TOKEN" >"$GATEWAY_ENV"
ACTION_LOG="$TMP/actions.log"
REBUILT_MARKER="$TMP/rebuilt"
: >"$ACTION_LOG"
mkdir -p "$TMP/home"

CHECKOUT_SHA="$(git -C "$ROOT" rev-parse HEAD)"
STALE_SHA="1111111111111111111111111111111111111111"

printf '%s\n' \
  '#!/usr/bin/env bash' \
  'if [[ -n "${EXPECTED_OPENAI_API_KEY:-}" && "${OPENAI_API_KEY:-}" != "$EXPECTED_OPENAI_API_KEY" ]]; then exit 97; fi' \
  'if [[ -n "${EXPECTED_VERTEX_PROJECT:-}" && "${VERTEX_PROJECT:-}" != "$EXPECTED_VERTEX_PROJECT" ]]; then exit 98; fi' \
  'if [[ -n "${EXPECTED_XAI_API_KEY:-}" && "${XAI_API_KEY:-}" != "$EXPECTED_XAI_API_KEY" ]]; then exit 99; fi' \
  'if [[ "${EXPECT_SHELL_OWNED_ABSENT:-0}" == 1 && ( -n "${XAI_API_KEY+x}" || -n "${NOUS_API_KEY+x}" || -n "${VERTEX_PROJECT+x}" ) ]]; then exit 95; fi' \
  'if [[ -n "${EXPECTED_GW_API_KEY:-}" && "${GW_API_KEY:-}" != "$EXPECTED_GW_API_KEY" ]]; then exit 96; fi' \
  'url="${!#}"' \
  'if [[ -n "${FAKE_REBUILT_MARKER:-}" && -f "$FAKE_REBUILT_MARKER" ]]; then' \
  '  sha="${FAKE_CHECKOUT_SHA:?}"' \
  'else' \
  '  sha="${FAKE_BUILD_SHA:?}"' \
  'fi' \
  'dirty="${FAKE_BUILD_DIRTY:-false}"' \
  'if [[ "${FAKE_STACK_DOWN_UNTIL_REBUILD:-0}" == 1 && ! -f "${FAKE_REBUILT_MARKER:?}" ]]; then exit 22; fi' \
  'case "$url" in' \
  '  */api/health)' \
  '    expected="${EXPECTED_COUNCIL_TOKEN:-}"' \
  '    if [[ -n "$expected" && " $* " != *" Authorization: Bearer $expected "* ]]; then exit 22; fi' \
  '    printf '\''{"status":"ok","build_sha":"%s","build_dirty":%s}\n'\'' "$sha" "$dirty" ;;' \
  '  */health/sidecar)' \
  '    printf '\''{"status":"ok","build_sha":"%s","build_dirty":%s}\n'\'' "$sha" "$dirty" ;;' \
  '  *) printf '\''%s\n'\'' '\''{"status":"ok"}'\'' ;;' \
  'esac' >"$FAKE_BIN/curl"
chmod +x "$FAKE_BIN/curl"

printf '%s\n' \
  '#!/usr/bin/env bash' \
  'case "$*" in' \
  '  *"rev-parse --is-inside-work-tree"*) printf '\''true\n'\'' ;;' \
  "  *\"rev-parse HEAD\"*) printf '%s\\n' '$CHECKOUT_SHA' ;;" \
  '  *"status --porcelain --untracked-files=no") [[ "${FAKE_TRACKED_DIRTY:-0}" != 1 ]] || printf '\'' M tracked-file\n'\'' ;;' \
  '  *"status --porcelain"*)' \
  '    if [[ "${FAKE_TRACKED_DIRTY:-0}" == 1 ]]; then printf '\'' M tracked-file\n'\''' \
  '    elif [[ "${FAKE_UNTRACKED_ONLY:-0}" == 1 ]]; then printf '\''?? local-only\n'\''; fi ;;' \
  '  *"branch --show-current"*) printf '\''main\n'\'' ;;' \
  '  *"remote get-url origin"*) printf '\''https://github.com/irinityhq/irin.git\n'\'' ;;' \
  '  *) exec /usr/bin/git "$@" ;;' \
  'esac' >"$FAKE_BIN/git"
chmod +x "$FAKE_BIN/git"

printf '%s\n' \
  '#!/usr/bin/env bash' \
  'printf '\''launchctl %s\n'\'' "$*" >>"${FAKE_ACTION_LOG:?}"' \
  'if [[ "${1:-}" == enable && "${FAKE_LAUNCHCTL_ENABLE_FAIL:-0}" == 1 ]]; then exit 42; fi' \
  'if [[ "${1:-}" == print && -n "${FAKE_LAUNCHCTL_PRINT_SUCCESSES:-}" ]]; then' \
  '  count_file="${FAKE_LAUNCHCTL_PRINT_COUNT_FILE:?}"' \
  '  count="$(cat "$count_file" 2>/dev/null || printf 0)"' \
  '  count=$((count + 1))' \
  '  printf '\''%s\n'\'' "$count" >"$count_file"' \
  '  if (( count <= FAKE_LAUNCHCTL_PRINT_SUCCESSES )); then exit 0; else exit 1; fi' \
  'fi' \
  'if [[ "${1:-}" == print && "${FAKE_LAUNCHCTL_PRINT_OK:-0}" != 1 ]]; then exit 1; fi' \
  'exit 0' >"$FAKE_BIN/launchctl"
chmod +x "$FAKE_BIN/launchctl"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'printf '\''docker %s\n'\'' "$*" >>"${FAKE_ACTION_LOG:?}"' \
  'if [[ "${EXPECT_COMPOSE_PROVIDER_ABSENT:-0}" == 1 && " $* " == *" compose "* ]]; then' \
  '  [[ " $* " == *" --env-file /dev/null "* ]] || exit 94' \
  '  [[ " $* " != *" ${IRIN_GATEWAY_ENV:?} "* ]] || exit 93' \
  '  [[ -z "${XAI_API_KEY+x}" && -z "${NOUS_API_KEY+x}" && -z "${VERTEX_PROJECT+x}" ]] || exit 92' \
  '  [[ "${GW_API_KEY:-}" == "${EXPECTED_COMPOSE_GW_API_KEY:?}" ]] || exit 91' \
  'fi' \
  'if [[ " $* " == *" build gateway sidecar "* ]]; then : >"${FAKE_REBUILT_MARKER:?}"; fi' \
  'exit 0' >"$FAKE_BIN/docker"
chmod +x "$FAKE_BIN/docker"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'printf '\''cargo %s\n'\'' "$*" >>"${FAKE_ACTION_LOG:?}"' \
  '[[ "${FAKE_BUILD_FAIL:-0}" != 1 ]] || exit 42' \
  ': >"${FAKE_REBUILT_MARKER:?}"' >"$FAKE_BIN/cargo"
chmod +x "$FAKE_BIN/cargo"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'printf '\''npm %s\n'\'' "$*" >>"${FAKE_ACTION_LOG:?}"' >"$FAKE_BIN/npm"
chmod +x "$FAKE_BIN/npm"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'case "$*" in' \
  '  "status --json") printf '\''{"Self":{"DNSName":"phone.example.ts.net."}}\n'\'' ;;' \
  '  "serve status --json")' \
  '    if [[ -n "${FAKE_TAILSCALE_SERVE_JSON:-}" ]]; then printf '\''%s\n'\'' "$FAKE_TAILSCALE_SERVE_JSON"; else printf '\''{}\n'\''; fi ;;' \
  '  "serve status") printf '\''Tailscale Serve configured\n'\'' ;;' \
  '  "status") exit 0 ;;' \
  '  *) exit 2 ;;' \
  'esac' >"$FAKE_BIN/tailscale"
chmod +x "$FAKE_BIN/tailscale"

jq -n \
  --arg root "$ROOT" \
  --arg sha "$CHECKOUT_SHA" \
  '{profile:"canonical",root:$root,origin:"https://github.com/irinityhq/irin.git",branch:"main",sha:$sha,dirty:false,compose_project:"gateway"}' \
  >"$STATE_DIR/source.json"

set +e
OUTPUT="$(
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_TAILSCALE_SERVE=0 \
  FAKE_BUILD_SHA="$STALE_SHA" \
  bash "$ROOT/scripts/irin-runtime.sh" status 2>&1
)"
STATUS=$?
set -e

[[ "$STATUS" -ne 0 ]] || {
  printf 'expected stale runtime status to fail\n%s\n' "$OUTPUT" >&2
  exit 1
}
grep -Fq "RUNNING Council sha=$STALE_SHA tree=clean" <<<"$OUTPUT"
grep -Fq "RUNNING Gateway-sidecar sha=$STALE_SHA tree=clean" <<<"$OUTPUT"
grep -Fq "RUNTIME_MISMATCH" <<<"$OUTPUT"
if grep -Fq "RUNTIME_PARITY ok" <<<"$OUTPUT"; then
  printf 'stale live binaries were represented as current\n%s\n' "$OUTPUT" >&2
  exit 1
fi

printf 'runtime-status stale identity: PASS\n'

set +e
PARITY_OUTPUT="$(
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_TAILSCALE_SERVE=0 \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  FAKE_BUILD_SHA="$CHECKOUT_SHA" \
  EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
  bash "$ROOT/scripts/irin-runtime.sh" status 2>&1
)"
PARITY_STATUS=$?
set -e
[[ "$PARITY_STATUS" -eq 0 ]] || {
  printf 'expected matching runtime status to pass\n%s\n' "$PARITY_OUTPUT" >&2
  exit 1
}
grep -Fq "RUNTIME_PARITY ok sha=$CHECKOUT_SHA tree=clean" <<<"$PARITY_OUTPUT"
if grep -Fq "RUNTIME_MISMATCH" <<<"$PARITY_OUTPUT"; then
  printf 'matching live binaries were represented as mismatched\n%s\n' "$PARITY_OUTPUT" >&2
  exit 1
fi

printf 'runtime-status matching identity: PASS\n'

# Empty template assignments in either private env file must not clobber a
# provider exported by the user's login shell.
LEGACY_PROVIDER_ENV="$TMP/legacy-providers.env"
printf 'OPENAI_API_KEY=\n' >"$LEGACY_PROVIDER_ENV"
printf 'XAI_API_KEY=stale-copied-value\n' >>"$LEGACY_PROVIDER_ENV"
printf 'OPENAI_API_KEY=\n' >>"$GATEWAY_ENV"
printf 'XAI_API_KEY=stale-gateway-value\n' >>"$GATEWAY_ENV"
printf 'NOUS_API_KEY=stale-gateway-value\n' >>"$GATEWAY_ENV"
printf 'VERTEX_PROJECT=your-gcp-project\n' >>"$GATEWAY_ENV"
printf 'GW_API_KEY=stored-current-gateway-key\n' >>"$GATEWAY_ENV"
set +e
EMPTY_KEY_OUTPUT="$(
  OPENAI_API_KEY='login-shell-test-value' \
  EXPECTED_OPENAI_API_KEY='login-shell-test-value' \
  XAI_API_KEY='login-shell-current-value' \
  EXPECTED_XAI_API_KEY='login-shell-current-value' \
  GW_API_KEY='stale-shell-gateway-key' \
  EXPECTED_GW_API_KEY='stored-current-gateway-key' \
  VERTEX_PROJECT='login-shell-real-project' \
  EXPECTED_VERTEX_PROJECT='login-shell-real-project' \
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_TAILSCALE_SERVE=0 \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  IRIN_PROVIDER_ENV="$LEGACY_PROVIDER_ENV" \
  FAKE_BUILD_SHA="$CHECKOUT_SHA" \
  EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
  bash "$ROOT/scripts/irin-runtime.sh" status 2>&1
)"
EMPTY_KEY_STATUS=$?
set -e
[[ "$EMPTY_KEY_STATUS" -eq 0 ]] || {
  printf 'empty private env value clobbered a login-shell provider export\n%s\n' "$EMPTY_KEY_OUTPUT" >&2
  exit 1
}
grep -Fq "RUNTIME_PARITY ok sha=$CHECKOUT_SHA tree=clean" <<<"$EMPTY_KEY_OUTPUT"
printf 'runtime separates shell-owned providers from IRIN credentials: PASS\n'

# A copied provider value in gateway.env must never become a fallback source.
# If the login shell does not export it, the runtime must leave it absent.
set +e
SHELL_ONLY_OUTPUT="$(
  env -u XAI_API_KEY -u OPENAI_API_KEY -u ANTHROPIC_API_KEY \
    -u NVIDIA_API_KEY -u NOUS_API_KEY -u VERTEX_PROJECT -u VERTEX_LOCATION \
    -u VERTEX_GEMINI_MODEL -u GOOGLE_CLOUD_PROJECT \
    EXPECT_SHELL_OWNED_ABSENT=1 \
    XDG_STATE_HOME="$STATE_HOME" \
    IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
    IRIN_TAILSCALE_SERVE=0 \
    IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
    FAKE_BUILD_SHA="$CHECKOUT_SHA" \
    EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
    bash "$ROOT/scripts/irin-runtime.sh" status 2>&1
)"
SHELL_ONLY_STATUS=$?
set -e
[[ "$SHELL_ONLY_STATUS" -eq 0 ]] || {
  printf 'runtime revived a copied provider setting outside the login shell\n%s\n' \
    "$SHELL_ONLY_OUTPUT" >&2
  exit 1
}
printf 'runtime never falls back to copied provider settings: PASS\n'

# Runtime status may advertise the private origin only when every expected
# Serve handler points at this runtime's loopback service and path.
SERVE_KEY='phone.example.ts.net:443'
FULL_SERVE_JSON="$(jq -n \
  --arg key "$SERVE_KEY" \
  '{Web:{($key):{Handlers:{
    "/":{Proxy:"http://127.0.0.1:3010"},
    "/api":{Proxy:"http://127.0.0.1:8765/api"},
    "/ws":{Proxy:"http://127.0.0.1:8765/ws"},
    "/watch":{Proxy:"http://127.0.0.1:18080/watch"},
    "/health":{Proxy:"http://127.0.0.1:18080/health"}
  }}}}')"
ROUTED_PHONE_OUTPUT="$(
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  FAKE_BUILD_SHA="$CHECKOUT_SHA" \
  FAKE_TAILSCALE_SERVE_JSON="$FULL_SERVE_JSON" \
  EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
  bash "$ROOT/scripts/irin-runtime.sh" status 2>&1
)"
grep -Fq 'PRIVATE_PHONE https://phone.example.ts.net' <<<"$ROUTED_PHONE_OUTPUT"

MISSING_ROUTE_JSON="$(jq 'del(.Web["phone.example.ts.net:443"].Handlers["/ws"])' \
  <<<"$FULL_SERVE_JSON")"
UNROUTED_PHONE_OUTPUT="$(
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  FAKE_BUILD_SHA="$CHECKOUT_SHA" \
  FAKE_TAILSCALE_SERVE_JSON="$MISSING_ROUTE_JSON" \
  EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
  bash "$ROOT/scripts/irin-runtime.sh" status 2>&1
)"
if grep -Fq 'PRIVATE_PHONE ' <<<"$UNROUTED_PHONE_OUTPUT"; then
  printf 'runtime status claimed phone access with an incomplete Serve mapping\n%s\n' \
    "$UNROUTED_PHONE_OUTPUT" >&2
  exit 1
fi
printf 'runtime status requires complete Tailscale Serve mapping: PASS\n'

# The persistent login agent must enter the user's zsh login environment before
# booting, while preserving clone paths that require both shell and XML quoting.
LOGIN_ROOT="$TMP/clone & operator's IRIN"
LOGIN_SCRIPT="$LOGIN_ROOT/scripts/irin-runtime.sh"
LOGIN_PLIST="$TMP/login-agent.plist"
LOGIN_EXEC_LOG="$TMP/login-exec.log"
mkdir -p "$(dirname "$LOGIN_SCRIPT")"
cp "$ROOT/scripts/irin-runtime.sh" "$LOGIN_SCRIPT"
: >"$ACTION_LOG"
set +e
FAILED_ENABLE_OUTPUT="$(
  HOME="$TMP/home" \
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_STATE_DIR="$STATE_DIR" \
  IRIN_RUNTIME_LOGIN_PLIST="$LOGIN_PLIST" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  FAKE_LAUNCHCTL_ENABLE_FAIL=1 \
  bash "$LOGIN_SCRIPT" install-login 2>&1
)"
FAILED_ENABLE_STATUS=$?
set -e
[[ "$FAILED_ENABLE_STATUS" -ne 0 ]]
grep -Fq 'ERROR: launchctl enable failed for com.irinity.irin-runtime.login' \
  <<<"$FAILED_ENABLE_OUTPUT"
if grep -Fq 'launchctl bootstrap ' "$ACTION_LOG"; then
  printf 'login agent continued to bootstrap after enable failed\n%s\n' \
    "$(<"$ACTION_LOG")" >&2
  exit 1
fi
: >"$ACTION_LOG"
HOME="$TMP/home" \
XDG_STATE_HOME="$STATE_HOME" \
IRIN_RUNTIME_STATE_DIR="$STATE_DIR" \
IRIN_RUNTIME_LOGIN_PLIST="$LOGIN_PLIST" \
IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
FAKE_ACTION_LOG="$ACTION_LOG" \
FAKE_LAUNCHCTL_PRINT_OK=1 \
bash "$LOGIN_SCRIPT" install-login >/dev/null
EXPECTED_LOGIN_ACTIONS="$(printf '%s\n' \
  "launchctl bootout gui/$(id -u)/com.irinity.irin-runtime.login" \
  "launchctl enable gui/$(id -u)/com.irinity.irin-runtime.login" \
  "launchctl bootstrap gui/$(id -u) $LOGIN_PLIST" \
  "launchctl print gui/$(id -u)/com.irinity.irin-runtime.login")"
[[ "$(<"$ACTION_LOG")" == "$EXPECTED_LOGIN_ACTIONS" ]] || {
  printf 'login agent did not re-enable the label before bootstrap\nexpected:\n%s\nactual:\n%s\n' \
    "$EXPECTED_LOGIN_ACTIONS" "$(<"$ACTION_LOG")" >&2
  exit 1
}
/usr/bin/plutil -lint "$LOGIN_PLIST" >/dev/null
LOGIN_SHELL="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:0' "$LOGIN_PLIST")"
LOGIN_FLAGS="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:1' "$LOGIN_PLIST")"
[[ "$LOGIN_SHELL" == "/bin/zsh" ]] || {
  printf 'login agent did not use zsh: %s\n' "$LOGIN_SHELL" >&2
  exit 1
}
[[ "$LOGIN_FLAGS" == "-lic" ]] || {
  printf 'login agent did not enter an interactive login shell: %s\n' "$LOGIN_FLAGS" >&2
  exit 1
}
LOGIN_COMMAND="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:2' "$LOGIN_PLIST")"
SHELL_RC_PATH="$TMP/home/.zsh""rc"
printf 'export LOGIN_SHELL_PROVIDER_MARKER=login-shell-export\n' >"$SHELL_RC_PATH"
cat >"$LOGIN_SCRIPT" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n%s\n' "${1:-}" "${LOGIN_SHELL_PROVIDER_MARKER:-missing}" >"${LOGIN_EXEC_LOG:?}"
EOF
chmod +x "$LOGIN_SCRIPT"
HOME="$TMP/home" LOGIN_EXEC_LOG="$LOGIN_EXEC_LOG" \
  "$LOGIN_SHELL" "$LOGIN_FLAGS" "$LOGIN_COMMAND"
[[ "$(<"$LOGIN_EXEC_LOG")" == $'boot\nlogin-shell-export' ]] || {
  printf 'login agent did not preserve the interactive zsh export\n%s\n' \
    "$(<"$LOGIN_EXEC_LOG")" >&2
  exit 1
}
printf 'runtime login agent preserves login shell and quoted clone path: PASS\n'

# A RunAtLoad boot can still hold the runtime lock when an operator immediately
# repeats setup. The next control command must wait briefly, never run alongside it.
LOCK_MARKER="$TMP/control-lock-held"
/usr/bin/lockf -k -t 0 "$STATE_DIR/control.lock" /bin/sh -c \
  'touch "$1"; sleep 1' _ "$LOCK_MARKER" &
LOCK_HOLDER_PID=$!
for _ in $(seq 1 50); do
  [[ -f "$LOCK_MARKER" ]] && break
  sleep 0.02
done
[[ -f "$LOCK_MARKER" ]] || {
  printf 'failed to establish isolated runtime lock contention\n' >&2
  exit 1
}
set +e
WAITED_BOOT_OUTPUT="$(
  HOME="$TMP/home" \
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_CONTROL_LOCK_WAIT_SECS=3 \
  IRIN_TAILSCALE_SERVE=0 \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  FAKE_BUILD_SHA="$CHECKOUT_SHA" \
  FAKE_CHECKOUT_SHA="$CHECKOUT_SHA" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
  bash "$ROOT/scripts/irin-runtime.sh" boot 2>&1
)"
WAITED_BOOT_STATUS=$?
set -e
wait "$LOCK_HOLDER_PID"
[[ "$WAITED_BOOT_STATUS" -eq 0 ]] || {
  printf 'runtime boot did not wait for the active recovery command\n%s\n' \
    "$WAITED_BOOT_OUTPUT" >&2
  exit 1
}
grep -Fq 'OK: stack already healthy' <<<"$WAITED_BOOT_OUTPUT"
printf 'runtime control lock waits safely for login recovery: PASS\n'

set +e
UNTRACKED_OUTPUT="$(
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_TAILSCALE_SERVE=0 \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  FAKE_BUILD_SHA="$CHECKOUT_SHA" \
  FAKE_UNTRACKED_ONLY=1 \
  EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
  bash "$ROOT/scripts/irin-runtime.sh" status 2>&1
)"
UNTRACKED_STATUS=$?
set -e
[[ "$UNTRACKED_STATUS" -eq 0 ]] || {
  printf 'expected untracked-only checkout to preserve tracked provenance parity\n%s\n' "$UNTRACKED_OUTPUT" >&2
  exit 1
}
grep -Fq "CHECKOUT profile=canonical branch=main sha=$CHECKOUT_SHA tree=clean" <<<"$UNTRACKED_OUTPUT"
grep -Fq "RUNTIME_PARITY ok sha=$CHECKOUT_SHA tree=clean" <<<"$UNTRACKED_OUTPUT"

printf 'runtime-status untracked-only provenance: PASS\n'

jq -n \
  --arg root "$ROOT" \
  --arg sha "$CHECKOUT_SHA" \
  '{profile:"canonical",root:$root,origin:"https://github.com/irinityhq/irin.git",branch:"main",sha:$sha,dirty:true,compose_project:"gateway"}' \
  >"$STATE_DIR/source.json"

set +e
TRACKED_DIRTY_OUTPUT="$(
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_TAILSCALE_SERVE=0 \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  FAKE_BUILD_SHA="$CHECKOUT_SHA" \
  FAKE_BUILD_DIRTY=true \
  FAKE_TRACKED_DIRTY=1 \
  EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
  bash "$ROOT/scripts/irin-runtime.sh" status 2>&1
)"
TRACKED_DIRTY_STATUS=$?
set -e
[[ "$TRACKED_DIRTY_STATUS" -eq 0 ]] || {
  printf 'expected matching tracked-dirty provenance to remain truthful parity\n%s\n' "$TRACKED_DIRTY_OUTPUT" >&2
  exit 1
}
grep -Fq "CHECKOUT profile=canonical branch=main sha=$CHECKOUT_SHA tree=dirty" <<<"$TRACKED_DIRTY_OUTPUT"
grep -Fq "RUNTIME_PARITY ok sha=$CHECKOUT_SHA tree=dirty" <<<"$TRACKED_DIRTY_OUTPUT"

printf 'runtime-status tracked-dirty provenance: PASS\n'

: >"$ACTION_LOG"
set +e
UNTRACKED_START_OUTPUT="$(
  HOME="$TMP/home" \
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_TAILSCALE_SERVE=0 \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  FAKE_BUILD_SHA="$CHECKOUT_SHA" \
  FAKE_UNTRACKED_ONLY=1 \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  bash "$ROOT/scripts/irin-runtime.sh" start 2>&1
)"
UNTRACKED_START_STATUS=$?
set -e
[[ "$UNTRACKED_START_STATUS" -ne 0 ]] || {
  printf 'expected manual canonical start to reject untracked files\n%s\n' "$UNTRACKED_START_OUTPUT" >&2
  exit 1
}
grep -Fq "canonical runtime checkout is dirty" <<<"$UNTRACKED_START_OUTPUT"
if grep -Eq '^(cargo |npm |docker .* build)' "$ACTION_LOG"; then
  printf 'rejected manual start reached build actions\n%s\n' "$(<"$ACTION_LOG")" >&2
  exit 1
fi

printf 'runtime-start untracked guardrail: PASS\n'

# A healthy stale stack after a pull must rebuild and restart before the
# checkout receipt can replace the prior live identity.
jq -n \
  --arg root "$ROOT" \
  --arg sha "$STALE_SHA" \
  '{profile:"canonical",root:$root,origin:"https://github.com/irinityhq/irin.git",branch:"main",sha:$sha,dirty:false,compose_project:"gateway"}' \
  >"$STATE_DIR/source.json"
rm -f "$REBUILT_MARKER"
LAUNCHCTL_PRINT_COUNT="$TMP/launchctl-print-count"
rm -f "$LAUNCHCTL_PRINT_COUNT"
: >"$ACTION_LOG"
set +e
BOOT_OUTPUT="$(
  env -u XAI_API_KEY -u OPENAI_API_KEY -u ANTHROPIC_API_KEY \
  -u NVIDIA_API_KEY -u NOUS_API_KEY -u VERTEX_PROJECT -u VERTEX_LOCATION \
  -u VERTEX_GEMINI_MODEL -u GOOGLE_CLOUD_PROJECT \
  HOME="$TMP/home" \
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_TAILSCALE_SERVE=0 \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  IRIN_COUNCIL_PORT=48765 \
  IRIN_WEB_PORT=43010 \
  IRIN_GATEWAY_PORT=48080 \
  FAKE_BUILD_SHA="$STALE_SHA" \
  FAKE_CHECKOUT_SHA="$CHECKOUT_SHA" \
  FAKE_REBUILT_MARKER="$REBUILT_MARKER" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  FAKE_LAUNCHCTL_PRINT_SUCCESSES=3 \
  FAKE_LAUNCHCTL_PRINT_COUNT_FILE="$LAUNCHCTL_PRINT_COUNT" \
  EXPECT_COMPOSE_PROVIDER_ABSENT=1 \
  EXPECTED_COMPOSE_GW_API_KEY='stored-current-gateway-key' \
  EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
  bash "$ROOT/scripts/irin-runtime.sh" boot 2>&1
)"
BOOT_STATUS=$?
set -e

[[ "$BOOT_STATUS" -eq 0 ]] || {
  printf 'expected stale healthy boot to self-heal\n%s\n' "$BOOT_OUTPUT" >&2
  exit 1
}
RECEIPT_SHA="$(jq -r '.sha' "$STATE_DIR/source.json")"
[[ "$RECEIPT_SHA" == "$CHECKOUT_SHA" ]] || {
  printf 'self-healed boot wrote unexpected receipt %s\n%s\n' "$RECEIPT_SHA" "$BOOT_OUTPUT" >&2
  exit 1
}
grep -Fq "cargo build --manifest-path $ROOT/Cargo.toml --release -p council-rs --bin council" "$ACTION_LOG"
grep -Fq "npm run build:hosted" "$ACTION_LOG"
grep -Fq " build gateway sidecar" "$ACTION_LOG"
grep -Fq "launchctl submit" "$ACTION_LOG"
grep -Fq " up -d" "$ACTION_LOG"
BOOTOUT_LINE="$(grep -n -m1 '^launchctl bootout gui/.*/com.irinity.irin-runtime$' "$ACTION_LOG" | cut -d: -f1)"
SERVE_PRINT_COUNT="$(awk '/^launchctl submit / { exit } /^launchctl print gui\/.*\/com\.irinity\.irin-runtime$/ { count++ } END { print count + 0 }' "$ACTION_LOG")"
SERVE_PRINT_LINE="$(awk '/^launchctl submit / { exit } /^launchctl print gui\/.*\/com\.irinity\.irin-runtime$/ { line=NR } END { print line }' "$ACTION_LOG")"
SUBMIT_LINE="$(grep -n -m1 '^launchctl submit ' "$ACTION_LOG" | cut -d: -f1)"
[[ "$SERVE_PRINT_COUNT" -eq 4 \
  && -n "$BOOTOUT_LINE" && -n "$SERVE_PRINT_LINE" && -n "$SUBMIT_LINE" \
  && "$BOOTOUT_LINE" -lt "$SERVE_PRINT_LINE" \
  && "$SERVE_PRINT_LINE" -lt "$SUBMIT_LINE" ]] || {
  printf 'runtime start did not confirm the retired serve job was gone before replacement\n%s\n' \
    "$(<"$ACTION_LOG")" >&2
  exit 1
}

printf 'runtime-boot stale self-heal: PASS\n'

# A failed self-heal must preserve the prior receipt.
jq -n \
  --arg root "$ROOT" \
  --arg sha "$STALE_SHA" \
  '{profile:"canonical",root:$root,origin:"https://github.com/irinityhq/irin.git",branch:"main",sha:$sha,dirty:false,compose_project:"gateway"}' \
  >"$STATE_DIR/source.json"
rm -f "$REBUILT_MARKER"
: >"$ACTION_LOG"

set +e
FAILED_BOOT_OUTPUT="$(
  HOME="$TMP/home" \
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_TAILSCALE_SERVE=0 \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  IRIN_COUNCIL_PORT=48765 \
  IRIN_WEB_PORT=43010 \
  IRIN_GATEWAY_PORT=48080 \
  FAKE_BUILD_SHA="$STALE_SHA" \
  FAKE_CHECKOUT_SHA="$CHECKOUT_SHA" \
  FAKE_REBUILT_MARKER="$REBUILT_MARKER" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  FAKE_BUILD_FAIL=1 \
  EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
  bash "$ROOT/scripts/irin-runtime.sh" boot 2>&1
)"
FAILED_BOOT_STATUS=$?
set -e

[[ "$FAILED_BOOT_STATUS" -ne 0 ]] || {
  printf 'expected failed self-heal boot to fail\n%s\n' "$FAILED_BOOT_OUTPUT" >&2
  exit 1
}
FAILED_RECEIPT_SHA="$(jq -r '.sha' "$STATE_DIR/source.json")"
[[ "$FAILED_RECEIPT_SHA" == "$STALE_SHA" ]] || {
  printf 'failed self-heal overwrote receipt with %s\n%s\n' "$FAILED_RECEIPT_SHA" "$FAILED_BOOT_OUTPUT" >&2
  exit 1
}
if grep -Eq '^(launchctl bootout|docker .* down)' "$ACTION_LOG"; then
  printf 'failed build tore down the previously healthy stale stack\n%s\n' "$(<"$ACTION_LOG")" >&2
  exit 1
fi

printf 'runtime-boot failed self-heal receipt preservation: PASS\n'

# A fully matching healthy stack remains the idempotent boot fast path.
jq -n \
  --arg root "$ROOT" \
  --arg sha "$CHECKOUT_SHA" \
  '{profile:"canonical",root:$root,origin:"https://github.com/irinityhq/irin.git",branch:"main",sha:$sha,dirty:false,compose_project:"gateway"}' \
  >"$STATE_DIR/source.json"
rm -f "$REBUILT_MARKER"
: >"$ACTION_LOG"

set +e
FAST_BOOT_OUTPUT="$(
  HOME="$TMP/home" \
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_TAILSCALE_SERVE=0 \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  IRIN_COUNCIL_PORT=48765 \
  IRIN_WEB_PORT=43010 \
  IRIN_GATEWAY_PORT=48080 \
  FAKE_BUILD_SHA="$CHECKOUT_SHA" \
  FAKE_CHECKOUT_SHA="$CHECKOUT_SHA" \
  FAKE_REBUILT_MARKER="$REBUILT_MARKER" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
  bash "$ROOT/scripts/irin-runtime.sh" boot 2>&1
)"
FAST_BOOT_STATUS=$?
set -e

[[ "$FAST_BOOT_STATUS" -eq 0 ]] || {
  printf 'expected matching healthy boot to stay fast\n%s\n' "$FAST_BOOT_OUTPUT" >&2
  exit 1
}
grep -Fq "OK: stack already healthy" <<<"$FAST_BOOT_OUTPUT"
if grep -Eq '^(cargo |npm |docker .* (build gateway sidecar|up -d)|launchctl submit)' "$ACTION_LOG"; then
  printf 'matching healthy boot performed rebuild/restart actions\n%s\n' "$(<"$ACTION_LOG")" >&2
  exit 1
fi

printf 'runtime-boot matching fast path: PASS\n'

# A down stack also rebuilds from source before any restart, rather than
# trusting release/.next artifacts or a previously tagged sidecar image.
jq -n \
  --arg root "$ROOT" \
  --arg sha "$STALE_SHA" \
  '{profile:"canonical",root:$root,origin:"https://github.com/irinityhq/irin.git",branch:"main",sha:$sha,dirty:false,compose_project:"gateway"}' \
  >"$STATE_DIR/source.json"
rm -f "$REBUILT_MARKER"
: >"$ACTION_LOG"

set +e
DOWN_BOOT_OUTPUT="$(
  HOME="$TMP/home" \
  XDG_STATE_HOME="$STATE_HOME" \
  IRIN_RUNTIME_PATH="$FAKE_BIN:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IRIN_TAILSCALE_SERVE=0 \
  IRIN_GATEWAY_ENV="$GATEWAY_ENV" \
  IRIN_COUNCIL_PORT=48765 \
  IRIN_WEB_PORT=43010 \
  IRIN_GATEWAY_PORT=48080 \
  FAKE_BUILD_SHA="$STALE_SHA" \
  FAKE_CHECKOUT_SHA="$CHECKOUT_SHA" \
  FAKE_REBUILT_MARKER="$REBUILT_MARKER" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  FAKE_STACK_DOWN_UNTIL_REBUILD=1 \
  EXPECTED_COUNCIL_TOKEN="$COUNCIL_TOKEN" \
  bash "$ROOT/scripts/irin-runtime.sh" boot 2>&1
)"
DOWN_BOOT_STATUS=$?
set -e

[[ "$DOWN_BOOT_STATUS" -eq 0 ]] || {
  printf 'expected down boot to rebuild and recover\n%s\n' "$DOWN_BOOT_OUTPUT" >&2
  exit 1
}
grep -Fq "cargo build --manifest-path $ROOT/Cargo.toml --release -p council-rs --bin council" "$ACTION_LOG"
grep -Fq " build gateway sidecar" "$ACTION_LOG"
grep -Fq "launchctl submit" "$ACTION_LOG"
grep -Fq " up -d" "$ACTION_LOG"

printf 'runtime-boot down-stack rebuild: PASS\n'
