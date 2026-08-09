-- ==========================================================================
-- watch_metric_keys_test.lua — length-prefix keys + label escape (H2)
--
-- RUN: from gateway/ root with package.path including lua/:
--   lua test/watch_metric_keys_test.lua
--   (or make -C gateway lua-unit after wiring)
-- ==========================================================================

package.path = "./?.lua;./lua/?.lua;./lua/?/init.lua;" .. package.path
local keys = require("lua.lib.watch_metric_keys")

local failures = 0
local function check(cond, msg)
    if cond then
        print("  ok   - " .. msg)
    else
        failures = failures + 1
        print("  FAIL - " .. msg)
    end
end

print("[1] round-trip encode/decode with colon in tenant")
do
    local tenant = "a:b"
    local sentinel = "silence-watch"
    local blob = keys.lp_encode({ tenant, sentinel })
    local parts = keys.lp_decode_all(blob)
    check(parts ~= nil, "decode succeeds")
    check(parts and parts[1] == tenant, "tenant with colon preserved")
    check(parts and parts[2] == sentinel, "sentinel preserved")
end

print("[2] prom_escape_label escapes backslash quote newline")
do
    local raw = 'x\\y"z\nw'
    local esc = keys.prom_escape_label(raw)
    check(esc == 'x\\\\y\\"z\\nw', "escape shape: " .. esc)
end

print("[3] hostile tenant renders safe exposition fragment")
do
    local tenant = 'evil":0,x="'
    local esc = keys.prom_escape_label(tenant)
    local line = string.format('gw_watch_temperature{tenant="%s"} 0.1', esc)
    check(not line:find('tenant="evil"'), "raw quote cannot close label early")
    check(line:find("\\\\") or line:find('\\"'), "escape present in line")
end

print("[4] is_labeled_watch_key prefixes")
do
    check(keys.is_labeled_watch_key("watch_temperature:3:abc"), "temperature")
    check(keys.is_labeled_watch_key("watch_sentinel_fires:1:x"), "fires")
    check(not keys.is_labeled_watch_key("watch_arm_rejected_unauth_total"), "unlabeled")
end

if failures > 0 then
    print(string.format("\n%d failure(s)", failures))
    os.exit(1)
end
print("\nOK: watch_metric_keys")
os.exit(0)
