-- cost_capture_test.lua — response capture vs usage vs settlement.
--
-- Replays fixed buffered and streaming bodies through cost.capture_body and
-- cost.account. Chunk boundaries must not change client bytes, native vs
-- normalized buffers, usage, cost, the normalized-body ledger hash, or
-- budget/cache settlement.
--
-- Run from gateway/: `make lua-unit` (or `lua test/cost_capture_test.lua`).
package.path = "./?.lua;./lua/?.lua;" .. package.path

local json = {}

local function skip_ws(s, i)
    while true do
        local c = s:sub(i, i)
        if c ~= " " and c ~= "\n" and c ~= "\r" and c ~= "\t" then
            return i
        end
        i = i + 1
    end
end

local function utf8_char(cp)
    if cp < 0x80 then return string.char(cp) end
    if cp < 0x800 then
        return string.char(0xC0 + math.floor(cp / 0x40), 0x80 + (cp % 0x40))
    end
    return string.char(
        0xE0 + math.floor(cp / 0x1000),
        0x80 + (math.floor(cp / 0x40) % 0x40),
        0x80 + (cp % 0x40))
end

local function parse_string(s, i)
    i = i + 1
    local out = {}
    while i <= #s do
        local c = s:sub(i, i)
        if c == '"' then
            return table.concat(out), i + 1
        elseif c == "\\" then
            local n = s:sub(i + 1, i + 1)
            local map = {
                ['"'] = '"', ["\\"] = "\\", ["/"] = "/",
                b = "\b", f = "\f", n = "\n", r = "\r", t = "\t",
            }
            if map[n] then
                out[#out + 1] = map[n]
                i = i + 2
            elseif n == "u" then
                local hex = s:sub(i + 2, i + 5)
                local cp = tonumber(hex, 16)
                if not cp then return nil, i, "bad unicode" end
                out[#out + 1] = utf8_char(cp)
                i = i + 6
            else
                return nil, i, "bad escape"
            end
        else
            out[#out + 1] = c
            i = i + 1
        end
    end
    return nil, i, "unterminated string"
end

local function parse_number(s, i)
    local rest = s:sub(i)
    local token = rest:match("^%-?%d+%.%d+[eE][+%-]?%d+")
        or rest:match("^%-?%d+[eE][+%-]?%d+")
        or rest:match("^%-?%d+%.%d+")
        or rest:match("^%-?%d+")
    if not token then return nil, i, "bad number" end
    return tonumber(token), i + #token
end

local parse_value

local function parse_array(s, i)
    i = skip_ws(s, i + 1)
    local arr = {}
    if s:sub(i, i) == "]" then return arr, i + 1 end
    while true do
        local v, ni, err = parse_value(s, i)
        if err then return nil, ni, err end
        arr[#arr + 1] = v
        i = skip_ws(s, ni)
        local c = s:sub(i, i)
        if c == "]" then return arr, i + 1 end
        if c ~= "," then return nil, i, "bad array" end
        i = skip_ws(s, i + 1)
    end
end

local function parse_object(s, i)
    i = skip_ws(s, i + 1)
    local obj = {}
    if s:sub(i, i) == "}" then return obj, i + 1 end
    while true do
        if s:sub(i, i) ~= '"' then return nil, i, "bad key" end
        local key, ni, err = parse_string(s, i)
        if err then return nil, ni, err end
        i = skip_ws(s, ni)
        if s:sub(i, i) ~= ":" then return nil, i, "bad colon" end
        local v, vi, verr = parse_value(s, i + 1)
        if verr then return nil, vi, verr end
        obj[key] = v
        i = skip_ws(s, vi)
        local c = s:sub(i, i)
        if c == "}" then return obj, i + 1 end
        if c ~= "," then return nil, i, "bad object" end
        i = skip_ws(s, i + 1)
    end
end

parse_value = function(s, i)
    i = skip_ws(s, i)
    local c = s:sub(i, i)
    if c == '"' then return parse_string(s, i) end
    if c == "{" then return parse_object(s, i) end
    if c == "[" then return parse_array(s, i) end
    if s:sub(i, i + 3) == "true" then return true, i + 4 end
    if s:sub(i, i + 4) == "false" then return false, i + 5 end
    if s:sub(i, i + 3) == "null" then return nil, i + 4 end
    if c == "-" or c:match("%d") then return parse_number(s, i) end
    return nil, i, "bad value"
end

local function escape_str(s)
    return '"' .. s:gsub('[%z\1-\31\\"]', function(c)
        local map = {
            ['"'] = '\\"', ["\\"] = "\\\\",
            ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t",
        }
        return map[c] or string.format("\\u%04x", c:byte())
    end) .. '"'
end

local function is_array(t)
    local n, max = 0, 0
    for k in pairs(t) do
        if type(k) ~= "number" or k < 1 or k ~= math.floor(k) then
            return false
        end
        n = n + 1
        if k > max then max = k end
    end
    return n > 0 and n == max
end

local function encode_number(n)
    if n ~= n or n == math.huge or n == -math.huge then return "null" end
    if n == math.floor(n) and math.abs(n) < 1e15 then
        return string.format("%.0f", n)
    end
    return string.format("%.17g", n)
end

local function encode(v)
    local tv = type(v)
    if v == nil then return "null" end
    if tv == "boolean" then return v and "true" or "false" end
    if tv == "number" then return encode_number(v) end
    if tv == "string" then return escape_str(v) end
    if tv ~= "table" then return "null" end
    if is_array(v) then
        local parts = {}
        for i = 1, #v do parts[i] = encode(v[i]) end
        return "[" .. table.concat(parts, ",") .. "]"
    end
    local keys = {}
    for k, val in pairs(v) do
        if val ~= nil then keys[#keys + 1] = k end
    end
    table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
    local parts = {}
    for _, k in ipairs(keys) do
        parts[#parts + 1] = escape_str(tostring(k)) .. ":" .. encode(v[k])
    end
    return "{" .. table.concat(parts, ",") .. "}"
end

function json.decode(s)
    if type(s) ~= "string" or s == "" then return nil, "empty" end
    local v, _, err = parse_value(s, 1)
    if err then return nil, err end
    return v
end

json.encode = encode

package.preload["cjson.safe"] = function() return json end

local logs = {}
local scheduled, recorded, budget_records, cache_stores = {}, {}, {}, {}
local hashed = {}

local function fingerprint(body)
    if not body or body == "" then return "" end
    local acc = 0
    for i = 1, #body do
        acc = (acc * 131 + body:byte(i)) % 1000000007
    end
    return string.format("%d:%d", #body, acc)
end

local function working_timer_at(_, fn)
    fn(false)
    return true
end

local metrics_store = {}
local function metrics_incr(_, key, value, init)
    metrics_store[key] = (metrics_store[key] or init or 0) + (value or 0)
    return metrics_store[key]
end

_G.ngx = {
    status = 200,
    now = function() return 1000 end,
    localtime = function() return "2026-01-01T00:00:00" end,
    log = function(_, ...)
        local parts = {}
        for i = 1, select("#", ...) do
            parts[#parts + 1] = tostring(select(i, ...))
        end
        logs[#logs + 1] = table.concat(parts)
    end,
    INFO = 1, WARN = 2, ERR = 3, DEBUG = 4,
    shared = { gw_metrics = { incr = metrics_incr } },
    var = {},
    header = {},
    timer = { at = working_timer_at },
    ctx = {},
    arg = {},
}

package.preload["lib.hash"] = function()
    return {
        body_sha256_hex = function(body)
            hashed[#hashed + 1] = body
            return fingerprint(body)
        end,
    }
end
package.preload["lib.ledger"] = function()
    return {
        record_with_retry = function(_, _, payload, metadata)
            recorded[#recorded + 1] = { payload = payload, metadata = metadata }
        end,
        schedule = function(action, _, fn)
            return ngx.timer.at(0, function(premature)
                scheduled[#scheduled + 1] = action
                fn(premature)
            end)
        end,
    }
end
package.preload["sidecar"] = function()
    local noop = function() end
    return {
        route_outcome = noop,
        budget_record = function(key, actual, estimate)
            budget_records[#budget_records + 1] = {
                key = key, actual = actual, estimate = estimate,
            }
        end,
        cache_store = function(alias, raw_body, body, provider, version, _, sensitivity)
            cache_stores[#cache_stores + 1] = {
                alias = alias, raw_body = raw_body, body = body,
                provider = provider, version = version, sensitivity = sensitivity,
            }
        end,
        council_unlock = noop, council_idempotency_store = noop,
        council_idempotency_fail = noop, council_stats = noop, watch_stats = noop,
    }
end

local translator = require("translator")
local cost = require("cost")

local failures = 0
local function check(cond, msg)
    if cond then
        print("  ok   - " .. msg)
    else
        failures = failures + 1
        print("  FAIL - " .. msg)
    end
end

local function reset()
    logs, scheduled, recorded, budget_records, cache_stores, hashed = {}, {}, {}, {}, {}, {}
    metrics_store = {}
    ngx.status = 200
    ngx.ctx = {}
    ngx.arg = {}
    ngx.header = {}
    ngx.var = {}
    ngx.timer.at = working_timer_at
end

local function audit_from_logs()
    for i = #logs, 1, -1 do
        local payload = logs[i]:match("^cost: (%b{})$")
        if payload then return json.decode(payload) end
    end
    return nil
end

local function base_record(over)
    local rec = {
        provider = "openai",
        request_id = "req-1",
        raw_body = string.rep("q", 40),
        pricing = { input = 1000, output = 2000 },
        budget_key = "bud",
        budget_estimated_usd = 0.05,
        caller_key = "caller",
        alias = "alias-1",
        resolved_model = "model-1",
        sensitivity = "internal",
        t0 = 1,
        upstream_path = "/v1/chat/completions",
    }
    for k, v in pairs(over or {}) do rec[k] = v end
    return rec
end

local function replay(fields, parts, opts)
    opts = opts or {}
    reset()
    ngx.status = opts.status or 200
    ngx.ctx.gw = { record = base_record(fields) }
    -- Suppressed chunks set ngx.arg[1] to nil. Collect immediately: a hole
    -- at index 1 makes the length operator drop later emissions.
    local client_parts = {}
    for i, part in ipairs(parts) do
        ngx.arg = { part, i == #parts }
        cost.capture_body()
        if ngx.arg[1] ~= nil then
            client_parts[#client_parts + 1] = ngx.arg[1]
        end
    end
    cost.account()
    return {
        client = table.concat(client_parts),
        native = ngx.ctx.gw_response_buf_native,
        normalized = ngx.ctx.gw_response_buf_normalized,
        row = recorded[1],
        audit = audit_from_logs(),
        capped = ngx.ctx.gw_response_capped == true,
        redacted = ngx.ctx.gw_credentials_redacted == true,
        streaming_usage = ngx.ctx.gw_streaming_usage,
    }
end

local function splits(body, include_bytes)
    local half = math.max(1, math.floor(#body / 2))
    local out = {
        { name = "whole", parts = { body } },
        { name = "half", parts = { body:sub(1, half), body:sub(half + 1) } },
    }
    if include_bytes then
        local bytes = {}
        for i = 1, #body do bytes[i] = body:sub(i, i) end
        out[#out + 1] = { name = "bytes", parts = bytes }
    end
    return out
end

local function same_settlement(a, b, label)
    check(a.client == b.client, label .. ": client output matches across " .. b.split_name)
    check(a.native == b.native, label .. ": native buffer matches across " .. b.split_name)
    check(a.normalized == b.normalized, label .. ": normalized buffer matches across " .. b.split_name)
    check(a.row ~= nil and b.row ~= nil, label .. ": both splits settle a ledger row")
    if not a.row or not b.row then return end
    check(a.row.payload.tokens_in == b.row.payload.tokens_in, label .. ": tokens_in stable")
    check(a.row.payload.tokens_out == b.row.payload.tokens_out, label .. ": tokens_out stable")
    check(a.row.payload.cost_usd == b.row.payload.cost_usd, label .. ": cost stable")
    check(a.row.payload.response_body_sha256 == b.row.payload.response_body_sha256,
        label .. ": normalized hash stable")
    check(a.row.metadata.action == "outbound_response", label .. ": action stays outbound_response")
end

local function expect_hash(result, label)
    check(result.row ~= nil, label .. ": ledger row present")
    if not result.row then return end
    local want = fingerprint(result.normalized or "")
    check(result.row.payload.response_body_sha256 == want,
        label .. ": ledger hashes the normalized body")
    if result.native ~= result.normalized then
        check(result.row.payload.response_body_sha256 ~= fingerprint(result.native or ""),
            label .. ": ledger hash is not the native body")
        check(#hashed == 1 and hashed[1] == result.normalized,
            label .. ": hash input is the normalized buffer only")
    end
end

local function expect_budget(result, label)
    check(#budget_records == 1, label .. ": one budget settlement")
    if budget_records[1] and result.row then
        check(budget_records[1].key == "bud", label .. ": budget key")
        check(budget_records[1].actual == result.row.payload.cost_usd, label .. ": budget actual is cost")
        check(budget_records[1].estimate == 0.05, label .. ": budget estimate retained")
    end
end

print("[json] codec roundtrip")
do
    local sample = { usage = { prompt_tokens = 3, completion_tokens = 5 }, ok = true, items = { "a", "b" } }
    local again = json.decode(json.encode(sample))
    check(again and again.usage.prompt_tokens == 3 and again.items[2] == "b" and again.ok == true,
        "json codec roundtrips objects, arrays, numbers, and bools")
    check(json.decode("{") == nil, "malformed json decodes to nil")
end

print("[1] buffered passthrough — chunk boundaries, usage, hash, cache")
do
    local body = json.encode({
        id = "c1",
        choices = { { message = { role = "assistant", content = "hi" } } },
        usage = { prompt_tokens = 3, completion_tokens = 5 },
    })
    local runs = {}
    for _, split in ipairs(splits(body, true)) do
        local result = replay({ provider = "openai" }, split.parts)
        result.split_name = split.name
        runs[#runs + 1] = result
        check(result.client == body, "passthrough " .. split.name .. " client is the upstream body")
        check(result.native == body and result.normalized == body,
            "passthrough " .. split.name .. " keeps native and normalized identical")
        check(result.row and result.row.payload.tokens_in == 3 and result.row.payload.tokens_out == 5,
            "passthrough " .. split.name .. " usage comes from the native body")
        check(result.row and result.row.payload.cost_usd == (3 * 1000 + 5 * 2000) / 1000000,
            "passthrough " .. split.name .. " cost uses record pricing")
        expect_hash(result, "passthrough " .. split.name)
        expect_budget(result, "passthrough " .. split.name)
        check(#cache_stores == 1 and cache_stores[1].body.usage.prompt_tokens == 3
                and cache_stores[1].provider == "openai",
            "passthrough " .. split.name .. " caches the decoded native body")
        check(result.audit and result.audit.tokens_in == 3 and result.audit.capped == false
                and result.audit.unparsed == false,
            "passthrough " .. split.name .. " audit matches parsed usage")
    end
    same_settlement(runs[1], runs[2], "passthrough half")
    same_settlement(runs[1], runs[3], "passthrough bytes")
end

print("[2] buffered passthrough credential — native kept, client/ledger scrubbed, no cache")
do
    local body = json.encode({
        choices = { { message = { role = "assistant", content = "key sk-LEAKKEY99" } } },
        usage = { prompt_tokens = 2, completion_tokens = 2 },
    })
    local result = replay({ provider = "openai" }, { body:sub(1, 20), body:sub(21) })
    check(result.native == body, "credential native buffer stays unscrubbed")
    check(result.normalized ~= body and result.normalized:find("sk%-LEAKKEY99", 1) == nil,
        "credential normalized buffer is scrubbed")
    check(result.normalized:find("[REDACTED:openai_key]", 1, true) ~= nil,
        "credential normalized buffer carries the redaction token")
    check(result.client:find("[REDACTED:openai_key]", 1, true) ~= nil, "client sees the scrubbed body")
    check(result.redacted, "credential leak sets the redaction flag")
    check(#cache_stores == 0, "redacted response is not cached")
    expect_hash(result, "credential passthrough")
    check(result.row and result.row.payload.tokens_in == 2, "scrub does not change native usage")
end

print("[3] buffered anthropic translation — native usage, normalized client")
do
    local native_obj = {
        id = "msg_1",
        model = "claude",
        stop_reason = "end_turn",
        content = { { type = "text", text = "hi" } },
        usage = { input_tokens = 7, output_tokens = 9 },
    }
    local body = json.encode(native_obj)
    local expected = json.encode(translator.translate_response("anthropic", json.decode(body)))
    local runs = {}
    for _, split in ipairs(splits(body, true)) do
        local result = replay({
            provider = "anthropic",
            needs_response_translation = true,
        }, split.parts)
        result.split_name = split.name
        runs[#runs + 1] = result
        check(result.client == expected, "translated " .. split.name .. " client is the normalized body")
        check(result.native == body, "translated " .. split.name .. " native buffer is upstream JSON")
        check(result.normalized == expected and result.normalized ~= result.native,
            "translated " .. split.name .. " normalized buffer is not the native body")
        check(result.row and result.row.payload.tokens_in == 7 and result.row.payload.tokens_out == 9,
            "translated " .. split.name .. " usage is parsed from native fields")
        expect_hash(result, "translated " .. split.name)
        check(#cache_stores == 1 and cache_stores[1].body.usage.input_tokens == 7
                and cache_stores[1].body.content[1].text == "hi",
            "translated " .. split.name .. " caches native content, not the client shape")
    end
    same_settlement(runs[1], runs[2], "translated half")
    same_settlement(runs[1], runs[3], "translated bytes")
end

print("[4] buffered responses reshape — native chat body, client output[]")
do
    local native_obj = {
        id = "c1",
        choices = { { message = { role = "assistant", content = "yo" }, finish_reason = "stop" } },
        usage = { prompt_tokens = 4, completion_tokens = 1 },
    }
    local body = json.encode(native_obj)
    local expected = json.encode(translator.denormalize_messages_to_responses(json.decode(body)))
    local runs = {}
    for _, split in ipairs(splits(body, false)) do
        local result = replay({ provider = "openai", is_responses_api = true }, split.parts)
        result.split_name = split.name
        runs[#runs + 1] = result
        check(result.client == expected, "reshape " .. split.name .. " client is Responses shape")
        check(result.native == body, "reshape " .. split.name .. " native stays chat JSON")
        check(result.normalized == expected and result.normalized:find('"output"', 1, true) ~= nil,
            "reshape " .. split.name .. " normalized buffer is the client shape")
        check(result.row and result.row.payload.tokens_in == 4 and result.row.payload.tokens_out == 1,
            "reshape " .. split.name .. " usage still comes from the native body")
        expect_hash(result, "reshape " .. split.name)
    end
    same_settlement(runs[1], runs[2], "reshape half")
end

print("[5] streaming passthrough — boundaries, usage frame, native hash")
do
    local body = table.concat({
        'data: {"choices":[{"delta":{"content":"hi"}}]}\n\n',
        'data: {"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":2,"total_tokens":13}}\n\n',
        "data: [DONE]\n\n",
    })
    local runs = {}
    for _, split in ipairs(splits(body, true)) do
        local result = replay({ provider = "openai", is_streaming = true }, split.parts)
        result.split_name = split.name
        runs[#runs + 1] = result
        check(result.client == body, "stream passthrough " .. split.name .. " forwards upstream bytes")
        check(result.native == body and result.normalized == body,
            "stream passthrough " .. split.name .. " seals both buffers to upstream SSE")
        check(result.row and result.row.payload.tokens_in == 11 and result.row.payload.tokens_out == 2,
            "stream passthrough " .. split.name .. " usage comes from the SSE usage frame")
        check(#cache_stores == 0, "stream passthrough " .. split.name .. " does not cache")
        expect_hash(result, "stream passthrough " .. split.name)
        check(result.audit and result.audit.is_streaming == true, "stream audit marks is_streaming")
    end
    same_settlement(runs[1], runs[2], "stream passthrough half")
    same_settlement(runs[1], runs[3], "stream passthrough bytes")
end

print("[6] streaming responses wrap — client events differ from native SSE")
do
    local body = table.concat({
        'data: {"choices":[{"delta":{"content":"hi"}}]}\n\n',
        'data: {"choices":[],"usage":{"prompt_tokens":6,"completion_tokens":1,"total_tokens":7}}\n\n',
        "data: [DONE]\n\n",
    })
    local runs = {}
    for _, split in ipairs(splits(body, true)) do
        local result = replay({
            provider = "openai",
            is_streaming = true,
            is_responses_api = true,
            upstream_path = "/v1/chat/completions",
        }, split.parts)
        result.split_name = split.name
        runs[#runs + 1] = result
        check(result.client:find("response.created", 1, true) ~= nil
                and result.client:find("response.completed", 1, true) ~= nil
                and result.client:find("hi", 1, true) ~= nil,
            "wrap " .. split.name .. " emits Responses events including the text")
        check(result.client ~= body, "wrap " .. split.name .. " client is not the upstream SSE")
        check(result.native == body and result.normalized == body,
            "wrap " .. split.name .. " native and normalized stay the upstream SSE")
        check(result.row and result.row.payload.tokens_in == 6 and result.row.payload.tokens_out == 1,
            "wrap " .. split.name .. " settles streaming usage")
        expect_hash(result, "wrap " .. split.name)
        check(#cache_stores == 0, "wrap " .. split.name .. " does not cache a stream")
    end
    same_settlement(runs[1], runs[2], "wrap half")
    same_settlement(runs[1], runs[3], "wrap bytes")
end

print("[7] streaming translation and responses wrap together")
do
    local events = {
        'event: message_start\ndata: {"type":"message_start","message":{"id":"msg_1","model":"claude","usage":{"input_tokens":4}}}\n\n',
        'event: content_block_delta\ndata: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}\n\n',
        'event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}\n\n',
        'event: message_stop\ndata: {"type":"message_stop"}\n\n',
    }
    local body = table.concat(events)
    local plain = replay({ provider = "anthropic", is_streaming = true }, { body })
    local wrapped = replay({
        provider = "anthropic",
        is_streaming = true,
        is_responses_api = true,
    }, { body })
    check(plain.client:find("chat.completion.chunk", 1, true) ~= nil
            and plain.client:find("hi", 1, true) ~= nil,
        "translated stream emits OpenAI chunks")
    check(plain.native == body and plain.normalized == body,
        "translated stream buffers stay upstream bytes")
    check(plain.row and plain.row.payload.tokens_in == 4 and plain.row.payload.tokens_out == 2,
        "translated stream usage comes from the translator, not the client JSON")
    check(plain.client ~= plain.native, "translated client bytes differ from the native buffer")
    expect_hash(plain, "translated stream")
    check(wrapped.client:find("response.output_text.delta", 1, true) ~= nil
            or wrapped.client:find("response.created", 1, true) ~= nil,
        "translated+wrap emits Responses events")
    check(wrapped.native == body, "translated+wrap native buffer is still upstream SSE")
    check(wrapped.row and wrapped.row.payload.tokens_in == 4 and wrapped.row.payload.tokens_out == 2,
        "translated+wrap keeps translator usage")
    local half = replay({ provider = "anthropic", is_streaming = true }, {
        body:sub(1, math.floor(#body / 2)),
        body:sub(math.floor(#body / 2) + 1),
    })
    check(half.client == plain.client and half.native == plain.native
            and half.row.payload.tokens_in == plain.row.payload.tokens_in
            and half.row.payload.response_body_sha256 == plain.row.payload.response_body_sha256,
        "translated stream half-split matches the single chunk")
end

print("[8] usage-only events and the chunk/eof usage predicates")
do
    local only = 'data: {"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":1,"total_tokens":5}}\n\n'
    local result = replay({ provider = "openai", is_streaming = true }, { only })
    check(result.row and result.row.payload.tokens_in == 4 and result.row.payload.tokens_out == 1,
        "usage-only chat chunk settles prompt and completion tokens")

    local vertex = 'data: {"usageMetadata":{"promptTokenCount":6,"candidatesTokenCount":2}}\n\n'
    local vrun = replay({ provider = "vertex", is_streaming = true }, { vertex })
    check(vrun.client == "", "vertex usage-only chunk emits no client bytes")
    check(vrun.native == vertex, "vertex usage-only native buffer is the upstream frame")
    check(vrun.row and vrun.row.payload.tokens_in == 6 and vrun.row.payload.tokens_out == 2,
        "vertex usage-only event still settles usage")

    local total_only = 'data: {"usage":{"total_tokens":9}}\n\n'
    local on_chunk = replay({ provider = "openai", is_streaming = true }, { total_only })
    check(on_chunk.row and on_chunk.row.payload.tokens_in == 0 and on_chunk.row.payload.tokens_out == 0
            and on_chunk.audit and on_chunk.audit.tokens_estimated ~= true,
        "chunk-phase total_tokens-only usage is noted, so settlement does not estimate")

    reset()
    ngx.ctx.gw = { record = base_record({ provider = "openai", is_streaming = true }) }
    ngx.arg = { 'data: {"usage":{"total_tokens":9}}', false }
    cost.capture_body()
    ngx.arg = { "", true }
    cost.capture_body()
    cost.account()
    local row = recorded[1]
    check(row and row.metadata.tokens_estimated ~= true and row.payload.tokens_out == math.floor(#('data: {"usage":{"total_tokens":9}}') / 16),
        "eof-flush total_tokens-only usage is ignored and the stream is estimated")
end

print("[9] malformed data")
do
    local bad_then_good = table.concat({
        "data: {not-json\n\n",
        'data: {"choices":[{"delta":{"content":"ok"}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}\n\n',
    })
    local streamed = replay({ provider = "openai", is_streaming = true }, {
        bad_then_good:sub(1, 12),
        bad_then_good:sub(13),
    })
    check(streamed.row and streamed.row.payload.tokens_in == 1 and streamed.row.payload.tokens_out == 1,
        "malformed SSE frame does not drop a later usage frame")

    local buffered = replay({ provider = "openai" }, { "{not-json" })
    check(buffered.row and buffered.row.metadata.unparsed == true
            and buffered.row.metadata.tokens_estimated == true
            and buffered.row.payload.tokens_in == 10
            and buffered.row.payload.tokens_out == 0,
        "malformed buffered body still settles an input-only estimate")
    check(#cache_stores == 0, "malformed buffered body is not cached")
    check(scheduled[1] == "outbound_response", "malformed body still schedules outbound_response")
end

print("[10] capture limit")
do
    local huge = string.rep("a", 1048577)
    local streamed = replay({ provider = "openai", is_streaming = true }, { huge, "data: [DONE]\n\n" })
    check(streamed.capped, "streaming over-cap chunk sets the cap")
    check(streamed.native == nil, "streaming cap before eof does not seal buffers")
    check(streamed.row and streamed.row.payload.tokens_in == 10 and streamed.row.payload.tokens_out == 0,
        "streaming cap without a sealed body estimates from the request only")
    check(#cache_stores == 0, "streaming cap does not cache")

    reset()
    local buffered = replay({ provider = "openai" }, { huge })
    check(buffered.capped, "buffered over-cap chunk sets the cap")
    check(buffered.native == nil, "buffered cap returns before sealing")
    check(buffered.row and buffered.row.metadata.capped == true
            and buffered.row.metadata.tokens_estimated == true
            and buffered.row.payload.tokens_out == 0
            and buffered.row.payload.tokens_in == 10,
        "buffered cap settles an input-only estimated row")
    check(#cache_stores == 0, "buffered cap does not cache")
end

print("[11] disconnect before eof")
do
    reset()
    ngx.ctx.gw = { record = base_record({ provider = "openai", is_streaming = true }) }
    ngx.arg = { 'data: {"choices":[{"delta":{"content":"hi"}}]}\n\n', false }
    cost.capture_body()
    local client = ngx.arg[1]
    cost.account()
    local row = recorded[1]
    check(client == 'data: {"choices":[{"delta":{"content":"hi"}}]}\n\n',
        "disconnect still forwarded the chunk already received")
    check(ngx.ctx.gw_response_buf_native == nil, "disconnect does not seal native bytes")
    check(row and row.metadata.action == "outbound_response", "disconnect still settles")
    check(row and row.payload.tokens_in == 10 and row.payload.tokens_out == 0,
        "disconnect without usage estimates tokens and does not invent a hash")
    check(row and row.payload.response_body_sha256 == "", "disconnect ledger hash is empty")
    check(#cache_stores == 0, "disconnect does not cache")
    check(#budget_records == 1 and budget_records[1].actual == row.payload.cost_usd,
        "disconnect still settles the budget")
end

print("[12] incomplete final frame")
do
    local complete = 'data: {"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":3,"total_tokens":11}}\n\n'
    local mid = math.floor(#complete / 2)
    local split = replay({ provider = "openai", is_streaming = true }, {
        complete:sub(1, mid),
        complete:sub(mid + 1),
    })
    check(split.row and split.row.payload.tokens_in == 8 and split.row.payload.tokens_out == 3,
        "usage frame split across chunks is recovered")

    reset()
    ngx.ctx.gw = { record = base_record({ provider = "openai", is_streaming = true }) }
    ngx.arg = { 'data: {"usage":{"prompt_tokens":8,"completion_tokens":3}', true }
    cost.capture_body()
    cost.account()
    local row = recorded[1]
    check(row and row.payload.tokens_in == math.floor(40 / 4),
        "eof inside a JSON frame does not parse a partial usage object")
    check(row and row.payload.response_body_sha256 == fingerprint('data: {"usage":{"prompt_tokens":8,"completion_tokens":3}'),
        "incomplete frame is still the sealed native body")
end

if failures > 0 then
    print(string.format("cost_capture_test: %d failure(s)", failures))
    os.exit(1)
end
print("cost_capture_test: ok")
