#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
NGINX="$ROOT/gateway/nginx.conf"
ROUTER="$ROOT/gateway/lua/router.lua"
MODELS="$ROOT/gateway/conf/models.json"

[[ $(grep -Fc 'location = /v1/models {' "$NGINX") -eq 1 ]]
grep -Fq 'require("router").models()' "$NGINX"
grep -Fq 'function _M.models()' "$ROUTER"
grep -Fq 'authenticate_request(ngx.req.get_headers())' "$ROUTER"
grep -Fq 'for model_id, model_cfg in pairs(config.models)' "$ROUTER"
grep -Fq 'for alias, resolved in pairs(config.aliases)' "$ROUTER"
grep -Fq 'ready = ready' "$ROUTER"
grep -Fq 'proxy_unreachable_or_invalid' "$ROUTER"
grep -Fq 'credentials_verified' "$ROUTER"
grep -Fq 'credentials_rejected' "$ROUTER"
grep -Fq 'project_unavailable' "$ROUTER"
grep -Fq 'advertised_model_set' "$ROUTER"
grep -Fq 'decoded.data or decoded.models' "$ROUTER"
grep -Fq 'httpc:set_timeout(15000)' "$ROUTER"
grep -Fq 'lua_ssl_trusted_certificate /etc/ssl/certs/ca-certificates.crt;' \
  "$ROOT/gateway/nginx.conf"
[[ $(grep -Fc 'add_header X-Gateway-Request-ID $request_id always;' "$NGINX") -eq 1 ]]
[[ $(grep -Fc 'proxy_hide_header     X-Gateway-Request-ID;' "$NGINX") -eq 1 ]]
grep -Fq '["X-Proxy-Auth"] = "Bearer " .. proxy_token' "$ROUTER"
grep -Fq 'model_unsupported' "$ROUTER"
grep -Fq 'transports = council_transport.advertised_for_provider' "$ROUTER"
grep -Fq 'X-Council-Transport-ID' "$ROUTER"
grep -Fq 'X-Council-Original-Provider' "$ROUTER"
grep -Fq 'council_transport.is_trusted_council' "$ROUTER"
grep -Fq 'ERR_COUNCIL_TRANSPORT_IDENTITY' "$ROUTER"
grep -Fq 'sidecar.vertex_token()' "$ROUTER"
grep -Fq 'os.getenv("VERTEX_GEMINI_MODEL")' "$ROUTER"
grep -Fq 'resolved_name == "gemini-3.1-pro-preview"' "$ROUTER"
grep -Fq 'translator.resolve_vertex_path(path, upstream_model)' "$ROUTER"

for model in \
  grok-4.3 \
  grok-4.20-0309-reasoning \
  gpt-5.6-sol \
  claude-opus-4-8 \
  claude-opus-4-6 \
  gemini-3.1-pro-preview \
  gemini-3.5-flash \
  mistralai/mistral-large-3-675b-instruct-2512 \
  qwen/qwen3.5-397b-a17b \
  z-ai/glm-5.2 \
  nvidia/nemotron-3-ultra-550b-a55b; do
  jq -e --arg model "$model" '.models[$model] != null' "$MODELS" >/dev/null
done

grep -Fq '["claude", "auth", "status"]' "$ROOT/gateway/tools/claude-proxy.py"
grep -Fq '["codex", "login", "status"]' "$ROOT/gateway/tools/codex-proxy.py"

echo "OK: authenticated exact model catalog surface is configured"
