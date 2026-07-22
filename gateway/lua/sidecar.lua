-- ==========================================================================
-- sidecar.lua — Lua client for the Rust gateway sidecar.
--
-- Provides cosocket-based callouts from OpenResty to the Rust sidecar
-- at configurable address (default: unix:/tmp/gateway-sidecar.sock:).
--
-- Each method returns (result_table, err_string).
-- On success: result_table is the decoded JSON response, err is nil.
-- On failure: result_table is nil, err is a string.
--
-- Connection pooling is automatic via ngx.socket.tcp keepalive.
-- ==========================================================================

local cjson = require "cjson.safe"
local http  = require "resty.http"  -- lua-resty-http

local _M = {}

-- ---------------------------------------------------------------------------
-- Configuration — must call _M.init() from init_by_lua_block
-- ---------------------------------------------------------------------------

local SIDECAR_ADDR = "unix:/tmp/gateway-sidecar.sock:"
local SIDECAR_TIMEOUT_MS = 50

function _M.init()
    SIDECAR_ADDR = os.getenv("SIDECAR_ADDR") or "unix:/tmp/gateway-sidecar.sock:"
    SIDECAR_TIMEOUT_MS = tonumber(os.getenv("SIDECAR_TIMEOUT_MS")) or 50
    ngx.log(ngx.INFO, "sidecar: configured at ", SIDECAR_ADDR,
            " timeout=", SIDECAR_TIMEOUT_MS, "ms")
end

-- ---------------------------------------------------------------------------
-- Internal HTTP helper (cosocket + connection pool)
-- ---------------------------------------------------------------------------

-- Returns (host_field, connect_target) where:
--   host_field      — value used in the HTTP Host header (and pool name)
--   connect_target  — argument to httpc:connect (UDS string or {host, port})
local function parse_sidecar_addr(addr)
    -- UDS form: "unix:/path/to.sock:" — strip optional trailing colon.
    -- The trailing colon is the resty.http convention for "no port" but
    -- httpc:connect does not expect it.
    local unix_path = addr:match("^unix:(.+):$") or addr:match("^unix:(.+)$")
    if unix_path then
        return "sidecar.local", "unix:" .. unix_path
    end
    -- TCP form: "host:port"
    local host, port = addr:match("^([^:]+):(%d+)$")
    if host and port then
        return host, { host = host, port = tonumber(port) }
    end
    return addr, addr
end

local function sidecar_post(path, body_table, extra_headers)
    local httpc = http.new()
    httpc:set_timeout(SIDECAR_TIMEOUT_MS)

    local body_json = cjson.encode(body_table)
    local host_field, connect_target = parse_sidecar_addr(SIDECAR_ADDR)

    -- Pool name keys keep-alive sockets per-target. Including the address
    -- string keeps UDS and TCP pools separated cleanly.
    local pool_name = "sidecar:" .. SIDECAR_ADDR

    local ok, conn_err
    if type(connect_target) == "table" then
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target.host,
            port   = connect_target.port,
            pool   = pool_name,
        }
    else
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target,  -- "unix:/path"
            pool   = pool_name,
        }
    end
    if not ok then
        return nil, "sidecar unreachable: " .. (conn_err or "connect failed")
    end

    local headers = {
        ["Host"]           = host_field,
        ["Content-Type"]   = "application/json",
        ["Content-Length"] = tostring(#body_json),
    }
    if extra_headers then
        for k, v in pairs(extra_headers) do
            headers[k] = v
        end
    end

    local res, req_err = httpc:request{
        method  = "POST",
        path    = path,
        body    = body_json,
        headers = headers,
    }
    if not res then
        httpc:close()
        return nil, "sidecar request failed: " .. (req_err or "unknown")
    end

    local body, body_err = res:read_body()
    if not body then
        httpc:close()
        return nil, "sidecar body read failed: " .. (body_err or "unknown")
    end

    -- Return the connection to the keep-alive pool (60s, 10 per worker).
    httpc:set_keepalive(60000, 10)

    local decoded, decode_err = cjson.decode(body)
    if not decoded then
        return nil, "sidecar response parse error: " .. (decode_err or "empty body")
    end

    return decoded, nil, res.status
end

-- ---------------------------------------------------------------------------
-- Guard — input decontamination
-- ---------------------------------------------------------------------------

--- Check input content for injection, encoding attacks, and prompt manipulation.
-- @param content  string  The raw user input to scan
-- @param source   string  Identifier for logging (e.g. "api", "web")
-- @return table|nil  Guard result with verdict, threats, blocked flag
-- @return string|nil Error message if sidecar unreachable
function _M.guard_input(content, source)
    local result, err = sidecar_post("/guard/input", {
        content = content,
        source  = source or "gateway",
    })
    if err then
        -- Fail closed: if sidecar is down, block the request
        ngx.log(ngx.ERR, "sidecar: guard failed (fail-closed): ", err)
        return { blocked = true, verdict = "error", blocked_reason = "sidecar unreachable" }, nil
    end
    return result, nil
end

-- ---------------------------------------------------------------------------
-- Auth — validate API keys
-- ---------------------------------------------------------------------------

--- Check authentication virtual API key.
-- @param raw_key  string  The provided API key
-- @param ip       string  Client IP address
-- @return table|nil  Auth result (allowed, budget_key, tier, limits...)
-- @return string|nil Error message
function _M.auth_check(raw_key, ip)
    local result, err, status = sidecar_post("/auth/check", {
        raw_key = raw_key,
        ip      = ip,
    })
    if err then
        -- Fail closed on auth check
        return { allowed = false, reason = "sidecar unreachable", rate_limit_limit = 0, rate_limit_remaining = 0, rate_limit_reset = 0 }, nil
    end
    return result, nil
end

--- Check IP address against the sidecar's CIDR allow/deny policy.
-- @param ip  string  Client IP address (typically ngx.var.remote_addr)
-- @return table|nil  IP check result { allowed, reason, is_internal }
-- @return string|nil Error message (always nil — fail-closed result is
--                    encoded in the returned table so call sites can stay
--                    simple)
function _M.ip_check(ip)
    local result, err = sidecar_post("/auth/ip-check", { ip = ip })
    if err then
        -- Fail closed on IP check: a sidecar outage MUST NOT silently open
        -- the gate. Caller should treat allowed=false the same as a real
        -- denial.
        ngx.log(ngx.ERR, "sidecar: ip_check failed (fail-closed): ", err)
        return { allowed = false, reason = "sidecar unreachable", is_internal = false }, nil
    end
    return result, nil
end

-- ---------------------------------------------------------------------------
-- Guard — outbound sovereignty
-- ---------------------------------------------------------------------------

--- Check outbound response for values alignment.
-- @param content  string  The LLM response content
-- @param action_type string Type of action (e.g. "llm_call")
-- @return table|nil  Sovereignty result
-- @return string|nil Error message
function _M.guard_sovereignty(content, action_type)
    return sidecar_post("/guard/sovereignty", {
        action_desc = content,
        action_type = action_type or "llm_call",
        energy      = 1.0,
    })
end

-- ---------------------------------------------------------------------------
-- Ledger — cryptographic audit trail
-- ---------------------------------------------------------------------------

--- Record an event in the tamper-evident ledger.
-- @param source     string       Source of the event (e.g. "gateway")
-- @param target     string       Target of the event (e.g. "model-id")
-- @param payload    table        The payload being processed
-- @param metadata   table        Additional context
-- @param caller_key string|nil   Per-key audit identity (key_id from /auth/check).
--                                Empty string or nil → sidecar stores NULL
--                                (legacy / no-auth). Schema v2+.
-- @return table|nil  Ledger record result
-- @return string|nil Error message
function _M.ledger_record(source, target, payload, metadata, caller_key)
    local body = {
        source   = source,
        target   = target,
        payload  = payload,
        metadata = metadata or {},
    }
    if caller_key and caller_key ~= "" then
        body.caller_key = caller_key
    end
    return sidecar_post("/ledger/record", body)
end

-- ---------------------------------------------------------------------------
-- Route — smart model selection
-- ---------------------------------------------------------------------------

--- Ask the sidecar to score and select the best model.
-- @param model         string|nil  Specific model requested (nil = auto-select)
-- @param body          table       The full request body (messages, tools, etc.)
-- @param strategy      string|nil  "quality" | "balanced" | "economy" | "speed"
-- @param sensitivity   string|nil  "GREEN" | "YELLOW" | "RED" (forwarded as
--                                  X-Sensitivity-Level header — the sidecar
--                                  has no opinion on payload sensitivity).
-- @param sovereign_mode boolean|nil If true, forces all routing to local providers
-- @return table|nil  Routing decision (model_id, provider, score, sensitivity, ...)
-- @return string|nil Error message
function _M.route_decide(model, body, strategy, sensitivity, sovereign_mode)
    local req = {
        body = body,
    }
    if model and model ~= "" then
        req.model = model
    end
    if strategy and strategy ~= "" then
        req.strategy = strategy
    end

    local headers = {}
    if sensitivity and sensitivity ~= "" then
        headers["X-Sensitivity-Level"] = sensitivity
    end
    if sovereign_mode then
        headers["X-Sovereign-Mode"] = "true"
    end

    local result, err, status = sidecar_post("/route/decide", req,
        next(headers) and headers or nil)
    if err then
        return nil, err
    end
    if status and status >= 400 then
        return nil, "routing failed: " .. (result.error or "unknown")
    end
    return result, nil
end

--- Report outcome back to sidecar for per-family health tracking.
-- @param model_id   string   Resolved model ID (not alias)
-- @param success    boolean  Whether the upstream call succeeded
-- @param latency_ms number   Round-trip latency in ms
-- @param error_msg  string|nil  Error message if failed
function _M.route_outcome(model_id, success, latency_ms, error_msg)
    -- Fire and forget — don't block the response
    local result, err = sidecar_post("/route/outcome", {
        model_id   = model_id,
        success    = success,
        latency_ms = latency_ms,
        error      = error_msg,
    })
    if err then
        ngx.log(ngx.WARN, "sidecar: outcome report failed: ", err)
    end
    return result, err
end

-- ---------------------------------------------------------------------------
-- Budget — pre-flight cost check + post-flight spend recording
-- ---------------------------------------------------------------------------

--- Pre-flight: check if estimated cost fits within budget.
-- @param budget_key     string  Per-user/per-org budget key
-- @param estimated_cost number  Estimated cost in USD
-- @return table|nil  Budget check result (allowed, reason, status)
-- @return string|nil Error
function _M.budget_check(budget_key, estimated_cost)
    local result, err, status = sidecar_post("/budget/check", {
        budget_key     = budget_key,
        estimated_cost = estimated_cost or 0,
    })
    if err then
        -- Fail closed on budget check
        return { allowed = false, reason = "sidecar unreachable" }, nil
    end
    -- Propagate HTTP 429 as budget exceeded
    if status == 429 then
        return result, nil  -- result.allowed will be false
    end
    return result, nil
end

--- Post-flight: record actual cost against budget.
-- @param budget_key  string  Per-user/per-org budget key
-- @param actual_cost number  Actual cost in USD
function _M.budget_record(budget_key, actual_cost)
    local result, err = sidecar_post("/budget/record", {
        budget_key  = budget_key,
        actual_cost = actual_cost,
    })
    if err then
        ngx.log(ngx.WARN, "sidecar: budget record failed: ", err)
    end
    return result, err
end

-- ---------------------------------------------------------------------------
-- Policy — sensitivity-based routing firewall
-- ---------------------------------------------------------------------------

-- Map the contract vocabulary (GREEN/YELLOW/RED — caller's trust verdict
-- per COUNCIL_GATEWAY_CONTRACT.md) onto the policy module's internal
-- vocabulary (PUBLIC/INTERNAL/SOVEREIGN — provider-allowance tiers).
-- This is the only place that knows about both vocabularies.
local CONTRACT_TO_POLICY_LEVEL = {
    GREEN  = "PUBLIC",
    YELLOW = "INTERNAL",
    RED    = "SOVEREIGN",
}

--- Evaluate content sensitivity and check provider allowance.
-- @param provider  string  Target provider name
-- @param content   string  Request content (kept for legacy detection path)
-- @param level     string|nil  Contract-level verdict ("GREEN"|"YELLOW"|"RED");
--                              translated to the policy enum here.
-- @return table|nil  Policy result (allowed, level, detected_signals, dry_run)
-- @return string|nil Error
function _M.policy_evaluate(provider, content, level)
    local req = {
        provider = provider,
        content  = content,
    }
    if level and level ~= "" then
        req.sensitivity_level = CONTRACT_TO_POLICY_LEVEL[level:upper()]
            or level:upper()
    end
    return sidecar_post("/policy/evaluate", req)
end

-- ---------------------------------------------------------------------------
-- Cache — check and store prompt/response pairs
-- ---------------------------------------------------------------------------

--- Check if a response exists in cache for the given alias + raw body bytes.
--
-- IMPORTANT: `raw_body` MUST be the literal request bytes from
-- ngx.req.get_body_data() (or the body file), NOT a re-encoded JSON of the
-- decoded table. The Rust side hashes raw bytes — re-encoding through cjson
-- would produce a different canonical form than the original request and
-- the cache would never hit. See sidecar-rs/src/cache.rs::generate_cache_key.
--
-- The result includes the `provider` that produced the cached response.
-- The caller MUST run the response back through translator.translate_response
-- before emitting to the client — the cache stores native upstream shape.
--
-- @param alias                       string  Client-supplied model alias
-- @param raw_body                    string  Literal request body bytes
-- @param expected_translator_version number  translator.TRANSLATOR_VERSION
-- @return table|nil  { hit, response (native shape), provider, latency_ms }
-- @return string|nil Error
function _M.cache_check(alias, raw_body, expected_translator_version)
    return sidecar_post("/cache/check", {
        alias                       = alias,
        raw_body                    = raw_body,
        expected_translator_version = expected_translator_version,
    })
end

--- Store a NATIVE upstream response in cache.
--
-- IMPORTANT: pass the pre-translation upstream body (e.g.,
-- ngx.ctx.gw_response_buf_native), NOT the normalized one. Storing a
-- normalized body would defeat the cache-shape invariant: cache hits
-- re-run translate_response so wire shape stays consistent with fresh
-- requests, and that requires the native shape on disk.
--
-- @param alias               string  Client-supplied model alias
-- @param raw_body            string  Literal request body bytes
-- @param native_response     table   Decoded native upstream response
-- @param provider            string  Provider name ("anthropic", "xai", ...)
-- @param translator_version  number  translator.TRANSLATOR_VERSION
-- @param ttl_secs            number|nil  TTL in seconds (default: 24h)
function _M.cache_store(alias, raw_body, native_response, provider, translator_version, ttl_secs)
    return sidecar_post("/cache/store", {
        alias              = alias,
        raw_body           = raw_body,
        response           = native_response,
        provider           = provider,
        translator_version = translator_version,
        ttl_secs           = ttl_secs,
    })
end

-- ---------------------------------------------------------------------------
-- Council endpoint (Phase 0.5, spec §5.4–§5.8)
--
-- The Lua router calls these in a peek → lock → claim sequence for council-*
-- model requests. The post-flight cleanup (unlock + store/fail) runs from a
-- timer scheduled in cost.lua's log phase — those call sites must use the
-- module-load function bindings defined at the top of cost.lua, NOT
-- `sidecar.council_*(...)` directly inside the timer body. See the
-- Keep sidecar calls out of timer closures so connection failures stay visible.
-- ---------------------------------------------------------------------------

--- Read-only idempotency lookup. Returns the cached response on hit, marks
-- conflict when the same key is being reused for a different body, or
-- pending when another request owns this key. Used to short-circuit replay
-- without ever inserting Pending state.
function _M.council_idempotency_peek(caller_key, idempotency_key, body_sha256)
    return sidecar_post("/council/idempotency/peek", {
        caller_key      = caller_key,
        idempotency_key = idempotency_key,
        body_sha256     = body_sha256,
    })
end

--- Insert an owner-aware Pending reservation AFTER the concurrency lock has
-- been acquired. Returns `conflict=true` when a foreign owner won the race
-- between peek and claim; the caller MUST then release its lock and 409.
function _M.council_idempotency_claim(caller_key, idempotency_key, body_sha256, owner_request_id)
    return sidecar_post("/council/idempotency/claim", {
        caller_key       = caller_key,
        idempotency_key  = idempotency_key,
        body_sha256      = body_sha256,
        owner_request_id = owner_request_id,
    })
end

--- Transition Pending → Stored on success. The cached `response` table is
-- expected to carry `{ status, body_json, headers }` so the replay path
-- (§6.3) can reconstruct the original wire response.
--
-- P0-3: pass `owner_request_id` and `response_body_sha256` so subsequent
-- replays can surface `original_request_id` and `response_body_sha256`
-- on the `council_replay` ledger row — restoring the non-repudiation
-- pair `(raw_body_sha256, response_body_sha256)` for replays.
function _M.council_idempotency_store(
    caller_key, idempotency_key, body_sha256, response, ttl_seconds,
    owner_request_id, response_body_sha256)
    return sidecar_post("/council/idempotency/store", {
        caller_key            = caller_key,
        idempotency_key       = idempotency_key,
        body_sha256           = body_sha256,
        response              = response,
        ttl_seconds           = ttl_seconds or 86400,
        owner_request_id      = owner_request_id or "",
        response_body_sha256  = response_body_sha256 or "",
    })
end

--- Transition Pending → Failed (terminal). The Failed state evicts after
-- FAILED_TTL=60s so a retry-after-504 can proceed without a 120s self-DoS
-- window (spec P1 #9).
function _M.council_idempotency_fail(caller_key, idempotency_key)
    return sidecar_post("/council/idempotency/fail", {
        caller_key      = caller_key,
        idempotency_key = idempotency_key,
    })
end

--- Acquire one slot of the per-caller concurrency budget (cap=2).
-- Result: `{ granted = boolean, active = u32, grant_id = string }`. The
-- caller MUST thread the returned grant_id back to `council_unlock` so the
-- sidecar removes the exact slot rather than any-one — without that, a
-- sweeper reclaim of a stale slot races a late unlock from a still-live
-- handler and pops the wrong entry (FIX-1).
function _M.council_lock(caller_key)
    return sidecar_post("/council/lock", {
        caller_key = caller_key,
    })
end

--- Release a previously granted lock.
-- @param caller_key string  per-caller concurrency namespace
-- @param grant_id   string  the value returned in council_lock's response;
--                           empty string is tolerated (legacy callers) but
--                           logs a WARN on the sidecar side because of the
--                           slot-stealing race that motivated FIX-1.
function _M.council_unlock(caller_key, grant_id)
    return sidecar_post("/council/unlock", {
        caller_key = caller_key,
        grant_id   = grant_id or "",
    })
end

-- ---------------------------------------------------------------------------
-- Vertex AI
-- ---------------------------------------------------------------------------

--- Fetch a fresh Vertex ADC token from the sidecar
-- @return table|nil response data
-- @return string|nil error message
function _M.vertex_token()
    local httpc = http.new()
    httpc:set_timeout(SIDECAR_TIMEOUT_MS)

    local host_field, connect_target = parse_sidecar_addr(SIDECAR_ADDR)
    local pool_name = "sidecar:" .. SIDECAR_ADDR

    local ok, conn_err
    if type(connect_target) == "table" then
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target.host,
            port   = connect_target.port,
            pool   = pool_name,
        }
    else
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target,
            pool   = pool_name,
        }
    end
    if not ok then return nil, "sidecar unreachable: " .. (conn_err or "connect failed") end

    local res, req_err = httpc:request{
        method  = "GET",
        path    = "/vertex/token",
        headers = { ["Host"] = host_field },
    }
    if not res then
        httpc:close()
        return nil, "sidecar request failed: " .. (req_err or "unknown")
    end

    local body, body_err = res:read_body()
    if not body then
        httpc:close()
        return nil, "sidecar body read failed: " .. (body_err or "unknown")
    end

    httpc:set_keepalive(60000, 10)

    if res.status ~= 200 then
        return nil, "sidecar returned " .. res.status .. ": " .. body
    end

    local decoded, decode_err = cjson.decode(body)
    if not decoded then
        return nil, "sidecar response parse error: " .. (decode_err or "empty body")
    end

    return decoded, nil
end

-- ---------------------------------------------------------------------------
-- Council stats (P1-C)
-- ---------------------------------------------------------------------------

--- Fetch a snapshot of council counters + concurrency gauges from the sidecar.
-- Polled by cost.lua at worker init (only worker 0) so prometheus can expose
-- gw_council_active_swept_total, gw_council_unlock_missing_grant_total,
-- gw_council_active_locks, gw_council_active_caller_keys without each scrape
-- making its own UDS round-trip.
-- @return table|nil  { active_swept_total, unlock_missing_grant_total,
--                     active_locks, active_caller_keys, stored_bytes }
-- @return string|nil error
function _M.council_stats()
    local httpc = http.new()
    -- Stats polling is not on the request hot path — fires every 30s from a
    -- single worker timer. The default SIDECAR_TIMEOUT_MS (200ms in compose)
    -- is calibrated for hot-path calls and is tight for the cold connection
    -- + lock acquisition pattern this endpoint hits. 1000ms is generous.
    httpc:set_timeout(1000)

    local host_field, connect_target = parse_sidecar_addr(SIDECAR_ADDR)
    -- Use a dedicated pool name to keep the polling connection isolated
    -- from the hot-path pool. Combined with the close() below, this means
    -- each poll opens + closes its own socket and we can't inherit a
    -- stale-keepalive socket the server already closed.
    local pool_name = "sidecar:stats:" .. SIDECAR_ADDR

    local ok, conn_err
    if type(connect_target) == "table" then
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target.host,
            port   = connect_target.port,
            pool   = pool_name,
        }
    else
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target,
            pool   = pool_name,
        }
    end
    if not ok then return nil, "sidecar unreachable: " .. (conn_err or "connect failed") end

    local res, req_err = httpc:request{
        method  = "GET",
        path    = "/council/stats",
        headers = { ["Host"] = host_field, ["Connection"] = "close" },
    }
    if not res then
        httpc:close()
        return nil, "sidecar request failed: " .. (req_err or "unknown")
    end

    local body, body_err = res:read_body()
    if not body then
        httpc:close()
        return nil, "sidecar body read failed: " .. (body_err or "unknown")
    end

    -- Close rather than keepalive: this is a low-frequency poller, and a
    -- 30s gap between calls is long enough that a pooled socket can be
    -- closed by the server side, surfacing as a read-timeout on next use.
    httpc:close()

    if res.status ~= 200 then
        return nil, "sidecar returned " .. res.status .. ": " .. body
    end

    local decoded, decode_err = cjson.decode(body)
    if not decoded then
        return nil, "sidecar response parse error: " .. (decode_err or "empty body")
    end

    return decoded, nil
end

-- ---------------------------------------------------------------------------
-- Watch stats (T33.P0.2 — review)
-- ---------------------------------------------------------------------------

--- Fetch a snapshot of watch-plane counters from the sidecar. Mirrors
-- `council_stats` shape so cost.lua can poll on the same cadence and emit
-- gw_watch_* counters on /metrics without inventing a second pattern.
-- @return table|nil  { audit_infra_errors_total, persist_failures_total }
-- @return string|nil error
function _M.watch_stats()
    local httpc = http.new()
    -- Same rationale as council_stats: poller cadence (30s), not hot-path.
    httpc:set_timeout(1000)

    local host_field, connect_target = parse_sidecar_addr(SIDECAR_ADDR)
    local pool_name = "sidecar:watch_stats:" .. SIDECAR_ADDR

    local ok, conn_err
    if type(connect_target) == "table" then
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target.host,
            port   = connect_target.port,
            pool   = pool_name,
        }
    else
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target,
            pool   = pool_name,
        }
    end
    if not ok then return nil, "sidecar unreachable: " .. (conn_err or "connect failed") end

    local res, req_err = httpc:request{
        method  = "GET",
        path    = "/watch/stats",
        headers = { ["Host"] = host_field, ["Connection"] = "close" },
    }
    if not res then
        httpc:close()
        return nil, "sidecar request failed: " .. (req_err or "unknown")
    end

    local body, body_err = res:read_body()
    if not body then
        httpc:close()
        return nil, "sidecar body read failed: " .. (body_err or "unknown")
    end

    httpc:close()

    if res.status ~= 200 then
        return nil, "sidecar returned " .. res.status .. ": " .. body
    end

    local decoded, decode_err = cjson.decode(body)
    if not decoded then
        return nil, "sidecar response parse error: " .. (decode_err or "empty body")
    end

    return decoded, nil
end

-- ---------------------------------------------------------------------------
-- Health
-- ---------------------------------------------------------------------------

function _M.health()
    local httpc = http.new()
    httpc:set_timeout(SIDECAR_TIMEOUT_MS)
    local uri = "http://" .. SIDECAR_ADDR .. "/health"
    local res, err = httpc:request_uri(uri, { method = "GET" })
    if not res then
        return nil, err
    end
    return cjson.decode(res.body), nil
end

-- ---------------------------------------------------------------------------
-- Admin proxy — transparently forwards /admin/* requests to the sidecar.
-- Auth is enforced at the sidecar level (admin_key in request body or
-- BOOTSTRAP_TOKEN match). The nginx layer just proxies.
-- ---------------------------------------------------------------------------

function _M.admin_proxy()
    local httpc = http.new()
    httpc:set_timeout(5000)

    local host_field, connect_target = parse_sidecar_addr(SIDECAR_ADDR)
    local pool_name = "sidecar:admin:" .. SIDECAR_ADDR

    local ok, conn_err
    if type(connect_target) == "table" then
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target.host,
            port   = connect_target.port,
            pool   = pool_name,
        }
    else
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target,
            pool   = pool_name,
        }
    end
    if not ok then
        ngx.status = 502
        ngx.header["Content-Type"] = "application/json"
        ngx.say(cjson.encode({error = "sidecar unreachable", detail = conn_err or "connect failed"}))
        return
    end

    ngx.req.read_body()
    local body = ngx.req.get_body_data()

    -- W1b: forward X-Admin-Key, but ONLY for the /ledger/* routes that actually
    -- authorize on it (auth.check tier=="admin"). The /admin/* + /auth/rotate
    -- siblings read admin_key from the JSON body and ignore this header, so
    -- scoping the forward to /ledger/* is self-documenting and stops a future
    -- sibling on this shared proxy from silently inheriting X-Admin-Key.
    -- Authorization stays stripped — this surface never carried it.
    local req_headers = ngx.req.get_headers()
    local fwd_headers = {
        ["Host"]         = host_field,
        ["Content-Type"] = "application/json",
        ["X-Request-ID"] = ngx.var.request_id,
    }
    if ngx.var.uri:match("^/ledger/") and req_headers["X-Admin-Key"] then
        fwd_headers["X-Admin-Key"] = req_headers["X-Admin-Key"]
    end

    -- W1b: preserve the query string. admin_proxy previously set path =
    -- ngx.var.uri, silently dropping ?limit/?offset for /ledger/export. The
    -- /admin/* + /auth/rotate siblings are POSTs that read only the body (no
    -- Query extractor), so appending args is harmless to them.
    local path = ngx.var.uri
    if ngx.var.args and ngx.var.args ~= "" then
        path = path .. "?" .. ngx.var.args
    end

    local res, req_err = httpc:request{
        method  = ngx.req.get_method(),
        path    = path,
        body    = body,
        headers = fwd_headers,
    }
    if not res then
        httpc:close()
        ngx.status = 502
        ngx.header["Content-Type"] = "application/json"
        ngx.say(cjson.encode({error = "sidecar request failed", detail = req_err or "unknown"}))
        return
    end

    local resp_body = res:read_body()
    httpc:set_keepalive(10000, 4)

    ngx.status = res.status
    ngx.header["Content-Type"] = res.headers["Content-Type"] or "application/json"
    ngx.say(resp_body or "")
end

-- ---------------------------------------------------------------------------
-- Watch/outbox proxy — transparently forwards /watch/outbox/* requests to
-- the sidecar REST surface over UDS. The sidecar remains the authority for
-- tenant scoping, admin bearer validation, and status contracts; nginx only
-- exposes the operator-facing HTTP route on the gateway port.
-- ---------------------------------------------------------------------------

-- Browser callers (War Room web on localhost:3000, Tauri webview) are
-- same-machine loopback only. Echo the Origin back for loopback origins so
-- the Outbox panel can read responses, and answer preflight locally. Any
-- other origin gets no CORS headers (browser blocks the read) and OPTIONS
-- falls through to the sidecar's existing 405 contract.
local function watch_outbox_cors_origin(origin)
    if not origin then return nil end
    if origin == "tauri://localhost" then return origin end
    if origin:match("^https?://localhost$") or origin:match("^https?://localhost:%d+$")
        or origin:match("^https?://127%.0%.0%.1$") or origin:match("^https?://127%.0%.0%.1:%d+$")
        or origin:match("^https?://%[::1%]$") or origin:match("^https?://%[::1%]:%d+$") then
        return origin
    end
    return nil
end

function _M.watch_outbox_proxy()
    local cors_origin = watch_outbox_cors_origin(ngx.req.get_headers()["Origin"])
    if cors_origin then
        ngx.header["Access-Control-Allow-Origin"] = cors_origin
        ngx.header["Vary"] = "Origin"
        if ngx.req.get_method() == "OPTIONS" then
            ngx.header["Access-Control-Allow-Methods"] = "GET, POST, OPTIONS"
            ngx.header["Access-Control-Allow-Headers"] = "Authorization, Content-Type, X-Tenant-Scope"
            ngx.header["Access-Control-Max-Age"] = "600"
            ngx.status = 204
            return
        end
    end

    local httpc = http.new()
    httpc:set_timeout(5000)

    local host_field, connect_target = parse_sidecar_addr(SIDECAR_ADDR)
    local pool_name = "sidecar:watch_outbox:" .. SIDECAR_ADDR

    local ok, conn_err
    if type(connect_target) == "table" then
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target.host,
            port   = connect_target.port,
            pool   = pool_name,
        }
    else
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target,
            pool   = pool_name,
        }
    end
    if not ok then
        ngx.status = 502
        ngx.header["Content-Type"] = "application/json"
        ngx.say(cjson.encode({error = "sidecar unreachable", detail = conn_err or "connect failed"}))
        return
    end

    ngx.req.read_body()
    local body = ngx.req.get_body_data()
    local req_headers = ngx.req.get_headers()
    local headers = {
        ["Host"]         = host_field,
        ["X-Request-ID"] = ngx.var.request_id,
    }

    if req_headers["Content-Type"] then
        headers["Content-Type"] = req_headers["Content-Type"]
    end
    if req_headers["Authorization"] then
        headers["Authorization"] = req_headers["Authorization"]
    end
    if req_headers["X-Tenant-Scope"] then
        headers["X-Tenant-Scope"] = req_headers["X-Tenant-Scope"]
    end
    if body then
        headers["Content-Length"] = tostring(#body)
    end

    local path = ngx.var.uri
    if ngx.var.args and ngx.var.args ~= "" then
        path = path .. "?" .. ngx.var.args
    end

    local res, req_err = httpc:request{
        method  = ngx.req.get_method(),
        path    = path,
        body    = body,
        headers = headers,
    }
    if not res then
        httpc:close()
        ngx.status = 502
        ngx.header["Content-Type"] = "application/json"
        ngx.say(cjson.encode({error = "sidecar request failed", detail = req_err or "unknown"}))
        return
    end

    local resp_body = res:read_body()
    httpc:set_keepalive(10000, 4)

    ngx.status = res.status
    ngx.header["Content-Type"] = res.headers["Content-Type"] or "application/json"
    if resp_body and #resp_body > 0 then
        ngx.print(resp_body)
    end
end

-- Gate 4 Watch UI projection. This proxy is deliberately separate from the
-- outbox proxy so its contract cannot grow into a general /watch tunnel.
-- It accepts exactly GET /watch/ui-snapshot/{one-path-segment}, forwards only
-- Authorization and request identity, and has no CORS policy because browsers
-- reach it through the Council BFF rather than talking to Gateway directly.
function _M.watch_ui_snapshot_proxy()
    if ngx.req.get_method() ~= "GET"
        or not ngx.var.uri:match("^/watch/ui%-snapshot/[^/]+$") then
        ngx.status = 405
        ngx.header["Content-Type"] = "application/json"
        ngx.say(cjson.encode({error = "method_not_allowed"}))
        return
    end

    local httpc = http.new()
    httpc:set_timeout(5000)

    local host_field, connect_target = parse_sidecar_addr(SIDECAR_ADDR)
    local pool_name = "sidecar:watch_ui_snapshot:" .. SIDECAR_ADDR
    local ok, conn_err
    if type(connect_target) == "table" then
        ok, conn_err = httpc:connect{
            scheme = "http",
            host = connect_target.host,
            port = connect_target.port,
            pool = pool_name,
        }
    else
        ok, conn_err = httpc:connect{
            scheme = "http",
            host = connect_target,
            pool = pool_name,
        }
    end
    if not ok then
        ngx.status = 502
        ngx.header["Content-Type"] = "application/json"
        ngx.say(cjson.encode({error = "sidecar unreachable", detail = conn_err or "connect failed"}))
        return
    end

    local req_headers = ngx.req.get_headers()
    local headers = {
        ["Host"] = host_field,
        ["X-Request-ID"] = ngx.var.request_id,
    }
    if req_headers["Authorization"] then
        headers["Authorization"] = req_headers["Authorization"]
    end

    local res, req_err = httpc:request{
        method = "GET",
        path = ngx.var.uri,
        headers = headers,
    }
    if not res then
        httpc:close()
        ngx.status = 502
        ngx.header["Content-Type"] = "application/json"
        ngx.say(cjson.encode({error = "sidecar request failed", detail = req_err or "unknown"}))
        return
    end

    local resp_body = res:read_body()
    httpc:set_keepalive(10000, 4)
    ngx.status = res.status
    ngx.header["Content-Type"] = res.headers["Content-Type"] or "application/json"
    if resp_body and #resp_body > 0 then
        ngx.print(resp_body)
    end
end

-- ---------------------------------------------------------------------------
-- Librarian — identity/memory proxy and commits
-- ---------------------------------------------------------------------------

--- Fetch Identity + Memory context for a tenant.
-- @param tenant_id string  The tenant ID
-- @return table|nil  Context result (identity, memory)
-- @return string|nil Error
function _M.librarian_context(tenant_id)
    local httpc = http.new()
    httpc:set_timeout(SIDECAR_TIMEOUT_MS)

    local host_field, connect_target = parse_sidecar_addr(SIDECAR_ADDR)
    local pool_name = "sidecar:librarian:" .. SIDECAR_ADDR

    local ok, conn_err
    if type(connect_target) == "table" then
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target.host,
            port   = connect_target.port,
            pool   = pool_name,
        }
    else
        ok, conn_err = httpc:connect{
            scheme = "http",
            host   = connect_target,
            pool   = pool_name,
        }
    end
    if not ok then return nil, "sidecar unreachable: " .. (conn_err or "connect failed") end

    local res, req_err = httpc:request{
        method  = "GET",
        path    = "/librarian/context/" .. tenant_id,
        headers = { ["Host"] = host_field },
    }
    if not res then
        httpc:close()
        return nil, "sidecar request failed: " .. (req_err or "unknown")
    end

    local body, body_err = res:read_body()
    httpc:set_keepalive(60000, 10)

    if res.status ~= 200 then
        return nil, "sidecar returned " .. res.status .. ": " .. body
    end

    local decoded, decode_err = cjson.decode(body)
    if not decoded then
        return nil, "sidecar response parse error: " .. (decode_err or "empty body")
    end

    return decoded, nil
end

--- Submit a commit proposal to Librarian.
-- @param tenant_id      string
-- @param causal_fire_id string
-- @param content        string
-- @param weight         number|nil
-- @return table|nil  Result
-- @return string|nil Error
function _M.librarian_commit(tenant_id, causal_fire_id, content, weight)
    return sidecar_post("/librarian/commit", {
        tenant_id      = tenant_id,
        causal_fire_id = causal_fire_id,
        content        = content,
        weight         = weight or 1.0,
    })
end

return _M
