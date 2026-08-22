-- cost_account_test.lua — B-09 / #0165 regression.
-- `cost.account()` must schedule the outbound_response ledger write even when
-- the native response body is empty or unparseable. Pre-fix it early-returned
-- and the request left no ledger row. Run from gateway/: `make lua-unit`.
package.path = "./?.lua;./lua/?.lua;" .. package.path

local scheduled, recorded, cache_stores = {}, {}, 0

package.preload["cjson.safe"] = function()
    return {
        decode = function(s)
            if type(s) ~= "string" or s == "" then return nil, "empty" end
            if s:sub(1, 1) == "{" then return { usage = { prompt_tokens = 3, completion_tokens = 5 } } end
            return nil, "Expected value but found invalid token"
        end,
        encode = function(t)
            local parts = {}
            for k, v in pairs(t) do parts[#parts + 1] = tostring(k) .. "=" .. tostring(v) end
            table.sort(parts)
            return "{" .. table.concat(parts, ",") .. "}"
        end,
    }
end
package.preload["lib.providers"] = function()
    return { extract_usage = function() return { tokens_in = 3, tokens_out = 5, cached_in = 0 } end }
end
package.preload["translator"] = function() return { TRANSLATOR_VERSION = "test" } end
package.preload["sidecar"] = function()
    local noop = function() end
    return {
        route_outcome = noop, budget_record = noop,
        cache_store = function() cache_stores = cache_stores + 1 end,
        council_unlock = noop, council_idempotency_store = noop,
        council_idempotency_fail = noop, council_stats = noop, watch_stats = noop,
    }
end
package.preload["lib.hash"] = function() return { body_sha256_hex = function() return "sha" end } end
package.preload["lib.ledger"] = function()
    return {
        record_with_retry = function(provider, direction, payload, metadata, caller)
            recorded[#recorded + 1] = { payload = payload, metadata = metadata }
        end,
        schedule = function(action, request_id, fn)
            scheduled[#scheduled + 1] = action
            fn(false)
        end,
    }
end
package.preload["lib.credential_scrub"] = function() return {} end
package.preload["lib.responses_stream"] = function() return {} end
package.preload["lib.watch_metric_keys"] = function() return {} end

_G.ngx = {
    status = 200,
    now = function() return 1 end,
    localtime = function() return "t" end,
    log = function() end,
    INFO = 1, WARN = 2, ERR = 3,
    shared = {},
    var = {},
    header = {},
    timer = { at = function(_, fn) fn(false) end },
    ctx = {},
}

local cost = require("cost")

local failures = 0
local function check(cond, msg)
    if cond then print("  ok   - " .. msg) else failures = failures + 1; print("  FAIL - " .. msg) end
end

local function run(native_body)
    scheduled, recorded, cache_stores = {}, {}, 0
    ngx.ctx = {
        gw = { record = {
            provider = "openai", request_id = "r1", raw_body = string.rep("x", 40),
            pricing = { input = 1, output = 1 }, budget_key = "default",
            caller_key = "k", alias = "gpt", resolved_model = "gpt-test", t0 = 1,
        } },
        gw_response_buf_native = native_body,
    }
    cost.account()
    return recorded[1]
end

local row = run("")
check(scheduled[1] == "outbound_response", "empty body still schedules outbound_response")
check(row ~= nil and row.metadata.tokens_estimated == true, "empty body row is marked tokens_estimated")
check(row ~= nil and row.metadata.unparsed == true, "empty body row is marked unparsed")
check(row ~= nil and row.payload.tokens_in == 10 and row.payload.tokens_out == 0, "empty body uses input-only estimate")
check(cache_stores == 0, "empty body never reaches cache_store")

row = run("not json")
check(scheduled[1] == "outbound_response", "unparseable body still schedules outbound_response")
check(row ~= nil and row.metadata.tokens_estimated == true, "unparseable body row is marked tokens_estimated")
check(row ~= nil and row.metadata.unparsed == true, "unparseable body row is marked unparsed")
check(row ~= nil and row.payload.tokens_in == 10 and row.payload.tokens_out == 0, "unparseable body uses input-only estimate")
check(cache_stores == 0, "unparseable body never reaches cache_store")

row = run('{"usage":{}}')
check(scheduled[1] == "outbound_response", "parseable body schedules outbound_response")
check(row ~= nil and row.metadata.tokens_estimated == false and row.metadata.unparsed == false, "parseable body row is not marked estimated or unparsed")
check(row ~= nil and row.payload.tokens_in == 3 and row.payload.tokens_out == 5, "parseable body keeps provider usage")
check(cache_stores == 1, "parseable success body reaches cache_store")

if failures > 0 then
    print(failures .. " failure(s)")
    os.exit(1)
end
print("cost_account_test: PASS")
