#!/usr/bin/env bash
# Developer-only: prepare private Gateway local config and ledger key.
# Does not start Council, Next.js, login recovery, or CLI proxies.
# Optional product path for compose/Gateway development — not an installed-app dependency.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
IRIN_HOME="${IRIN_HOME:-$HOME/.irin}"
GATEWAY_ENV="${IRIN_GATEWAY_ENV:-$CONFIG_HOME/irin/gateway.env}"
LEDGER_KEY="${LEDGER_KEY_PATH:-$IRIN_HOME/ledger_key.pem}"
COMPOSE_LEDGER_KEY="${IRIN_COMPOSE_LEDGER_KEY:-$IRIN_HOME/compose-ledger-key}"

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

require_command() {
  local command="$1" label="$2" guidance="$3"
  command -v "$command" >/dev/null 2>&1 \
    || die "missing ${label} command: ${command}. ${guidance}"
}

require_command openssl "OpenSSL" "Install OpenSSL, then retry."

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

for managed_key in \
  AUTH_PEPPER BOOTSTRAP_TOKEN WATCH_ADMIN_TOKEN COUNCIL_GATEWAY_TOKEN \
  CLAUDE_PROXY_TOKEN CODEX_PROXY_TOKEN; do
  managed_value="$(sed -n "s/^${managed_key}=//p" "$GATEWAY_ENV" | sed -n '1p')"
  if [[ -z "$managed_value" || "$managed_value" == __GENERATED_*__ ]]; then
    managed_secret="$(openssl rand -hex 32)"
    managed_tmp="$(mktemp "$(dirname "$GATEWAY_ENV")/.gateway.env.XXXXXX")"
    awk -v key="$managed_key" -v value="$managed_secret" '
      BEGIN { written = 0 }
      $0 ~ "^" key "=" { if (!written) print key "=" value; written = 1; next }
      { print }
      END { if (!written) print key "=" value }
    ' "$GATEWAY_ENV" > "$managed_tmp"
    chmod 600 "$managed_tmp"
    mv "$managed_tmp" "$GATEWAY_ENV"
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

# Compose stack uses a dedicated seed so docker never bind-mounts the
# operator canonical ledger_key.pem or host gcloud ADC.
if [[ ! -f "$COMPOSE_LEDGER_KEY" ]]; then
  mkdir -p "$(dirname "$COMPOSE_LEDGER_KEY")"
  openssl rand -out "$COMPOSE_LEDGER_KEY" 32
  chmod 600 "$COMPOSE_LEDGER_KEY"
  printf 'Generated compose ledger key at %s\n' "$COMPOSE_LEDGER_KEY"
else
  compose_size="$(wc -c < "$COMPOSE_LEDGER_KEY" | tr -d ' ')"
  [[ "$compose_size" == "32" ]] || die "existing compose ledger key must be exactly 32 bytes"
  chmod 600 "$COMPOSE_LEDGER_KEY"
  printf 'Keeping existing compose ledger key at %s\n' "$COMPOSE_LEDGER_KEY"
fi

printf 'Gateway local configuration is ready.\n'
printf 'This helper does not start Council, War Room, login recovery, or CLI proxies.\n'
