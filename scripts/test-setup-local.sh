#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

FAKE_BIN="$TMP/bin"
HOME_DIR="$TMP/home"
ACTION_LOG="$TMP/runtime-actions.log"
FAKE_RUNTIME="$TMP/irin-runtime"
mkdir -p "$FAKE_BIN" "$HOME_DIR"
: >"$ACTION_LOG"

write_fake() {
  local name="$1"
  shift
  {
    printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail'
    printf '%s\n' "$@"
  } >"$FAKE_BIN/$name"
  chmod +x "$FAKE_BIN/$name"
}

write_fake uname 'printf '\''Darwin\n'\'''
write_fake cargo 'exit 0'
write_fake rustc 'exit 0'
write_fake node '[[ "${1:-}" == "-p" ]] && printf '\''20\n'\'''
write_fake npm 'exit 0'
write_fake docker '[[ "${1:-}" == "info" ]] && exit 0; exit 0'
write_fake curl \
  'case " $* " in' \
  '  *" /admin/keys "*) printf '\''{"raw_key":"gw_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","key_id":"k_a1b2c3d4"}\n'\'' ;;' \
  '  *" /v1/models "*) printf '\''{"data":[]}\n'\'' ;;' \
  'esac'
write_fake git 'exit 0'
write_fake jq \
  'if [[ " $* " == *" -Rnc "* ]]; then' \
  '  printf '\''{"budget_key":"local-council","tier":"default","rpm":600,"service_role":"council","admin_key":"test"}\n'\''' \
  'elif [[ " $* " == *" .raw_key "* ]]; then' \
  '  printf '\''gw_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n'\''' \
  'elif [[ " $* " == *" .key_id "* ]]; then' \
  '  printf '\''k_a1b2c3d4\n'\''' \
  'elif [[ -n "${FAKE_TAILSCALE_DNS:-}" ]]; then' \
  '  printf '\''%s\n'\'' "$FAKE_TAILSCALE_DNS"' \
  'fi'
write_fake make 'exit 0'
write_fake lockf 'exit 0'
write_fake launchctl 'exit 0'
write_fake openssl \
  'if [[ "$1" == "rand" && "$2" == "-hex" ]]; then' \
  '  printf '\''feedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedface\n'\''' \
  'elif [[ "$1" == "rand" && "$2" == "-out" ]]; then' \
  '  printf '\''0123456789abcdef0123456789abcdef'\'' >"$3"' \
  'else' \
  '  exit 2' \
  'fi'

cat >"$FAKE_RUNTIME" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "${1:-}" >>"${FAKE_ACTION_LOG:?}"
case "${1:-}" in
  start)
    printf 'OK: canonical runtime started\n'
    ;;
  reload-local-config)
    printf 'OK: local runtime configuration reloaded\n'
    ;;
  reload-gateway-config)
    printf 'OK: Gateway runtime configuration reloaded\n'
    ;;
  install-login)
    [[ "$(sed -n '1p' "${FAKE_ACTION_LOG:?}")" == "start" ]] || {
      printf 'login recovery was installed before runtime start completed\n' >&2
      exit 91
    }
    printf 'OK: login recovery installed\n'
    ;;
  status)
    printf 'UP    Council http://127.0.0.1:8765\n'
    printf 'UP    War Room Web http://127.0.0.1:3010\n'
    printf 'UP    Gateway http://127.0.0.1:18080\n'
    if [[ -n "${FAKE_PRIVATE_PHONE_URL:-}" ]]; then
      printf 'PRIVATE_PHONE %s\n' "$FAKE_PRIVATE_PHONE_URL"
    fi
    ;;
  *)
    printf 'unexpected runtime action: %s\n' "${1:-}" >&2
    exit 92
    ;;
esac
EOF
chmod +x "$FAKE_RUNTIME"

# A failed prerequisite check must be actionable and must happen before setup
# writes any private files.
MISSING_HOME="$TMP/missing-home"
mkdir -p "$MISSING_HOME"
mv "$FAKE_BIN/cargo" "$FAKE_BIN/cargo.hidden"
set +e
MISSING_OUTPUT="$(
  HOME="$MISSING_HOME" \
  PATH="$FAKE_BIN:/usr/bin:/bin" \
  /bin/bash "$ROOT/scripts/setup-local.sh" --prepare-only 2>&1
)"
MISSING_STATUS=$?
set -e
mv "$FAKE_BIN/cargo.hidden" "$FAKE_BIN/cargo"
[[ "$MISSING_STATUS" -ne 0 ]] || {
  printf 'expected missing prerequisite preflight to fail\n%s\n' "$MISSING_OUTPUT" >&2
  exit 1
}
grep -Fq 'ERROR: missing Rust toolchain command: cargo' <<<"$MISSING_OUTPUT"
grep -Fq 'https://rustup.rs' <<<"$MISSING_OUTPUT"
[[ ! -e "$MISSING_HOME/.irin" && ! -e "$MISSING_HOME/.config/irin" ]] || {
  printf 'failed preflight wrote private setup files\n' >&2
  exit 1
}
printf 'setup missing prerequisite message: PASS\n'

# Prepare-only creates the documented private files but never calls the runtime.
: >"$ACTION_LOG"
PREPARE_OUTPUT="$(
  HOME="$HOME_DIR" \
  XDG_CONFIG_HOME="$HOME_DIR/.config" \
  IRIN_HOME="$HOME_DIR/.irin" \
  PATH="$FAKE_BIN:/usr/bin:/bin" \
  IRIN_RUNTIME_SCRIPT="$FAKE_RUNTIME" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  /usr/bin/make -s -C "$ROOT" setup-prepare 2>&1
)"
[[ ! -s "$ACTION_LOG" ]] || {
  printf 'prepare-only called the runtime\n%s\n' "$(<"$ACTION_LOG")" >&2
  exit 1
}
[[ -f "$HOME_DIR/.config/irin/gateway.env" ]]
[[ ! -e "$HOME_DIR/.irin/.env" ]]
[[ -f "$HOME_DIR/.irin/ledger_key.pem" ]]
grep -Eq '^CLAUDE_PROXY_TOKEN=[0-9a-f]{64}$' "$HOME_DIR/.config/irin/gateway.env"
grep -Eq '^CODEX_PROXY_TOKEN=[0-9a-f]{64}$' "$HOME_DIR/.config/irin/gateway.env"
grep -Eq '^WATCH_ADMIN_TOKEN=[0-9a-f]{64}$' "$HOME_DIR/.config/irin/gateway.env"
grep -Fq 'Local configuration is ready.' <<<"$PREPARE_OUTPUT"
printf 'setup prepare-only: PASS\n'

# Existing installs receive missing adapter secrets atomically. Existing
# non-empty values are preserved exactly and private values are never printed.
printf 'BOOTSTRAP_TOKEN=test-bootstrap\nCLAUDE_PROXY_TOKEN=keep-this-existing-value\nCODEX_PROXY_TOKEN=__GENERATED_CODEX_PROXY_TOKEN__\n' \
  >"$HOME_DIR/.config/irin/gateway.env"
MIGRATION_OUTPUT="$(
  HOME="$HOME_DIR" \
  XDG_CONFIG_HOME="$HOME_DIR/.config" \
  IRIN_HOME="$HOME_DIR/.irin" \
  PATH="$FAKE_BIN:/usr/bin:/bin" \
  IRIN_RUNTIME_SCRIPT="$FAKE_RUNTIME" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  /usr/bin/make -s -C "$ROOT" setup-prepare 2>&1
)"
grep -Fxq 'CLAUDE_PROXY_TOKEN=keep-this-existing-value' \
  "$HOME_DIR/.config/irin/gateway.env"
grep -Eq '^CODEX_PROXY_TOKEN=[0-9a-f]{64}$' \
  "$HOME_DIR/.config/irin/gateway.env"
grep -Eq '^WATCH_ADMIN_TOKEN=[0-9a-f]{64}$' \
  "$HOME_DIR/.config/irin/gateway.env"
if grep -Fq '__GENERATED_' "$HOME_DIR/.config/irin/gateway.env"; then
  printf 'setup migration retained a public placeholder token\n' >&2
  exit 1
fi
[[ "$(grep -c '^CLAUDE_PROXY_TOKEN=' "$HOME_DIR/.config/irin/gateway.env")" == 1 ]]
[[ "$(grep -c '^CODEX_PROXY_TOKEN=' "$HOME_DIR/.config/irin/gateway.env")" == 1 ]]
[[ "$(grep -c '^WATCH_ADMIN_TOKEN=' "$HOME_DIR/.config/irin/gateway.env")" == 1 ]]
if grep -Eq 'keep-this-existing-value|feedface' <<<"$MIGRATION_OUTPUT"; then
  printf 'setup migration printed private values\n%s\n' "$MIGRATION_OUTPUT" >&2
  exit 1
fi
printf 'setup existing-install adapter-token migration: PASS\n'

# A legacy provider file is left byte-for-byte untouched. Setup does not create,
# read, or advertise this old credential location.
printf '# existing provider configuration\n' >"$HOME_DIR/.irin/.env"
PROVIDER_BEFORE="$(shasum -a 256 "$HOME_DIR/.irin/.env" | awk '{print $1}')"

# Full setup starts first, installs login recovery only after start returns,
# reports bounded status, and succeeds without Tailscale.
: >"$ACTION_LOG"
SETUP_OUTPUT="$(
  HOME="$HOME_DIR" \
  XDG_CONFIG_HOME="$HOME_DIR/.config" \
  IRIN_HOME="$HOME_DIR/.irin" \
  PATH="$FAKE_BIN:/usr/bin:/bin" \
  IRIN_RUNTIME_SCRIPT="$FAKE_RUNTIME" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  /usr/bin/make -s -C "$ROOT" setup 2>&1
)"
EXPECTED_ACTIONS=$'start\nreload-gateway-config\nreload-local-config\ninstall-login\nstatus'
[[ "$(<"$ACTION_LOG")" == "$EXPECTED_ACTIONS" ]] || {
  printf 'unexpected setup action order\nexpected:\n%s\nactual:\n%s\n' \
    "$EXPECTED_ACTIONS" "$(<"$ACTION_LOG")" >&2
  exit 1
}
PROVIDER_AFTER="$(shasum -a 256 "$HOME_DIR/.irin/.env" | awk '{print $1}')"
[[ "$PROVIDER_AFTER" == "$PROVIDER_BEFORE" ]] || {
  printf 'setup changed the existing provider file\n' >&2
  exit 1
}
grep -Fq 'UP    Council http://127.0.0.1:8765' <<<"$SETUP_OUTPUT"
grep -Fq 'UP    Gateway http://127.0.0.1:18080' <<<"$SETUP_OUTPUT"
grep -Fq 'War Room: http://127.0.0.1:3010' <<<"$SETUP_OUTPUT"
grep -Fq 'Private phone access: optional' <<<"$SETUP_OUTPUT"
grep -Fq 'Login recovery: enabled (opt out: ./scripts/irin-runtime.sh uninstall-login)' <<<"$SETUP_OUTPUT"
grep -Fq 'Next action: Open Discover' <<<"$SETUP_OUTPUT"
grep -Fq 'Provider discovery uses your login shell; IRIN does not copy provider credentials.' \
  <<<"$SETUP_OUTPUT"
if grep -Fq 'feedface' <<<"$SETUP_OUTPUT"; then
  printf 'setup printed generated private values\n%s\n' "$SETUP_OUTPUT" >&2
  exit 1
fi
grep -Fxq 'COUNCIL_GATEWAY_KEY_ID=k_a1b2c3d4' \
  "$HOME_DIR/.config/irin/gateway.env"
grep -Fq 'service_role:"council"' "$ROOT/scripts/setup-local.sh"
printf 'setup start, recovery, no-Tailscale, and completion output: PASS\n'

# Connectivity without the expected Serve routes must not claim phone access.
write_fake tailscale \
  'case " $* " in' \
  '  *" status --json "*) printf '\''{"Self":{"DNSName":"phone.example.ts.net."}}\n'\'' ;;' \
  '  *" status "*) exit 0 ;;' \
  '  *) exit 0 ;;' \
  'esac'
: >"$ACTION_LOG"
TAILSCALE_OUTPUT="$(
  HOME="$HOME_DIR" \
  XDG_CONFIG_HOME="$HOME_DIR/.config" \
  IRIN_HOME="$HOME_DIR/.irin" \
  PATH="$FAKE_BIN:/usr/bin:/bin" \
  IRIN_RUNTIME_SCRIPT="$FAKE_RUNTIME" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  FAKE_TAILSCALE_DNS='phone.example.ts.net.' \
  /usr/bin/make -s -C "$ROOT" setup 2>&1
)"
grep -Fq 'Private phone access: Tailscale is connected, but IRIN Serve routes are not ready; local access is ready. Rerun make setup.' \
  <<<"$TAILSCALE_OUTPUT"
if grep -Fq 'Private phone: https://' <<<"$TAILSCALE_OUTPUT"; then
  printf 'setup claimed private phone access without verified Serve routes\n%s\n' \
    "$TAILSCALE_OUTPUT" >&2
  exit 1
fi
printf 'setup connected-Tailscale without Serve routes remains local-only: PASS\n'

# The runtime's verified route report is the only source of the private URL.
: >"$ACTION_LOG"
TAILSCALE_OUTPUT="$(
  HOME="$HOME_DIR" \
  XDG_CONFIG_HOME="$HOME_DIR/.config" \
  IRIN_HOME="$HOME_DIR/.irin" \
  PATH="$FAKE_BIN:/usr/bin:/bin" \
  IRIN_RUNTIME_SCRIPT="$FAKE_RUNTIME" \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  FAKE_PRIVATE_PHONE_URL='https://phone.example.ts.net' \
  /usr/bin/make -s -C "$ROOT" setup 2>&1
)"
grep -Fq 'Private phone: https://phone.example.ts.net' <<<"$TAILSCALE_OUTPUT"
printf 'setup verified Tailscale Serve private URL: PASS\n'

: >"$ACTION_LOG"
TAILSCALE_DISABLED_OUTPUT="$(
  HOME="$HOME_DIR" \
  XDG_CONFIG_HOME="$HOME_DIR/.config" \
  IRIN_HOME="$HOME_DIR/.irin" \
  PATH="$FAKE_BIN:/usr/bin:/bin" \
  IRIN_RUNTIME_SCRIPT="$FAKE_RUNTIME" \
  IRIN_TAILSCALE_SERVE=0 \
  FAKE_ACTION_LOG="$ACTION_LOG" \
  FAKE_TAILSCALE_DNS='phone.example.ts.net.' \
  /usr/bin/make -s -C "$ROOT" setup 2>&1
)"
grep -Fq 'Private phone access: disabled by IRIN_TAILSCALE_SERVE=0; local access is ready.' \
  <<<"$TAILSCALE_DISABLED_OUTPUT"
if grep -Fq 'Private phone: https://' <<<"$TAILSCALE_DISABLED_OUTPUT"; then
  printf 'setup claimed private phone access while Tailscale Serve was disabled\n%s\n' \
    "$TAILSCALE_DISABLED_OUTPUT" >&2
  exit 1
fi
printf 'setup disabled-Tailscale remains local-only: PASS\n'
