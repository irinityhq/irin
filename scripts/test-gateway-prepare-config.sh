#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HELPER="$ROOT/gateway/tools/prepare-local-config.sh"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/irin-gateway-prepare-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

mode_of() {
  if stat -f '%Lp' "$1" >/dev/null 2>&1; then
    stat -f '%Lp' "$1"
  else
    stat -c '%a' "$1"
  fi
}

run_helper() {
  local case_root="$1"
  HOME="$case_root/home" \
  XDG_CONFIG_HOME="$case_root/config" \
  IRIN_HOME="$case_root/irin-home" \
  IRIN_GATEWAY_ENV="$case_root/config/irin/gateway.env" \
  LEDGER_KEY_PATH="$case_root/irin-home/ledger_key.pem" \
    bash "$HELPER"
}

# Prerequisite failure is side-effect free: only dirname is needed before the
# helper rejects the deliberately absent OpenSSL command.
mkdir -p "$tmp/no-openssl/bin"
ln -s "$(command -v dirname)" "$tmp/no-openssl/bin/dirname"
if HOME="$tmp/no-openssl/home" \
  XDG_CONFIG_HOME="$tmp/no-openssl/config" \
  IRIN_HOME="$tmp/no-openssl/irin-home" \
  IRIN_GATEWAY_ENV="$tmp/no-openssl/config/irin/gateway.env" \
  LEDGER_KEY_PATH="$tmp/no-openssl/irin-home/ledger_key.pem" \
  PATH="$tmp/no-openssl/bin" \
    /bin/bash "$HELPER" >/dev/null 2>&1; then
  fail "helper succeeded without OpenSSL"
fi
[[ ! -e "$tmp/no-openssl/config" ]] || fail "prerequisite failure wrote config"
[[ ! -e "$tmp/no-openssl/irin-home" ]] || fail "prerequisite failure wrote IRIN home"

# Fresh preparation replaces every managed placeholder and creates private,
# exact-width material.
fresh="$tmp/fresh"
run_helper "$fresh" >/dev/null
env_file="$fresh/config/irin/gateway.env"
ledger="$fresh/irin-home/ledger_key.pem"
[[ -f "$env_file" && -f "$ledger" ]] || fail "fresh files missing"
[[ "$(mode_of "$env_file")" == 600 ]] || fail "gateway.env is not 0600"
[[ "$(mode_of "$ledger")" == 600 ]] || fail "ledger key is not 0600"
[[ "$(mode_of "$(dirname "$env_file")")" == 700 ]] || fail "config dir is not 0700"
[[ "$(mode_of "$(dirname "$ledger")")" == 700 ]] || fail "IRIN home is not 0700"
[[ "$(wc -c < "$ledger" | tr -d ' ')" == 32 ]] || fail "ledger key is not 32 bytes"
if grep -q '__GENERATED_' "$env_file"; then
  fail "fresh config retained a generated placeholder"
fi
for key in AUTH_PEPPER BOOTSTRAP_TOKEN WATCH_ADMIN_TOKEN COUNCIL_GATEWAY_TOKEN CLAUDE_PROXY_TOKEN CODEX_PROXY_TOKEN; do
  value="$(sed -n "s/^${key}=//p" "$env_file" | sed -n '1p')"
  [[ "$value" =~ ^[0-9a-f]{64}$ ]] || fail "$key was not generated as 32-byte hex"
done

# Existing operator values are preserved while placeholders and missing managed
# fields migrate in place.
existing="$tmp/existing"
mkdir -p "$existing/config/irin" "$existing/irin-home"
printf '%s\n' \
  'AUTH_PEPPER=keep-operator-value' \
  'BOOTSTRAP_TOKEN=__GENERATED_BOOTSTRAP_TOKEN__' \
  'WATCH_ADMIN_TOKEN=' \
  'COUNCIL_GATEWAY_TOKEN=__GENERATED_COUNCIL_GATEWAY_TOKEN__' \
  'CLAUDE_PROXY_TOKEN=keep-claude-value' \
  > "$existing/config/irin/gateway.env"
printf '12345678901234567890123456789012' > "$existing/irin-home/ledger_key.pem"
run_helper "$existing" >/dev/null
grep -qx 'AUTH_PEPPER=keep-operator-value' "$existing/config/irin/gateway.env" \
  || fail "operator auth pepper was not preserved"
grep -qx 'CLAUDE_PROXY_TOKEN=keep-claude-value' "$existing/config/irin/gateway.env" \
  || fail "operator proxy token was not preserved"
for key in BOOTSTRAP_TOKEN WATCH_ADMIN_TOKEN COUNCIL_GATEWAY_TOKEN CODEX_PROXY_TOKEN; do
  value="$(sed -n "s/^${key}=//p" "$existing/config/irin/gateway.env" | sed -n '1p')"
  [[ "$value" =~ ^[0-9a-f]{64}$ ]] || fail "$key placeholder/missing value was not migrated"
done
[[ "$(mode_of "$existing/config/irin/gateway.env")" == 600 ]] || fail "existing env mode not repaired"
[[ "$(mode_of "$existing/irin-home/ledger_key.pem")" == 600 ]] || fail "existing key mode not repaired"

# An invalid existing signing seed is rejected and never overwritten.
invalid="$tmp/invalid"
mkdir -p "$invalid/config/irin" "$invalid/irin-home"
printf 'AUTH_PEPPER=keep\n' > "$invalid/config/irin/gateway.env"
printf 'short' > "$invalid/irin-home/ledger_key.pem"
if run_helper "$invalid" >/dev/null 2>&1; then
  fail "invalid existing ledger key was accepted"
fi
[[ "$(wc -c < "$invalid/irin-home/ledger_key.pem" | tr -d ' ')" == 5 ]] \
  || fail "invalid ledger key was overwritten"

printf 'gateway prepare-config tests passed\n'
