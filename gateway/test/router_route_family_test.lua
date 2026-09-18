-- router_route_family_test.lua — access-routing characterization.
--
-- Pins the per-route-family behavior of lua/router.lua: the order of
-- access-phase checks, upstream selection, credential stripping, response
-- statuses, and the ledger terminator rows. Written green against the
-- pre-PR4 router; refactors of the routing body must keep every assertion
-- true. Run from gateway/: `make lua-unit`.
package.path = "./?.lua;./lua/?.lua;" .. package.path

local S -- per-scenario state, rebuilt by reset()
local events, ledger_rows, scheduled, header_ops

local function rec(kind, name)
    events[#events + 1] = { kind = kind, name = name }
end

local function sidecar_calls()
    local out = {}
    for _, e in ipairs(events) do
        if e.kind == "sidecar" then out[#out + 1] = e.name end
    end
    return out
end

local function ledger_actions()
    local out = {}
    for _, r in ipairs(ledger_rows) do out[#out + 1] = r.meta.action end
    return out
end

local function last_ledger(action)
    for i = #ledger_rows, 1, -1 do
        if ledger_rows[i].meta.action == action then return ledger_rows[i] end
    end
    return nil
end

local function hdr_ops(op, name)
    for _, h in ipairs(header_ops) do
        if h[1] == op and h[2] == name then return h end
    end
    return nil
end

-- ---------------------------------------------------------------- stubs --
local real_getenv = os.getenv
local env_map = {
    GW_ENABLE_STREAMING      = "1",
    GW_ENABLE_BATCH          = "1",
    GW_ENABLE_COUNCIL_ENDPOINT = "1",
    COUNCIL_GATEWAY_KEY_ID   = "key-1",
    CLAUDE_PROXY_TOKEN       = "cliproxy-tok",
}

package.preload["cjson.safe"] = function()
    return {
        decode = function(s)
            S.decode_input = s
            if S.decoded == nil then return nil, "invalid json" end
            return S.decoded
        end,
        encode = function(t)
            local parts = {}
            for k, v in pairs(t) do
                parts[#parts + 1] = tostring(k) .. "=" .. tostring(v)
            end
            table.sort(parts)
            return "{" .. table.concat(parts, ",") .. "}"
        end,
    }
end
package.preload["resty.http"] = function()
    return { new = function() error("resty.http not exercised by this suite") end }
end
package.preload["config"] = function()
    local cfg = {
        providers = {
            openai = {
                base_url = "https://api.openai.local", auth_header = "Authorization",
                auth_prefix = "Bearer ",
                extra_headers = { ["x-openai-extra"] = "1" },
            },
            anthropic = {
                base_url = "https://api.anthropic.local", auth_header = "x-api-key",
                auth_prefix = "", extra_headers = { ["anthropic-version"] = "2023-06-01" },
            },
            council = {
                base_url = "http://council.local", auth_header = "X-Gateway-Auth",
                auth_prefix = "",
            },
            ["claude-cli"] = {
                base_url = "http://cli.local", auth_header = "Authorization",
                auth_prefix = "Bearer ",
            },
        },
        models = {
            ["gpt-test"] = { provider = "openai", pricing = { output = 2 },
                             path = "/v1/chat/completions" },
            ["claude-test"] = { provider = "anthropic", pricing = { output = 3 } },
            ["council-test"] = { provider = "council", pricing = {} },
            ["cli-test"] = { provider = "claude-cli", pricing = { output = 1 } },
        },
        aliases = {},
    }
    cfg.get_model = function(id) return cfg.models[id] end
    cfg.get_provider = function(m) return cfg.providers[m.provider] end
    cfg.resolve_model = function(alias)
        if S.model_unknown then return nil end
        return cfg.models[alias], alias
    end
    cfg.get_api_key = function(p)
        if p == "openai" then return "sk-openai" end
        if p == "anthropic" then return "sk-ant" end
        return "static-key"
    end
    return cfg
end
package.preload["sidecar"] = function()
    local function ok_auth()
        return { allowed = true, budget_key = "bk1", key_id = "key-1",
                 rate_limit_limit = 10, rate_limit_remaining = 9,
                 rate_limit_reset = 5 }
    end
    return {
        auth_check = function(key, ip)
            rec("sidecar", "auth_check")
            return S.auth_result or ok_auth()
        end,
        ip_check = function(ip)
            rec("sidecar", "ip_check")
            if S.ip_blocked then return { allowed = false, reason = "blocked ip" } end
            return { allowed = true }
        end,
        guard_input = function(text, src)
            rec("sidecar", "guard_input")
            return { blocked = S.guard_blocked == true,
                     blocked_reason = "threat detected", verdict = "verdict-x" }
        end,
        cache_check = function(alias, body, ver, sens)
            rec("sidecar", "cache_check")
            return S.cache_result
        end,
        route_decide = function(alias, req, strategy, sens, sov)
            rec("sidecar", "route_decide")
            if S.route_fail then return nil, "sidecar down" end
            return S.routing or {
                model_id = "gpt-test", requested_model = "gpt-test",
                effective_model = "gpt-test", strategy = "quality", score = 0.9,
            }
        end,
        budget_check = function(key, est)
            rec("sidecar", "budget_check")
            if S.budget_blocked then return { allowed = false, reason = "spent" } end
            return { allowed = true }
        end,
        policy_evaluate = function(provider, sens)
            rec("sidecar", "policy_evaluate")
            if S.policy_nil then
                return nil
            end
            if S.policy_blocked then
                return { allowed = false, dry_run = false, reason = "no",
                         level = "RED", detected_signals = {} }
            end
            return { allowed = true }
        end,
        council_idempotency_peek = function(ns, idem, sha)
            rec("sidecar", "peek")
            return S.peek
        end,
        council_lock = function(ns)
            rec("sidecar", "lock")
            return { granted = true, grant_id = "g1" }
        end,
        council_idempotency_claim = function(ns, idem, sha, rid)
            rec("sidecar", "claim")
            if S.claim_conflict then return { conflict = true } end
            return {}
        end,
        council_unlock = function(ns, gid)
            rec("sidecar", "unlock")
            return true
        end,
        vertex_token = function()
            rec("sidecar", "vertex_token")
            return { token = "adc-tok", source = "test" }
        end,
    }
end
package.preload["translator"] = function()
    return {
        TRANSLATOR_VERSION = "t1",
        needs_response_translation = function(p)
            return S.needs_translation == p
        end,
        translate_request = function(provider, req, model)
            rec("translator", "translate_request")
            return req, nil, nil
        end,
        translate_response = function(p, body) return body end,
        denormalize_messages_to_responses = function(b) return b end,
        normalize_responses_to_messages = function(req) end,
        set_target_path = function(p) end,
        set_budget_key = function(k) end,
        resolve_vertex_path = function(path, m) return path end,
    }
end
package.preload["lib.hash"] = function()
    return { body_sha256_hex = function(s) return "sha256:" .. #(s or "") end }
end
package.preload["lib.ledger"] = function()
    return {
        record_with_retry = function(actor, alias, payload, meta, caller)
            ledger_rows[#ledger_rows + 1] =
                { actor = actor, alias = alias, payload = payload,
                  meta = meta, caller = caller }
        end,
        schedule = function(action, request_id, fn)
            scheduled[#scheduled + 1] = action
            fn(false) -- eager, synchronous: premature=false
            return true
        end,
    }
end
package.preload["lib.shape_gate"] = function()
    return {
        validate = function(req, alias, size)
            rec("sidecar", "shape_gate")
            if S.shape_err then return S.shape_err end
            return nil
        end,
    }
end
package.preload["lib.content"] = function()
    return {
        extract_text = function(c, depth)
            if S.content_depth then return nil, "content_depth_exceeded" end
            return "user text", nil
        end,
    }
end
package.preload["lib.council_transport"] = function()
    return {
        is_trusted_council = function(role, key_id, env_key)
            rec("council_transport", "is_trusted_council")
            return role == "council" and key_id == env_key and key_id ~= nil
                   and key_id ~= ""
        end,
        matches = function(transport, provider)
            if S.transport_mismatch then return false, "transport_mismatch" end
            return true, nil
        end,
        is_local_provider = function(p) return true end,
        advertised_for_provider = function(p) return {} end,
    }
end
package.preload["cost"] = function()
    return {
        account_replay = function(record)
            rec("cost", "account_replay")
            error({ __ngx_exit = 200, replay = true })
        end,
    }
end

local EXIT_ERR = "__ngx_exit"

_G.ngx = {
    status = 200,
    var = {},
    header = {},
    ctx = {},
    INFO = 1, WARN = 2, ERR = 3, DEBUG = 4,
    log = function() end,
    now = function() return 1000 end,
    say = function(body) S.said[#S.said + 1] = body end,
    exit = function(status) error({ [EXIT_ERR] = status }) end,
    on_abort = function(fn) S.abort_fn = fn; return true end,
    shared = {
        gw_metrics = { incr = function(self, name)
            rec("metric", name)
        end },
    },
    req = {
        read_body = function() S.read_body_calls = S.read_body_calls + 1 end,
        get_body_data = function() return S.body end,
        get_body_file = function() return S.body_file end,
        set_body_data = function(b) S.set_body = b end,
        get_headers = function() return S.headers end,
        get_method = function() return S.method or "POST" end,
        set_header = function(name, v)
            S.headers[name] = v
            S.headers[name:lower()] = v
            header_ops[#header_ops + 1] = { "set", name, v }
        end,
        clear_header = function(name)
            S.headers[name] = nil
            S.headers[name:lower()] = nil
            header_ops[#header_ops + 1] = { "clear", name }
        end,
    },
    timer = { at = function(delay, fn) return true end },
}

io.open = function(path, mode)
    if S.body_file_content then
        return { read = function(_, _) return S.body_file_content end,
                 close = function() end }
    end
    return nil, "cannot open"
end

os.getenv = function(k)
    local v = env_map[k]
    if v ~= nil then return v end
    return real_getenv(k)
end

local function reset()
    S = {
        headers = { authorization = "Bearer rk-1" },
        body = '{"model":"gpt-test","messages":[{"role":"user","content":"hi"}]}',
        decoded = { model = "gpt-test",
                    messages = { { role = "user", content = "hi" } } },
        var = {
            uri = "/v1/chat/completions", request_method = "POST",
            request_id = "req-1", remote_addr = "127.0.0.1",
            is_args = "", args = "",
        },
        said = {}, read_body_calls = 0,
    }
    events, ledger_rows, scheduled, header_ops = {}, {}, {}, {}
    _G.ngx.ctx = {}
    _G.ngx.var = S.var
    _G.ngx.header = {}
    _G.ngx.status = 200
end

local function load_router(env_overrides)
    for k, v in pairs(env_overrides or {}) do env_map[k] = v end
    package.loaded["router"] = nil
    return require("router")
end

--- Run router.route(); returns the ngx.exit status (nil = proxy-through).
local function run_route()
    local router = load_router()
    local ok, err = pcall(router.route)
    if ok then return nil end
    if type(err) == "table" and err[EXIT_ERR] then
        return err[EXIT_ERR], err.replay
    end
    error(err) -- a real failure, not an emulated ngx.exit
end

-- --------------------------------------------------------------- assert --
local failures = 0
local function check(cond, msg)
    if not cond then
        failures = failures + 1
        print("FAIL: " .. msg)
    end
end
local function eq(actual, expected, msg)
    if type(expected) == "table" then
        if #actual ~= #expected then
            failures = failures + 1
            print("FAIL: " .. msg .. " (length " .. #actual .. " != " .. #expected .. ")")
            return
        end
        for i = 1, #expected do
            if actual[i] ~= expected[i] then
                failures = failures + 1
                print("FAIL: " .. msg .. " (at " .. i .. ": '"
                      .. tostring(actual[i]) .. "' != '" .. tostring(expected[i]) .. "')")
                return
            end
        end
        return
    end
    if actual ~= expected then
        failures = failures + 1
        print("FAIL: " .. msg .. " ('" .. tostring(actual) .. "' != '"
              .. tostring(expected) .. "')")
    end
end

local function default_record()
    return _G.ngx.ctx.gw and _G.ngx.ctx.gw.record or nil
end

-- ---------------------------------------------------------------- tests --
local function test_step0_rejections()
    reset()
    S.body = ""
    eq(run_route(), 400, "empty body status")
    reset()
    S.decoded = nil
    eq(run_route(), 400, "invalid JSON status")
    reset()
    S.decoded = { messages = {} }
    eq(run_route(), 400, "missing model status")
    reset()
    S.shape_err = { error = "request too large", field = "messages" }
    eq(run_route(), 400, "shape gate status")
    eq(sidecar_calls(), { "shape_gate" }, "shape gate runs before sidecar auth")
    reset()
    S.decoded = { model = "gpt-test", stream = "yes" }
    eq(run_route(), 400, "invalid stream field status")
end

local function test_body_file_fallback()
    reset()
    S.body = nil
    S.body_file = "/tmp/body.json"
    S.body_file_content = '{"model":"gpt-test"}'
    S.decoded = { model = "gpt-test", messages = { { role = "user", content = "hi" } } }
    eq(run_route(), nil, "body-file fallback reaches proxy path")
    eq(S.decode_input, S.body_file_content, "decoder receives body-file content")
    local record = default_record()
    eq(record.raw_body, '{"model":"gpt-test"}', "body file content becomes raw_body")
end

local function test_auth_rejections()
    reset()
    S.headers = {}
    eq(run_route(), 401, "missing key status")
    reset()
    S.auth_result = { allowed = false, reason = "bad key" }
    eq(run_route(), 401, "invalid key status")
    eq(sidecar_calls(), { "shape_gate", "auth_check" },
       "shape gate precedes auth_check")
    reset()
    S.auth_result = { allowed = false, reason = "slow down",
                      rate_limit_limit = 10, rate_limit_remaining = 0,
                      rate_limit_reset = 42 }
    eq(run_route(), 429, "rate limited status")
    eq(_G.ngx.header["X-RateLimit-Remaining"], 0, "rate limit remaining header")
    reset()
    S.ip_blocked = true
    eq(run_route(), 403, "ip blocked status")
    eq(sidecar_calls(), { "shape_gate", "auth_check", "ip_check" }, "ip_check after auth_check")
end

local function test_content_depth()
    reset()
    S.content_depth = true
    eq(run_route(), 400, "content depth status")
end

local function test_guard_block()
    reset()
    S.guard_blocked = true
    eq(run_route(), 403, "guard block status")
    eq(ledger_actions(), { "request_received", "guard_input" },
       "guard termination ledger order")
    local row = last_ledger("guard_input")
    eq(row.meta.decision, "blocked", "guard ledger decision")
    eq(row.meta.reason, "threat detected", "guard ledger reason frozen")
    eq(default_record().chain_terminated, true, "guard marks chain_terminated")
end

local function test_cache_hit()
    reset()
    S.cache_result = { hit = true, provider = "openai",
                       response = { id = "resp-1" } }
    local status = run_route()
    eq(status, 200, "cache hit status")
    eq(_G.ngx.header["X-Cache"], "HIT", "cache hit header")
    eq(#S.said, 1, "cache hit emits one body")
    eq(sidecar_calls(),
       { "shape_gate", "auth_check", "ip_check", "guard_input", "cache_check",
         "policy_evaluate" },
       "cache hit check order (policy evaluated on hit)")
    eq(ledger_actions(), { "request_received", "cache_check" },
       "cache hit ledger actions")
    local row = last_ledger("cache_check")
    eq(row.meta.decision, "hit", "cache ledger decision")
    eq(row.actor, "openai", "cache ledger actor is provider")
    eq(row.alias, "client", "cache ledger alias is client")
    eq(default_record().chain_terminated, true, "cache hit terminates chain")
end

local function test_cache_empty_provider_demoted()
    reset()
    S.cache_result = { hit = true, provider = "", response = { id = "x" } }
    eq(run_route(), nil, "empty-provider hit falls through to proxy")
    eq(sidecar_calls(),
       { "shape_gate", "auth_check", "ip_check", "guard_input", "cache_check",
         "route_decide", "budget_check", "policy_evaluate" },
       "demoted hit continues to full pipeline")
end

local function test_cache_policy_denied_demoted()
    reset()
    S.cache_result = { hit = true, provider = "openai", response = { id = "x" } }
    S.policy_blocked = true
    eq(run_route(), 403, "policy-denied hit is demoted then blocked at STEP 5")
    local policy_rows = {}
    for _, r in ipairs(ledger_rows) do
        if r.meta.action == "policy_evaluate" then policy_rows[#policy_rows + 1] = r end
    end
    eq(#policy_rows, 1, "only the STEP 5 denial writes a policy row")
end

local function test_route_and_model_failures()
    reset()
    S.route_fail = true
    eq(run_route(), 503, "sidecar routing failure status")
    eq(ledger_actions(), { "request_received", "route_decide" }, "route fail ledger")
    eq(last_ledger("route_decide").meta.reason, "sidecar_unreachable",
       "route fail ledger reason")
    reset()
    S.model_unknown = true
    S.routing = { model_id = "nope", requested_model = "nope",
                  effective_model = "nope", strategy = "quality", score = 0.5 }
    eq(run_route(), 400, "unknown model status")
    eq(last_ledger("route_decide").meta.reason, "unknown_model",
       "unknown model ledger reason")
end

local function test_budget_and_policy_blocks()
    reset()
    S.budget_blocked = true
    eq(run_route(), 429, "budget block status")
    local row = last_ledger("budget_check")
    eq(row.meta.decision, "blocked", "budget ledger decision")
    eq(row.payload.budget_key, "bk1", "budget ledger payload key")
    eq(default_record().chain_terminated, true, "budget block terminates chain")
    reset()
    S.policy_blocked = true
    eq(run_route(), 403, "policy block status")
    row = last_ledger("policy_evaluate")
    eq(row.meta.decision, "blocked", "policy ledger decision")
    eq(row.payload.provider, "openai", "policy ledger payload provider")
    eq(row.payload.level, "RED", "policy ledger payload level")
end

-- Live STEP 5 allows a nil policy result. Cache hits fail-closed on nil
-- (demote) and then re-run STEP 5, which still allows.
local function test_nil_policy_live_allows_cache_demotes()
    reset()
    S.policy_nil = true
    eq(run_route(), nil, "nil live STEP 5 policy allows the request")
    check(last_ledger("policy_evaluate") == nil,
          "nil live policy does not write a blocked policy row")
    eq(_G.ngx.header["X-Cache"], nil, "nil live policy is not a cache hit")

    reset()
    S.cache_result = { hit = true, provider = "openai", response = { id = "x" } }
    S.policy_nil = true
    eq(run_route(), nil, "nil cache policy demotes the hit then live STEP 5 allows")
    eq(_G.ngx.header["X-Cache"], nil, "nil cache policy does not serve the hit")
    eq(sidecar_calls(),
       { "shape_gate", "auth_check", "ip_check", "guard_input", "cache_check",
         "policy_evaluate", "route_decide", "budget_check", "policy_evaluate" },
       "demoted nil-policy hit re-runs routing then live STEP 5")
end

local function test_proxy_success_order()
    reset()
    eq(run_route(), nil, "success proxies through without exit")
    eq(sidecar_calls(),
       { "shape_gate", "auth_check", "ip_check", "guard_input", "cache_check",
         "route_decide", "budget_check", "policy_evaluate" },
       "success path check order")
    eq(scheduled, { "request_received" }, "success schedules only open-end row")
    local record = default_record()
    eq(record.provider, "openai", "record provider")
    eq(record.resolved_model, "gpt-test", "record resolved_model")
    eq(record.budget_key, "bk1", "auth-resolved budget key wins over header")
    eq(record.effective_model, "gpt-test", "record effective_model")
    eq(S.var.target_url, "https://api.openai.local/v1/chat/completions", "target_url")
    eq(S.var.target_host, "api.openai.local", "target_host")
    eq(S.var.auth_value, "Bearer sk-openai", "openai auth_value")
    check(hdr_ops("clear", "Authorization") ~= nil, "client Authorization stripped")
    eq(S.headers.authorization, nil, "client Authorization removed")
    check(hdr_ops("clear", "X-API-Key") ~= nil, "client X-API-Key stripped")
    eq(S.headers["x-openai-extra"], "1", "provider extra_headers applied")
    eq(_G.ngx.header["X-Routed-Model"], "gpt-test", "X-Routed-Model header")
    eq(_G.ngx.header["X-Routed-Provider"], "openai", "X-Routed-Provider header")
    check(S.set_body ~= nil, "translated body set for upstream")

    -- Budget key defaults to "default" only when neither header nor auth
    -- supplied one; the default is stamped late, at dispatch.
    reset()
    S.auth_result = { allowed = true, key_id = "key-1",
                      rate_limit_limit = 10, rate_limit_remaining = 9,
                      rate_limit_reset = 5 }
    eq(run_route(), nil, "no-budget-key success proxies through")
    eq(default_record().budget_key, "default", "empty budget key defaults late")
end

local function test_anthropic_upstream_auth()
    reset()
    S.decoded = { model = "claude-test",
                  messages = { { role = "user", content = "hi" } } }
    S.routing = { model_id = "claude-test", requested_model = "claude-test",
                  effective_model = "claude-test", strategy = "quality", score = 0.5 }
    eq(run_route(), nil, "anthropic path proxies")
    eq(S.var.auth_value, "", "anthropic clears bearer auth_value")
    eq(S.headers["x-api-key"], "sk-ant", "anthropic x-api-key set to provider key")
    eq(S.headers["anthropic-version"], "2023-06-01",
       "anthropic provider extra header applied")
end

local function test_cli_provider_proxy_token()
    reset()
    S.decoded = { model = "cli-test",
                  messages = { { role = "user", content = "hi" } } }
    S.routing = { model_id = "cli-test", requested_model = "cli-test",
                  effective_model = "cli-test", strategy = "quality", score = 0.5 }
    eq(run_route(), nil, "cli path proxies")
    eq(S.headers["X-Proxy-Auth"], "Bearer cliproxy-tok",
       "CLI proxy shared secret injected")
end

local function test_cli_streaming_unsupported()
    reset()
    S.decoded = { model = "cli-test", stream = true,
                  messages = { { role = "user", content = "hi" } } }
    S.routing = { model_id = "cli-test", requested_model = "cli-test",
                  effective_model = "cli-test", strategy = "quality", score = 0.5 }
    eq(run_route(), 501, "CLI streaming status")
end

local function test_streaming_globally_disabled()
    env_map.GW_ENABLE_STREAMING = "0"
    reset()
    S.decoded = { model = "gpt-test", stream = true,
                  messages = { { role = "user", content = "hi" } } }
    eq(run_route(), 501, "global streaming gate status")
    reset()
    S.decoded = { model = "council-test", stream = true,
                  messages = { { role = "user", content = "hi" } } }
    S.routing = { model_id = "council-test", requested_model = "council-test",
                  effective_model = "council-test", strategy = "quality", score = 0.5 }
    eq(run_route(), 400, "council alias bypasses global gate, 400 at own gate")
    env_map.GW_ENABLE_STREAMING = "1"
end

local function test_transport_identity_denied()
    reset()
    S.headers["x-council-transport-id"] = "grok_cli"
    eq(run_route(), 403, "untrusted transport identity status")
    eq(last_ledger("route_decide").meta.reason, "council_transport_identity_denied",
       "transport identity ledger reason")
    eq(sidecar_calls(), { "shape_gate", "auth_check", "ip_check" },
       "transport denial precedes guard/cache/route")
end

local function test_council_restore_and_reentry()
    reset()
    S.decoded = { model = "council-test",
                  messages = { { role = "user", content = "hi" } } }
    S.headers["x-council-depth"] = "1"
    S.headers["x-council-session-id"] = "sess-9"
    S.headers["x-council-transport-id"] = "grok_cli"
    S.auth_result = { allowed = true, budget_key = "bk1", key_id = "key-1",
                      service_role = "council",
                      rate_limit_limit = 10, rate_limit_remaining = 9,
                      rate_limit_reset = 5 }
    S.routing = { model_id = "council-test", requested_model = "council-test",
                  effective_model = "council-test", strategy = "quality", score = 0.5 }
    eq(run_route(), 409, "restored depth 1 reentry status")
    check(hdr_ops("set", "X-Council-Session-Id") ~= nil,
          "trusted council gets session header restored")
    local record = default_record()
    eq(record.parent_council_request_id, "", "no parent request id in scenario")
    eq(record.requested_transport, "grok_cli", "requested_transport stashed")
    local calls = sidecar_calls()
    local saw_route_decide = false
    for _, c in ipairs(calls) do
        if c == "route_decide" then saw_route_decide = true end
    end
    check(not saw_route_decide,
          "exact-transport dispatch resolves by alias without route_decide")
    eq(calls, { "shape_gate", "auth_check", "ip_check", "guard_input",
                "budget_check", "policy_evaluate" },
       "exact-transport council check order (cache skipped for council alias)")
end

local function test_council_gate()
    reset()
    S.decoded = { model = "council-test",
                  messages = { { role = "user", content = "hi" } } }
    S.routing = { model_id = "council-test", requested_model = "council-test",
                  effective_model = "council-test", strategy = "quality", score = 0.5 }
    eq(run_route(), 400, "council missing idempotency key status")
    local saw_cache = false
    for _, c in ipairs(sidecar_calls()) do
        if c == "cache_check" then saw_cache = true end
    end
    check(not saw_cache, "council alias skips L1 cache (idempotency owned by council branch)")

    reset()
    S.decoded = { model = "council-test",
                  messages = { { role = "user", content = "hi" } } }
    S.routing = { model_id = "council-test", requested_model = "council-test",
                  effective_model = "council-test", strategy = "quality", score = 0.5 }
    S.headers["idempotency-key"] = "idem-1"
    S.peek = { pending = true }
    eq(run_route(), 409, "council pending idempotency status")
    for _, e in ipairs(events) do
        check(e.name ~= "lock", "pending peek must not acquire a lock")
    end

    reset()
    S.decoded = { model = "council-test",
                  messages = { { role = "user", content = "hi" } } }
    S.routing = { model_id = "council-test", requested_model = "council-test",
                  effective_model = "council-test", strategy = "quality", score = 0.5 }
    S.headers["idempotency-key"] = "idem-1"
    S.peek = { conflict = true }
    eq(run_route(), 409, "council idempotency conflict status")

    reset()
    S.decoded = { model = "council-test",
                  messages = { { role = "user", content = "hi" } } }
    S.routing = { model_id = "council-test", requested_model = "council-test",
                  effective_model = "council-test", strategy = "quality", score = 0.5 }
    S.headers["idempotency-key"] = "idem-1"
    S.peek = { hit = true, cached_response = { headers = {} },
               original_request_id = "orig-1", response_body_sha256 = "abc" }
    local status, replay = run_route()
    eq(status, 200, "council replay exits 200 via cost.account_replay")
    eq(replay, true, "replay flag propagated")
    local record = default_record()
    eq(record.council_replay, true, "replay marker on record")
    eq(record.council_replay_orig_request_id, "orig-1", "replay origin request id")

    reset()
    S.decoded = { model = "council-test",
                  messages = { { role = "user", content = "hi" } } }
    S.routing = { model_id = "council-test", requested_model = "council-test",
                  effective_model = "council-test", strategy = "quality", score = 0.5 }
    S.headers["idempotency-key"] = "idem-1"
    eq(run_route(), nil, "council claim success proxies through")
    local record = default_record()
    eq(record.council_locked, true, "council lock held on claim success")
    eq(record.council_grant_id, "g1", "council grant id captured")
    check(S.abort_fn ~= nil, "client-abort unlock registered")
    eq(S.headers["X-Council-Request-ID"], "req-1", "parent header stamped")
    eq(S.headers["X-Council-Depth"], "0", "depth header stamped")
    eq(sidecar_calls(),
       { "shape_gate", "auth_check", "ip_check", "guard_input", "route_decide",
         "budget_check", "policy_evaluate", "peek", "lock", "claim" },
       "council claim sidecar order")

    reset()
    S.decoded = { model = "council-test",
                  messages = { { role = "user", content = "hi" } } }
    S.routing = { model_id = "council-test", requested_model = "council-test",
                  effective_model = "council-test", strategy = "quality", score = 0.5 }
    S.headers["idempotency-key"] = "idem-1"
    S.claim_conflict = true
    eq(run_route(), 409, "claim conflict status")
    eq(sidecar_calls()[#sidecar_calls()], "unlock",
       "claim conflict unlocks the concurrency slot")
end

local function test_batch_family()
    -- batch disabled requires a router loaded with GW_ENABLE_BATCH unset
    env_map.GW_ENABLE_BATCH = "0"
    reset()
    S.var.uri = "/v1/batches"
    eq(run_route(), 501, "batch disabled status")
    env_map.GW_ENABLE_BATCH = "1"

    reset()
    S.var.uri = "/v1/batches"
    eq(run_route(), 400, "batch missing provider status")

    reset()
    S.var.uri = "/v1/batches"
    S.headers["x-provider"] = "mistral"
    eq(run_route(), 400, "batch unsupported provider status")

    reset()
    S.var.uri = "/v1/batches"
    S.headers["x-provider"] = "openai"
    eq(run_route(), nil, "batch create proxies through")
    eq(sidecar_calls(), { "auth_check", "ip_check", "budget_check" },
       "batch check order (no guard/cache/route)")
    eq(scheduled, { "batch_received" }, "batch open-end ledger row")
    local row = last_ledger("batch_create")
    check(row ~= nil, "batch create action recorded")
    eq(row.meta.batch_mode, true, "batch ledger metadata flag")
    eq(S.var.target_url, "https://api.openai.local/v1/batches", "batch target_url")
    eq(S.headers["x-openai-extra"], "1", "batch provider extra headers applied")
    eq(_G.ngx.header["X-Batch-Op"], "create", "batch op header")

    reset()
    S.var.uri = "/v1/batches/job-9"
    S.var.request_method = "GET"
    S.method = "GET"
    S.headers["x-provider"] = "anthropic"
    S.body = nil
    eq(run_route(), nil, "batch status GET proxies through")
    eq(S.read_body_calls, 0, "GET batch does not read a body")
    local row2 = last_ledger("batch_status")
    check(row2 ~= nil, "batch status action recorded")
    eq(S.var.target_url, "https://api.anthropic.local/v1/messages/batches/job-9",
       "anthropic batch URI rewrite")
    eq(S.headers["x-api-key"], "sk-ant", "anthropic batch credential applied")
    eq(S.var.auth_value, "", "anthropic batch clears bearer auth_value")

    reset()
    S.var.uri = "/v1/batches"
    S.headers["x-provider"] = "openai"
    S.budget_blocked = true
    eq(run_route(), 429, "batch budget block status")
    local row3 = last_ledger("budget_check")
    eq(row3.meta.batch_mode, true, "batch budget row carries batch_mode")
end

local tests = {
    { name = "step0_rejections",        fn = test_step0_rejections },
    { name = "body_file_fallback",      fn = test_body_file_fallback },
    { name = "auth_rejections",         fn = test_auth_rejections },
    { name = "content_depth",           fn = test_content_depth },
    { name = "guard_block",             fn = test_guard_block },
    { name = "cache_hit",               fn = test_cache_hit },
    { name = "cache_empty_demoted",     fn = test_cache_empty_provider_demoted },
    { name = "cache_policy_demoted",    fn = test_cache_policy_denied_demoted },
    { name = "route_model_failures",    fn = test_route_and_model_failures },
    { name = "budget_policy_blocks",    fn = test_budget_and_policy_blocks },
    { name = "nil_policy_live_vs_cache", fn = test_nil_policy_live_allows_cache_demotes },
    { name = "proxy_success_order",     fn = test_proxy_success_order },
    { name = "anthropic_upstream_auth", fn = test_anthropic_upstream_auth },
    { name = "cli_proxy_token",         fn = test_cli_provider_proxy_token },
    { name = "cli_streaming",           fn = test_cli_streaming_unsupported },
    { name = "streaming_global_gate",   fn = test_streaming_globally_disabled },
    { name = "transport_identity",      fn = test_transport_identity_denied },
    { name = "council_reentry",         fn = test_council_restore_and_reentry },
    { name = "council_gate",            fn = test_council_gate },
    { name = "batch_family",            fn = test_batch_family },
}

for _, t in ipairs(tests) do
    local ok, err = pcall(t.fn)
    if not ok then
        failures = failures + 1
        print("FAIL: " .. t.name .. " errored: " .. tostring(err))
    end
end

if failures > 0 then
    print("router_route_family_test: " .. failures .. " failure(s)")
    os.exit(1)
end
print("OK: router route-family characterization holds")
