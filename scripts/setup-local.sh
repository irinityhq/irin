#!/usr/bin/env bash
# Prepare private local configuration and start the canonical macOS runtime.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
IRIN_HOME="${IRIN_HOME:-$HOME/.irin}"
GATEWAY_ENV="${IRIN_GATEWAY_ENV:-$CONFIG_HOME/irin/gateway.env}"
LEDGER_KEY="${LEDGER_KEY_PATH:-$IRIN_HOME/ledger_key.pem}"
RUNTIME_SCRIPT="${IRIN_RUNTIME_SCRIPT:-$ROOT/scripts/irin-runtime.sh}"
PREPARE_ONLY=0

case "${1:-}" in
  "") ;;
  --prepare-only) PREPARE_ONLY=1 ;;
  *) printf 'usage: %s [--prepare-only]\n' "$0" >&2; exit 2 ;;
esac

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

upsert_private_env_value() {
  local file="$1" key="$2" value="$3" tmp
  tmp="$(mktemp "$(dirname "$file")/.gateway.env.XXXXXX")"
  awk -v key="$key" -v value="$value" '
    BEGIN { written = 0 }
    $0 ~ "^" key "=" { if (!written) print key "=" value; written = 1; next }
    { print }
    END { if (!written) print key "=" value }
  ' "$file" > "$tmp"
  chmod 600 "$tmp"
  mv "$tmp" "$file"
}

require_command() {
  local command="$1" label="$2" guidance="$3"
  command -v "$command" >/dev/null 2>&1 \
    || die "missing ${label} command: ${command}. ${guidance}"
}

[[ "$(uname -s)" == "Darwin" ]] \
  || die "make setup currently supports macOS only"

require_command cargo "Rust toolchain" "Install Rust from https://rustup.rs, then retry make setup."
require_command rustc "Rust toolchain" "Install Rust from https://rustup.rs, then retry make setup."
require_command node "Node.js" "Install Node.js 20 or newer, then retry make setup."
require_command npm "Node.js package manager" "Install Node.js 20 or newer, then retry make setup."
require_command docker "Docker Desktop" "Install Docker Desktop for Mac, open it, then retry make setup."
require_command curl "macOS tooling" "Install the macOS command-line tools, then retry make setup."
require_command git "Git" "Install the macOS command-line tools, then retry make setup."
require_command openssl "OpenSSL" "Install OpenSSL with Homebrew, then retry make setup."
require_command jq "jq" "Install jq with Homebrew, then retry make setup."
require_command make "make" "Install the macOS command-line tools, then retry make setup."
require_command lockf "macOS runtime lock" "Use the macOS system lockf command, then retry make setup."
require_command launchctl "macOS login recovery" "Use the macOS system launchctl command, then retry make setup."

node_major="$(node -p 'Number(process.versions.node.split(".")[0])')"
[[ "$node_major" =~ ^[0-9]+$ ]] && (( node_major >= 20 )) \
  || die "Node.js 20 or newer is required. Install a current Node.js release, then retry make setup."

if (( PREPARE_ONLY == 0 )); then
  docker info >/dev/null 2>&1 \
    || die "Docker Desktop is installed, but its daemon is not running. Open Docker Desktop, wait until it is ready, then retry make setup."
fi

mkdir -p "$(dirname "$GATEWAY_ENV")" "$IRIN_HOME"
chmod 700 "$(dirname "$GATEWAY_ENV")" "$IRIN_HOME"
umask 077

if [[ ! -f "$GATEWAY_ENV" ]]; then
  auth_pepper="$(openssl rand -hex 32)"
  bootstrap_token="$(openssl rand -hex 32)"
  watch_admin_token="$(openssl rand -hex 32)"
  council_token="$(openssl rand -hex 32)"
  claude_proxy_token="$(openssl rand -hex 32)"
  codex_proxy_token="$(openssl rand -hex 32)"
  tmp="$(mktemp "${TMPDIR:-/tmp}/irin-gateway-env.XXXXXX")"
  trap 'rm -f "$tmp"' EXIT
  while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
      AUTH_PEPPER=__GENERATED_AUTH_PEPPER__)
        printf 'AUTH_PEPPER=%s\n' "$auth_pepper" ;;
      BOOTSTRAP_TOKEN=__GENERATED_BOOTSTRAP_TOKEN__)
        printf 'BOOTSTRAP_TOKEN=%s\n' "$bootstrap_token" ;;
      WATCH_ADMIN_TOKEN=__GENERATED_WATCH_ADMIN_TOKEN__)
        printf 'WATCH_ADMIN_TOKEN=%s\n' "$watch_admin_token" ;;
      COUNCIL_GATEWAY_TOKEN=__GENERATED_COUNCIL_GATEWAY_TOKEN__)
        printf 'COUNCIL_GATEWAY_TOKEN=%s\n' "$council_token" ;;
      CLAUDE_PROXY_TOKEN=__GENERATED_CLAUDE_PROXY_TOKEN__)
        printf 'CLAUDE_PROXY_TOKEN=%s\n' "$claude_proxy_token" ;;
      CODEX_PROXY_TOKEN=__GENERATED_CODEX_PROXY_TOKEN__)
        printf 'CODEX_PROXY_TOKEN=%s\n' "$codex_proxy_token" ;;
      *) printf '%s\n' "$line" ;;
    esac
  done < "$ROOT/config/gateway.env.example" > "$tmp"
  chmod 600 "$tmp"
  mv "$tmp" "$GATEWAY_ENV"
  trap - EXIT
  printf 'Created %s\n' "$GATEWAY_ENV"
else
  chmod 600 "$GATEWAY_ENV"
  printf 'Keeping existing %s\n' "$GATEWAY_ENV"
fi

# Existing installs may predate the Watch BFF credential or managed host-side
# CLI adapters. Add their shared secrets atomically without replacing or
# printing any existing value.
for proxy_key in WATCH_ADMIN_TOKEN CLAUDE_PROXY_TOKEN CODEX_PROXY_TOKEN; do
  proxy_value="$(sed -n "s/^${proxy_key}=//p" "$GATEWAY_ENV" | sed -n '1p')"
  if [[ -z "$proxy_value" || "$proxy_value" == __GENERATED_*__ ]]; then
    proxy_secret="$(openssl rand -hex 32)"
    proxy_tmp="$(mktemp "$(dirname "$GATEWAY_ENV")/.gateway.env.XXXXXX")"
    awk -v key="$proxy_key" -v value="$proxy_secret" '
      BEGIN { written = 0 }
      $0 ~ "^" key "=" { if (!written) print key "=" value; written = 1; next }
      { print }
      END { if (!written) print key "=" value }
    ' "$GATEWAY_ENV" > "$proxy_tmp"
    chmod 600 "$proxy_tmp"
    mv "$proxy_tmp" "$GATEWAY_ENV"
  fi
done

if [[ ! -f "$LEDGER_KEY" ]]; then
  openssl rand -out "$LEDGER_KEY" 32
  chmod 600 "$LEDGER_KEY"
  printf 'Generated local ledger key at %s\n' "$LEDGER_KEY"
else
  size="$(wc -c < "$LEDGER_KEY" | tr -d ' ')"
  [[ "$size" == "32" ]] || die "existing ledger key must be exactly 32 bytes"
  chmod 600 "$LEDGER_KEY"
  printf 'Keeping existing ledger key at %s\n' "$LEDGER_KEY"
fi

if (( PREPARE_ONLY == 1 )); then
  printf 'Local configuration is ready.\n'
  exit 0
fi

# Start must complete and release the runtime control lock before bootstrap of
# the RunAtLoad login agent can invoke its idempotent `boot` path.
"$RUNTIME_SCRIPT" start

# The Gateway stores only hashes of provisioned client keys. A fresh install
# therefore needs one bootstrap call before Council can use governed routing.
# Reuse a valid private key on subsequent setup runs; rotate only when missing
# or rejected by the live Gateway. Never print the raw key or response body.
gateway_client_key="$(sed -n 's/^GW_API_KEY=//p' "$GATEWAY_ENV" | sed -n '1p')"
gateway_client_key_id="$(sed -n 's/^COUNCIL_GATEWAY_KEY_ID=//p' "$GATEWAY_ENV" | sed -n '1p')"
gateway_client_ready=0
if [[ -n "$gateway_client_key" && "$gateway_client_key_id" =~ ^k_[0-9a-f]{8}$ ]] \
  && curl -fsS --max-time 10 \
    -H "Authorization: Bearer ${gateway_client_key}" \
    http://127.0.0.1:18080/v1/models >/dev/null 2>&1; then
  gateway_client_ready=1
fi

if (( gateway_client_ready == 0 )); then
  bootstrap_token="$(sed -n 's/^BOOTSTRAP_TOKEN=//p' "$GATEWAY_ENV" | sed -n '1p')"
  [[ -n "$bootstrap_token" && "$bootstrap_token" != __GENERATED_*__ ]] \
    || die "BOOTSTRAP_TOKEN is missing from $GATEWAY_ENV; cannot provision the local Gateway client key"
  provision_payload="$(jq -Rnc \
    '{budget_key:"local-council",tier:"default",rpm:600,service_role:"council",admin_key:input}' \
    <<<"$bootstrap_token")"
  provision_response="$(curl -fsS --max-time 15 -X POST \
    -H 'Content-Type: application/json' \
    --data-binary @- \
    http://127.0.0.1:18080/admin/keys <<<"$provision_payload")" \
    || die "Gateway rejected local client-key provisioning"
  gateway_client_key="$(jq -er \
    '.raw_key | select(type == "string" and test("^gw_[0-9a-f]{32}$"))' \
    <<<"$provision_response")" \
    || die "Gateway returned an invalid local client key"
  gateway_client_key_id="$(jq -er \
    '.key_id | select(type == "string" and test("^k_[0-9a-f]{8}$"))' \
    <<<"$provision_response")" \
    || die "Gateway returned an invalid local Council key identity"
  upsert_private_env_value "$GATEWAY_ENV" GW_API_KEY "$gateway_client_key"
  upsert_private_env_value "$GATEWAY_ENV" COUNCIL_GATEWAY_KEY_ID "$gateway_client_key_id"
  "$RUNTIME_SCRIPT" reload-gateway-config
  "$RUNTIME_SCRIPT" reload-local-config
  printf 'Provisioned the private local Gateway client key.\n'
fi

"$RUNTIME_SCRIPT" install-login
runtime_status="$("$RUNTIME_SCRIPT" status)"
printf '%s\n' "$runtime_status"
private_phone_url="$(sed -n 's/^PRIVATE_PHONE //p' <<<"$runtime_status" | sed -n '1p')"
[[ "$private_phone_url" == https://* ]] || private_phone_url=""

printf '\nIRIN is ready.\n'
printf 'War Room: http://127.0.0.1:3010\n'
printf 'Council: http://127.0.0.1:8765\n'
printf 'Gateway: http://127.0.0.1:18080\n'
if [[ "${IRIN_TAILSCALE_SERVE:-auto}" == "0" ]]; then
  printf 'Private phone access: disabled by IRIN_TAILSCALE_SERVE=0; local access is ready.\n'
elif [[ -n "$private_phone_url" ]]; then
  printf 'Private phone: %s\n' "$private_phone_url"
elif command -v tailscale >/dev/null 2>&1 \
  && tailscale status >/dev/null 2>&1; then
  printf 'Private phone access: Tailscale is connected, but IRIN Serve routes are not ready; local access is ready. Rerun make setup.\n'
else
  printf 'Private phone access: optional — install and connect Tailscale, then rerun make setup.\n'
fi
printf 'Provider discovery uses your login shell; IRIN does not copy provider credentials.\n'
printf 'Login recovery: enabled (opt out: ./scripts/irin-runtime.sh uninstall-login)\n'
printf 'Next action: Open Discover\n'
printf 'Optional second command: make app-install\n'
