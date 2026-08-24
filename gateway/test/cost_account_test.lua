-- cost_account_test.lua — B-09 / #0165 regression.
-- `cost.account()` must schedule the outbound_response ledger write even when
-- the native response body is empty or unparseable. Pre-fix it early-returned
-- and the request left no ledger row. Run from gateway/: `make lua-unit`.
package.path = "./?.lua;./lua/?.lua;" .. package.path

local scheduled, recorded, budget_records, cache_stores = {}, {}, {}, 0

local function working_timer_at(_, fn)
    fn(false)
    return true
end

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
        route_outcome = noop,
        budget_record = function(key, actual, estimate)
            budget_records[#budget_records + 1] = {
                key = key, actual = actual, estimate = estimate,
            }
        end,
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
            return ngx.timer.at(0, function(premature)
                scheduled[#scheduled + 1] = action
                fn(premature)
            end)
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
    timer = { at = working_timer_at },
    ctx = {},
}

local cost = require("cost")

local failures = 0
local function check(cond, msg)
    if cond then print("  ok   - " .. msg) else failures = failures + 1; print("  FAIL - " .. msg) end
end

local function run_record(record, native_body, error_code, status, timer_at)
    scheduled, recorded, budget_records, cache_stores = {}, {}, {}, 0
    ngx.timer.at = timer_at or working_timer_at
    ngx.status = status or 200
    ngx.ctx = {
        gw = { record = record },
        gw_response_buf_native = native_body,
        gw_error_code = error_code,
    }
    cost.account()
    return recorded[1]
end

local function run(native_body)
    return run_record({
        provider = "openai", request_id = "r1", raw_body = string.rep("x", 40),
        pricing = { input = 1, output = 1 }, budget_key = "default",
        budget_estimated_usd = 0.05,
        caller_key = "k", alias = "gpt", resolved_model = "gpt-test", t0 = 1,
    }, native_body)
end

local row = run("")
check(scheduled[1] == "outbound_response" and #scheduled == 1,
    "empty body schedules exactly one outbound_response")
check(row ~= nil and row.metadata.tokens_estimated == true, "empty body row is marked tokens_estimated")
check(row ~= nil and row.metadata.unparsed == true, "empty body row is marked unparsed")
check(row ~= nil and row.payload.tokens_in == 10 and row.payload.tokens_out == 0, "empty body uses input-only estimate")
check(cache_stores == 0, "empty body never reaches cache_store")
check(#budget_records == 1 and budget_records[1].key == "default"
        and math.abs(budget_records[1].actual - 0.00001) < 0.0000001
        and budget_records[1].estimate == 0.05,
    "empty body settles its estimate to the calculated actual cost")

row = run_record({
    request_id = "rejected-1", budget_key = "team-a",
    budget_estimated_usd = 0.05, t0 = 1,
}, nil, "ERR_TEST_REJECTED", 422)
check(scheduled[1] == "request_rejected" and #scheduled == 1,
    "unrouted request schedules exactly one request_rejected")
check(row ~= nil and row.payload.request_id == "rejected-1" and row.payload.status == 422,
    "request_rejected carries request id and response status")
check(row ~= nil and row.payload.error_code == "ERR_TEST_REJECTED",
    "request_rejected carries the handler error code")
check(row ~= nil and row.metadata.action == "request_rejected",
    "request_rejected records the terminating action")
check(#budget_records == 1 and budget_records[1].key == "team-a"
        and budget_records[1].actual == 0 and budget_records[1].estimate == 0.05,
    "unrouted request releases its budget estimate with zero actual cost")

row = run_record({
    request_id = "budget-check-rejected", budget_key = "team-a", t0 = 1,
}, nil, "ERR_BUDGET_EXCEEDED", 429)
check(#budget_records == 0,
    "rejected budget check without a retained estimate schedules no budget record")

row = run_record({
    request_id = "budget-release-timer-rejected",
    budget_key = "team-a",
    budget_estimated_usd = 0.05,
    t0 = 1,
}, nil, "ERR_POLICY_VIOLATION", 403,
    function() return nil, "timer pool full" end)
check(#budget_records == 0,
    "rejected budget release timer schedules no budget record")

local retry_record = { request_id = "rejected-retry", t0 = 1 }
row = run_record(retry_record, nil, "ERR_TIMER_REJECTED", 503,
    function() return nil, "timer pool full" end)
check(retry_record.chain_terminated ~= true and #scheduled == 0 and row == nil,
    "timer rejection leaves the unrouted request unterminated")
row = run_record(retry_record, nil, "ERR_TIMER_REJECTED", 503)
check(retry_record.chain_terminated == true and scheduled[1] == "request_rejected"
        and #scheduled == 1 and row ~= nil,
    "working scheduler can terminate the request after a timer rejection")

row = run_record({ request_id = "rejected-2", chain_terminated = true, t0 = 1 },
    nil, "ERR_ALREADY_TERMINATED")
check(#scheduled == 0 and row == nil, "already terminated unrouted request schedules nothing")

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
