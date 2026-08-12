#!/usr/bin/env bash
# Shell-level regression for council provider env allow/deny filter
# (mirrors private_config::is_council_provider_env_key without running Rust).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPORT="$ROOT/packaging/receipts/PROVIDER_ENV_FILTER.txt"
mkdir -p "$ROOT/packaging/receipts"
: >"$REPORT"

RS_SRC="$ROOT/council-rs/warroom-tauri/src-tauri/src/private_config.rs" \
python3 - <<'PY' | tee -a "$REPORT"
import os, re, sys

# Single source: extract the deny/vertex sets and the suffix rule from the
# authoritative Rust implementation so this mirror cannot drift silently.
rust = open(os.environ["RS_SRC"], encoding="utf-8").read()
m = re.search(r"pub fn is_council_provider_env_key\b.*?\n\}", rust, re.S)
assert m, "is_council_provider_env_key not found in private_config.rs"
fn = m.group(0)
head, _, tail = fn.partition("return false")
assert tail, "denylist arm not found"
deny = set(re.findall(r'"([A-Z0-9_]+)"', head))
vertex_src = fn.partition("matches!")[2]
assert vertex_src, "vertex allow-list arm not found"
vertex = set(re.findall(r'"([A-Z0-9_]+)"', vertex_src))
assert 'ends_with("_API_KEY")' in fn and '"OPENAI_ADMIN_KEY"' in fn, \
    "suffix/admin rule changed in Rust; update this mirror"
assert deny and vertex, "extracted empty sets from private_config.rs"

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
