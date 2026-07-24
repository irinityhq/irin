#!/usr/bin/env bash
# Shell-level regression for council provider env allow/deny filter
# (mirrors private_config::is_council_provider_env_key without running Rust).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPORT="$ROOT/packaging/receipts/PROVIDER_ENV_FILTER.txt"
mkdir -p "$ROOT/packaging/receipts"
: >"$REPORT"

python3 - <<'PY' | tee -a "$REPORT"
deny = {
    "GW_API_KEY",
    "WATCH_ADMIN_TOKEN",
    "COUNCIL_GATEWAY_TOKEN",
    "BOOTSTRAP_TOKEN",
    "AUTH_PEPPER",
    "CLAUDE_PROXY_TOKEN",
    "CODEX_PROXY_TOKEN",
    "CLOUDFLARE_API_TOKEN",
    "CLOUDFLARE_API_KEY",
}
vertex = {
    "VERTEX_PROJECT",
    "VERTEX_LOCATION",
    "VERTEX_GEMINI_MODEL",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
    "GOOGLE_APPLICATION_CREDENTIALS",
}

def is_council_provider_env_key(key: str) -> bool:
    if key in deny:
        return False
    if key.endswith("_API_KEY") or key == "OPENAI_ADMIN_KEY":
        return True
    return key in vertex

allow = [
    "XAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_ADMIN_KEY",
    "NVIDIA_API_KEY",
    "VERTEX_PROJECT",
    "GOOGLE_APPLICATION_CREDENTIALS",
]
block = list(deny) + ["PATH", "HOME", "USER"]
for k in allow:
    assert is_council_provider_env_key(k), k
for k in block:
    assert not is_council_provider_env_key(k), k
print("provider_env_filter_ok=true")
print("allow_count=", len(allow))
print("deny_count=", len(block))
PY
