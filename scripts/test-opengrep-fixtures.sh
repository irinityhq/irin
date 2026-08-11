#!/usr/bin/env bash
# Hermetic OpenGrep fixture contracts for PR #19 residual false-greens:
#   - nested Lua body keys must not silence top-level key rules
#   - Tauri governed spawn must consume gateway_creds, not only spell it
#
# Requires scripts/bootstrap-dev-tools.sh opengrep binary. No network.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/.irin-tools/bin/opengrep"
RULES="$ROOT/security/opengrep/rules"
FIX="$ROOT/security/opengrep/fixtures"
BOOTSTRAP="$ROOT/scripts/bootstrap-dev-tools.sh"
failures=0

pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; failures=$((failures + 1)); }

if [[ ! -x "$BIN" ]]; then
  bash "$BOOTSTRAP" >/dev/null
fi
[[ -x "$BIN" ]] || {
  printf 'test-opengrep-fixtures: opengrep missing at %s\n' "$BIN" >&2
  exit 1
}
[[ -d "$RULES" && -d "$FIX" ]] || {
  printf 'test-opengrep-fixtures: rules/fixtures missing\n' >&2
  exit 1
}

count_rule() {
  local file="$1" rule_suffix="$2"
  "$BIN" scan --config "$RULES" --disable-version-check --json --quiet "$file" 2>/dev/null \
    | python3 -c "
import json, sys
want = sys.argv[1]
data = json.load(sys.stdin)
n = 0
for r in data.get('results') or []:
    cid = r.get('check_id') or ''
    if cid.endswith(want) or want in cid:
        n += 1
print(n)
" "$rule_suffix"
}

# Nested metadata.raw_key must fire the top-level raw_key rule.
nested="$FIX/lua-auth-check-nested-raw_key.lua"
n="$(count_rule "$nested" "irin.lua.sidecar-auth-check-key-raw_key")"
if [[ "$n" -ge 1 ]]; then
  pass "lua nested raw_key fires auth-check raw_key rule (count=$n)"
else
  fail "lua nested raw_key false-green: expected raw_key rule finding"
fi

# Production-shaped ok must not fire raw_key or ip rules.
ok="$FIX/lua-auth-check-ok.lua"
n_raw="$(count_rule "$ok" "irin.lua.sidecar-auth-check-key-raw_key")"
n_ip="$(count_rule "$ok" "irin.lua.sidecar-auth-check-key-ip")"
if [[ "$n_raw" == "0" && "$n_ip" == "0" ]]; then
  pass "lua auth-check ok shape is clean"
else
  fail "lua auth-check ok shape unexpected findings raw=$n_raw ip=$n_ip"
fi

# Required keys plus an unrelated extra top-level field must stay clean.
extra="$FIX/lua-auth-check-extra-fields-ok.lua"
n_raw="$(count_rule "$extra" "irin.lua.sidecar-auth-check-key-raw_key")"
n_ip="$(count_rule "$extra" "irin.lua.sidecar-auth-check-key-ip")"
if [[ "$n_raw" == "0" && "$n_ip" == "0" ]]; then
  pass "lua auth-check extra-field ok shape is clean"
else
  fail "lua auth-check extra-field false red raw=$n_raw ip=$n_ip"
fi

# Key name only in a comment / string must still fire missing-field.
comment_only="$FIX/lua-auth-check-comment-only.lua"
string_only="$FIX/lua-auth-check-string-only.lua"
n_cmt="$(count_rule "$comment_only" "irin.lua.sidecar-auth-check-key-raw_key")"
n_str="$(count_rule "$string_only" "irin.lua.sidecar-auth-check-key-raw_key")"
if [[ "$n_cmt" -ge 1 ]]; then
  pass "lua comment-only raw_key fires missing-field (count=$n_cmt)"
else
  fail "lua comment-only raw_key false-green: expected missing-field finding"
fi
if [[ "$n_str" -ge 1 ]]; then
  pass "lua string-only raw_key fires missing-field (count=$n_str)"
else
  fail "lua string-only raw_key false-green: expected missing-field finding"
fi

# Param-only / drop / literal reinject must fire; $C-derived reinject must not.
param_only="$FIX/rust-spawn-param-only.rs"
param_used="$FIX/rust-spawn-param-used.rs"
drop_only="$FIX/rust-spawn-drop-creds.rs"
literal_key="$FIX/rust-spawn-literal-key.rs"
n_only="$(count_rule "$param_only" "irin.rust.tauri-governed-requires-creds-param")"
n_used="$(count_rule "$param_used" "irin.rust.tauri-governed-requires-creds-param")"
n_drop="$(count_rule "$drop_only" "irin.rust.tauri-governed-requires-creds-param")"
n_lit="$(count_rule "$literal_key" "irin.rust.tauri-governed-requires-creds-param")"
if [[ "$n_only" -ge 1 ]]; then
  pass "rust spawn param-only fires creds-use rule (count=$n_only)"
else
  fail "rust spawn param-only false-green: expected creds-use finding"
fi
if [[ "$n_drop" -ge 1 ]]; then
  pass "rust spawn drop-creds fires reinject rule (count=$n_drop)"
else
  fail "rust spawn drop-creds false-green: expected reinject finding"
fi
if [[ "$n_lit" -ge 1 ]]; then
  pass "rust spawn literal GW_API_KEY fires reinject rule (count=$n_lit)"
else
  fail "rust spawn literal GW_API_KEY false-green: expected reinject finding"
fi
if [[ "$n_used" == "0" ]]; then
  pass "rust spawn param-used is clean"
else
  fail "rust spawn param-used unexpected findings count=$n_used"
fi

# Production surfaces must stay clean under the strengthened rules.
prod_lua_n="$("$BIN" scan --config "$RULES/lua-sidecar-contract.yaml" \
  --disable-version-check --json --quiet gateway/lua/sidecar.lua 2>/dev/null \
  | python3 -c "import json,sys; print(len(json.load(sys.stdin).get('results') or []))")"
if [[ "$prod_lua_n" == "0" ]]; then
  pass "production gateway/lua/sidecar.lua clean under lua-sidecar-contract"
else
  fail "production sidecar.lua unexpected findings count=$prod_lua_n"
fi

prod_rs_n="$("$BIN" scan --config "$RULES/rust-tauri-spawn-env.yaml" \
  --disable-version-check --json --quiet \
  council-rs/warroom-tauri/src-tauri/src/sidecar.rs 2>/dev/null \
  | python3 -c "import json,sys; print(len(json.load(sys.stdin).get('results') or []))")"
if [[ "$prod_rs_n" == "0" ]]; then
  pass "production sidecar.rs clean under rust-tauri-spawn-env"
else
  fail "production sidecar.rs unexpected findings count=$prod_rs_n"
fi

if (( failures > 0 )); then
  printf 'opengrep fixture contracts: FAILED (%d)\n' "$failures" >&2
  exit 1
fi
printf 'opengrep fixture contracts: OK\n'
