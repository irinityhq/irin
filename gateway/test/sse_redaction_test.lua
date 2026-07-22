-- ==========================================================================
-- sse_redaction_test.lua — regression tripwire for lua/lib/sse.lua strict logs
--
-- WHY: sse.lua's strict-mode diagnostics (process_line) fire on the raw
-- upstream SSE stream BEFORE lua/cost.lua's credential_scrub runs. The T24
-- redaction audit flagged that the pre-scrub WARN lines at sse.lua:96/:117
-- emitted up to 80 raw bytes of upstream content, so a secret riding a
-- malformed line or an unknown field value could reach the error log unscrubbed.
-- The fix logs LENGTH ONLY (never content); the field name at :117 is kept
-- because it is an SSE protocol token, not upstream content. This is the
-- tripwire that a future edit re-introducing raw content trips.
--
-- RUN: `lua test/sse_redaction_test.lua` from the gateway repo root
--      (or `make lua-unit`). Mocks the ngx.log sink and asserts no planted
--      sentinel value survives into any captured log line.
-- ==========================================================================

package.path = "./?.lua;" .. package.path

-- Minimal ngx mock: capture every log line as a single concatenated string.
local captured = {}
_G.ngx = {
    WARN = "WARN",
    DEBUG = "DEBUG",
    ERR = "ERR",
    log = function(_level, ...)
        local parts = {}
        for i = 1, select("#", ...) do
            parts[#parts + 1] = tostring((select(i, ...)))
        end
        captured[#captured + 1] = table.concat(parts)
    end,
}

local sse = require("lua.lib.sse")

local failures = 0
local function check(cond, msg)
    if cond then
        print("  ok   - " .. msg)
    else
        failures = failures + 1
        print("  FAIL - " .. msg)
    end
end

local function all_logs()
    return table.concat(captured, "\n")
end

-- 1. SECURITY INVARIANT: a malformed (no-colon) line's raw content never logs.
print("[1] malformed no-colon line — content never reaches the log")
do
    captured = {}
    local parser = sse.new({ strict = true })
    parser:feed("SENTINEL_NOCOLON_SECRET\n")
    local logs = all_logs()
    check(not logs:find("SENTINEL_NOCOLON_SECRET", 1, true),
          "raw no-colon line body not present in any log line")
    check(logs:find("length", 1, true) ~= nil,
          "length-only diagnostic emitted for malformed line")
end

-- 2. SECURITY INVARIANT: an unknown field's VALUE never logs; the field NAME
--    (a protocol token) is kept for diagnosability.
print("[2] unknown field — value redacted, field name (protocol token) kept")
do
    captured = {}
    local parser = sse.new({ strict = true })
    parser:feed("x-leak-field: SENTINEL_FIELD_VALUE\n")
    local logs = all_logs()
    check(not logs:find("SENTINEL_FIELD_VALUE", 1, true),
          "unknown-field value not present in any log line")
    check(logs:find("x-leak-field", 1, true) ~= nil,
          "field name (protocol token) retained for diagnosability")
    check(logs:find("value length", 1, true) ~= nil,
          "value length-only diagnostic emitted for unknown field")
end

-- 3. NON-STRICT: no diagnostics at all (behavior unchanged by the fix).
print("[3] non-strict mode stays silent")
do
    captured = {}
    local parser = sse.new({ strict = false })
    parser:feed("SENTINEL_QUIET\n")
    parser:feed("x-leak-field: SENTINEL_QUIET\n")
    check(#captured == 0, "no strict diagnostics emitted when strict = false")
end

print("")
if failures == 0 then
    print("PASS sse_redaction (no-colon + unknown-field + non-strict)")
    os.exit(0)
else
    print("FAIL sse_redaction — " .. failures .. " assertion(s) failed")
    os.exit(1)
end
