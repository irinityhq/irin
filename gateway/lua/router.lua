-- ==========================================================================
-- router.lua — Access-phase upstream selector with sidecar integration.
--
-- Hot path sequence (when sidecar is up):
--   1. Guard    — decontaminate input (fail-closed)
--   2. Cache    — check for cached response (short-circuit on hit)
--   3. Route    — smart model selection (quality/balanced/economy/speed)
--   4. Budget   — pre-flight cost check
--   5. Policy   — sensitivity firewall check
--   6. Translate — convert body to provider-native format (Rosetta layer)
--   7. → proxy_pass to resolved provider
--
-- Fallback: If sidecar is unreachable, falls back to direct routing
-- via config.resolve_model() (the pre-sidecar path).
--
-- All providers supported via universal translator.
-- ==========================================================================

local cjson      = require "cjson.safe"
local http       = require "resty.http"
local config     = require "config"
local sidecar    = require "sidecar"
local translator = require "translator"
local hash       = require "lib.hash"
local ledger     = require "lib.ledger"
local shape_gate = require "lib.shape_gate"
local content    = require "lib.content"
local council_transport = require "lib.council_transport"

-- Module-level binding for the ledger retry helper. See lib/ledger.lua for
-- the rationale; closures capture this function reference, not a module
-- table, so a hot-reload mid-request cannot silently divert open-end
-- writes. CI lint at test/lint-timer-closures.sh enforces that
-- `sidecar\.` never appears inside an `ngx.timer.at` body.
local ledger_record   = ledger.record_with_retry
local ledger_schedule = ledger.schedule
-- P2-B: pin sidecar.council_unlock at module load so the ngx.on_abort
-- handler registered below holds a function pointer, not a table lookup.
-- The abort closure fires AFTER the access phase exits — same hot-reload
-- limbo as ngx.timer.at — so a swapped sidecar module table could
-- otherwise silently divert in-flight unlocks. (The structural lint at
-- test/lint-timer-closures.sh scans `ngx.timer.at` bodies only; the same
-- defense applies to on_abort by convention.)
local sidecar_council_unlock = sidecar.council_unlock

local streaming_enabled = os.getenv("GW_ENABLE_STREAMING") == "1"
local batch_enabled     = os.getenv("GW_ENABLE_BATCH") == "1"

-- =========================================================================
-- Batch API endpoint map
--
-- Phase 5 item 3: Batch passthrough. Per-provider base path for batch
-- create / status / results. The gateway never inspects the batch contents
-- (file_id ref for OpenAI, inline requests[] for Anthropic) — it just
-- rewrites the URI from /v1/batches[...] to the provider's batch endpoint.
--
-- For Anthropic, the canonical batch endpoint is /v1/messages/batches.
-- For OpenAI, it's /v1/batches. We map both to a fixed prefix and append
-- whatever path component followed /v1/batches in the original URI.
--
-- Caller MUST supply X-Provider: openai|anthropic on every batch request.
-- The gateway has no `model` field to introspect (batch creation puts the
-- model per-line inside the JSONL on the file server for OpenAI, or per
-- requests[] item for Anthropic), so provider selection is explicit.
-- =========================================================================
local BATCH_PROVIDER_PREFIX = {
    openai    = "/v1/batches",
    anthropic = "/v1/messages/batches",
}

local _M = {}

--- Send a JSON error response (no string concat with user input)
--- @param status number HTTP status code
--- @param message string Human-readable error message
--- @param err_type string|nil Error type (OpenAI-compatible)
--- @param err_code string|nil Stable error code for metrics/alerting
local function json_error(status, message, err_type, err_code)
    ngx.status = status
    ngx.header["Content-Type"] = "application/json"
    local error_obj = {
        message = message,
        type    = err_type or "invalid_request",
    }
    if err_code then
        error_obj.code = err_code
        local m = ngx.shared.gw_metrics
        if m then m:incr("errors_total:" .. err_code, 1, 0) end
    end
    ngx.say(cjson.encode({ error = error_obj }))
    return ngx.exit(status)
end

-- Multimodal-aware text extraction now lives in lib/content.lua so other
-- call sites (notably the council translator at lua/translator.lua) can
-- reuse the depth cap + recursive descent. The wrapper below keeps the
-- router's call-site signature stable (returns the text, or nil) while
-- propagating the over-depth error via ngx.ctx for the caller below to
-- translate into a 400. See §5.2 / §12.6.
local function extract_text_content(c, depth)
    local text, err = content.extract_text(c, depth or 0)
    if err == "content_depth_exceeded" then
        -- Stash for the route() body to surface as 400 instead of silently
        -- treating the request as text-less.
        ngx.ctx.gw_content_depth_exceeded = true
        return nil
    end
    return text
end

--- Count user-provided "messages" across the two request shapes the gateway
-- accepts. The number is auditable metadata — it lets a chain reviewer see
-- the shape of a request from the ledger alone (without storing the body).
--
-- chat-completions: req.messages is an array of {role, content}
-- responses:        req.input is either a string (1) or an array of items
local function message_count(req)
    if type(req.messages) == "table" then
        return #req.messages
    end
    if type(req.input) == "string" then
        return 1
    end
    if type(req.input) == "table" then
        return #req.input
    end
    return 0
end

--- Authenticate the request — auth + IP gate.
-- Shared between the main /v1/ path and the /v1/batches passthrough so
-- both surfaces apply the same gating. On failure, json_error is called
-- and the function returns `false`; on success returns
-- `true, raw_key, auth_result, client_ip`.
local function authenticate_request(headers)
    local client_ip = ngx.var.remote_addr or "127.0.0.1"

    local auth_header = headers["authorization"]
    local x_api_key   = headers["x-api-key"]
    local raw_key     = ""

    if auth_header and auth_header:match("^Bearer%s+(.+)") then
        raw_key = auth_header:match("^Bearer%s+(.+)")
    elseif x_api_key then
        raw_key = x_api_key
    end

    if raw_key == "" then
        local m = ngx.shared.gw_metrics
        if m then m:incr("auth_rejections_total:missing_key", 1, 0) end
        json_error(401, "Authentication required. Provide 'Authorization: Bearer <key>' or 'X-API-Key: <key>'",
                   "unauthorized", "ERR_AUTH_MISSING")
        return false
    end

    local auth_result = sidecar.auth_check(raw_key, client_ip)
    if not auth_result.allowed then
        local m = ngx.shared.gw_metrics
        if auth_result.rate_limit_limit and auth_result.rate_limit_limit > 0
                and auth_result.rate_limit_remaining == 0 then
            if m then m:incr("auth_rejections_total:rate_limit", 1, 0) end
            ngx.header["X-RateLimit-Limit"]     = auth_result.rate_limit_limit
            ngx.header["X-RateLimit-Remaining"] = 0
            ngx.header["X-RateLimit-Reset"]     = auth_result.rate_limit_reset
            json_error(429, auth_result.reason or "Rate limit exceeded",
                       "rate_limit_exceeded", "ERR_RATE_LIMIT")
            return false
        end
        if m then m:incr("auth_rejections_total:invalid_key", 1, 0) end
        json_error(401, auth_result.reason or "Invalid API key",
                   "unauthorized", "ERR_AUTH_INVALID")
        return false
    end

    ngx.header["X-RateLimit-Limit"]     = auth_result.rate_limit_limit or 0
    ngx.header["X-RateLimit-Remaining"] = auth_result.rate_limit_remaining or 0
    ngx.header["X-RateLimit-Reset"]     = auth_result.rate_limit_reset or 0

    local ip_result = sidecar.ip_check(client_ip)
    if ip_result and not ip_result.allowed then
        local m = ngx.shared.gw_metrics
        if m then m:incr("ip_gate_blocks_total", 1, 0) end
        json_error(403, ip_result.reason or "IP address blocked",
                   "ip_blocked", "ERR_IP_BLOCKED")
        return false
    end

    ngx.req.clear_header("Authorization")
    ngx.req.clear_header("X-API-Key")

    return true, raw_key, auth_result, client_ip
end

--- Handle a batch API passthrough request (/v1/batches[/...]).
--
-- Skips: decon (no scannable text — file refs / inline batch payload),
-- cache (jobs are not cacheable), translate (passthrough), policy
-- (no last_user_content to evaluate), routing decision (provider is
-- explicit via X-Provider header). Auth, IP gate, rate limit, budget
-- pre-check ($0 estimate), and ledger still apply.
--
-- NOTE: For Anthropic batch creation, requests[] is inline content —
-- skipping decon here means inline-batch prompts bypass the input guard.
-- This is a deliberate trade-off: the guard scans last_user_content
-- which doesn't exist for batch shape, and decon is not designed to
-- walk an arbitrary requests[] array. Callers who want guard coverage
-- on inline batches should pre-scan client-side. Recorded as
-- batch_mode=true on the ledger so an auditor can find them.
local function route_batch(headers, raw_body)
    if not batch_enabled then
        return json_error(501, "batch API not enabled — set GW_ENABLE_BATCH=1",
            "not_implemented", "ERR_BATCH_DISABLED")
    end

    local request_method = ngx.var.request_method or "GET"
    local uri = ngx.var.uri or "/v1/batches"

    -- Provider selection (explicit, header-driven)
    local provider_name = headers["x-provider"]
    if not provider_name or provider_name == "" then
        return json_error(400,
            "X-Provider header required for batch operations (openai|anthropic)",
            "invalid_request", "ERR_BATCH_PROVIDER_MISSING")
    end
    provider_name = provider_name:lower()
    local prefix = BATCH_PROVIDER_PREFIX[provider_name]
    if not prefix then
        return json_error(400,
            "unsupported batch provider: " .. provider_name ..
            " (supported: openai, anthropic)",
            "invalid_request", "ERR_BATCH_PROVIDER_UNSUPPORTED")
    end

    -- Auth + IP gate
    local ok, raw_key, auth_result, client_ip = authenticate_request(headers)
    if not ok then return end

    local budget_key = headers["x-budget-key"] or ""
    if auth_result.budget_key and auth_result.budget_key ~= "" then
        budget_key = auth_result.budget_key
    end
    local caller_key = (auth_result.key_id and auth_result.key_id ~= "")
                       and auth_result.key_id or ""

    -- Determine batch operation (create | status | results | list | cancel)
    -- The op label is for the audit ledger; the request itself is just
    -- forwarded with the URI rewritten.
    local batch_op = "list"
    local id_segment = uri:match("^/v1/batches/(.+)$")
    if id_segment then
        if id_segment:find("/output", 1, true)
                or id_segment:find("/results", 1, true) then
            batch_op = "results"
        elseif id_segment:find("/cancel", 1, true) then
            batch_op = "cancel"
        else
            batch_op = "status"
        end
    elseif request_method == "POST" then
        batch_op = "create"
    end

    -- Resolve provider config (base_url, auth header, api key)
    local provider = config.providers[provider_name]
    if not provider then
        return json_error(502, "provider not configured: " .. provider_name,
                          "gateway_error", "ERR_PROVIDER_MISSING")
    end
    local api_key = config.get_api_key(provider_name)
    if api_key == "" then
        return json_error(502,
            "API key not configured for provider: " .. provider_name,
            "gateway_error", "ERR_PROVIDER_KEY_MISSING")
    end

    -- Rewrite URI: /v1/batches[/rest] -> {prefix}[/rest]
    -- For OpenAI both are /v1/batches so this is a no-op; for Anthropic
    -- /v1/batches becomes /v1/messages/batches.
    local tail = uri:sub(#"/v1/batches" + 1)  -- "" or "/abc..." (with leading /)
    local new_path = prefix .. tail
    if ngx.var.is_args == "?" then
        new_path = new_path .. "?" .. (ngx.var.args or "")
    end

    local target = provider.base_url .. new_path
    local host = provider.base_url:match("https?://([^/]+)") or ""

    ngx.var.target_url  = target
    ngx.var.target_host = host

    -- Provider-specific auth (Anthropic = x-api-key, others = Authorization)
    local auth_header_name = provider.auth_header or "Authorization"
    if auth_header_name == "x-api-key" then
        ngx.req.set_header("x-api-key", api_key)
        ngx.var.auth_value = ""
    else
        ngx.var.auth_value = (provider.auth_prefix or "Bearer ") .. api_key
    end
    if provider.extra_headers then
        for k, v in pairs(provider.extra_headers) do
            ngx.req.set_header(k, v)
        end
    end
    -- NOTE: Anthropic's Message Batches API requires the `anthropic-beta:
    -- message-batches-2024-09-24` header. The gateway intentionally does
    -- NOT inject this — passthrough means the caller controls beta opt-ins.
    -- Client-supplied headers other than auth are forwarded; if a 400 comes
    -- back from upstream about the beta header, the client should add it.

    -- Build the canonical record. batch_mode = true short-circuits the
    -- usage parser in cost.lua and selects the batch ledger action.
    local request_id = ngx.var.request_id
    ngx.ctx.gw = {
        record = {
            t0                          = ngx.now(),
            request_id                  = request_id,
            raw_body                    = raw_body or "",
            alias                       = "batch:" .. provider_name,
            last_user_content           = nil,
            sensitivity                 = "GREEN",
            council_role                = "none",
            routing_strategy            = "",
            budget_key                  = budget_key ~= "" and budget_key or "default",
            caller_key                  = caller_key,
            message_count               = 0,
            is_streaming                = false,
            -- Batch-specific:
            batch_mode                  = true,
            batch_op                    = batch_op,
            batch_provider              = provider_name,
            -- Derived (filled to satisfy cost.lua's contract):
            resolved_model              = "batch:" .. provider_name,
            provider                    = provider_name,
            pricing                     = {},
            requested_model             = "batch:" .. provider_name,
            effective_model             = "batch:" .. provider_name,
            needs_response_translation  = false,
            stream_translate_as         = nil,
        },
    }
    local record = ngx.ctx.gw.record

    -- Ledger: batch_received open-end event. Mirrors request_received but
    -- carries action=batch_<op> + batch_mode in metadata so a chain reviewer
    -- can audit batch traffic separately. raw_body_sha256 is "" for GETs.
    local fb_request_id  = record.request_id
    local fb_alias       = record.alias
    local fb_raw_body    = record.raw_body
    local fb_raw_body_sz = #(record.raw_body or "")
    local fb_caller_key  = record.caller_key
    local fb_op          = batch_op
    local fb_provider    = provider_name
    local fb_method      = request_method
    local fb_uri         = uri
    ledger_schedule("batch_received", fb_request_id, function(premature)
        if premature then return end
        ledger_record("client", fb_alias, {
            request_id          = fb_request_id,
            raw_body_sha256     = fb_raw_body_sz > 0 and hash.body_sha256_hex(fb_raw_body) or "",
            raw_body_size_bytes = fb_raw_body_sz,
            message_count       = 0,
        }, {
            action       = "batch_" .. fb_op,
            request_id   = fb_request_id,
            batch_mode   = true,
            batch_op     = fb_op,
            provider     = fb_provider,
            method       = fb_method,
            uri          = fb_uri,
        }, fb_caller_key)
    end)

    -- Budget pre-check. Cost is unknown at create time (provider returns
    -- a job ID, not usage); we estimate $0 to allow the job through but
    -- still surface a hard cap if the budget is exhausted. Real spend is
    -- recorded when the caller fetches /output and the JSONL is parsed.
    if record.budget_key and record.budget_key ~= "default" then
        local budget_result = sidecar.budget_check(record.budget_key, 0)
        if budget_result and not budget_result.allowed then
            local fb_bk      = record.budget_key
            local fb_reason  = budget_result.reason
            local fb_alias_b = record.alias
            local fb_req_b   = record.request_id
            local fb_ck_b    = record.caller_key
            ledger_schedule("budget_check", fb_req_b, function(premature)
                if premature then return end
                ledger_record("gateway", fb_alias_b, {
                    request_id = fb_req_b,
                    budget_key = fb_bk,
                }, {
                    action     = "budget_check",
                    decision   = "blocked",
                    request_id = fb_req_b,
                    budget_key = fb_bk,
                    reason     = fb_reason,
                    batch_mode = true,
                }, fb_ck_b)
            end)
            return json_error(429,
                "budget exceeded: " .. (budget_result.reason or "limit reached"),
                "budget_exceeded", "ERR_BUDGET_EXCEEDED")
        end
    end

    -- Provenance headers
    ngx.header["X-Routed-Provider"] = provider_name
    ngx.header["X-Batch-Op"]        = batch_op

    ngx.log(ngx.INFO, "router: batch ", batch_op, " -> ", provider_name,
            " @ ", target)
end

function _M.route()
    -- =====================================================================
    -- BATCH PASSTHROUGH BRANCH
    -- /v1/batches and /v1/batches/* are forwarded to the provider's batch
    -- endpoint. Auth + budget + ledger still apply; decon, cache, translate,
    -- and routing decisions are skipped. See route_batch() for details.
    -- =====================================================================
    local uri = ngx.var.uri or ""
    if uri == "/v1/batches" or uri:sub(1, #"/v1/batches/") == "/v1/batches/" then
        local headers = ngx.req.get_headers()
        -- For methods with a body (POST/PATCH) read it; GET status/output
        -- have no body and ngx.req.read_body() is a no-op + safe to skip.
        local method = ngx.var.request_method or "GET"
        local raw_body = ""
        if method == "POST" or method == "PUT" or method == "PATCH" then
            ngx.req.read_body()
            raw_body = ngx.req.get_body_data() or ""
            if raw_body == "" then
                local body_file = ngx.req.get_body_file()
                if body_file then
                    local fh = io.open(body_file, "r")
                    if fh then
                        raw_body = fh:read("*a") or ""
                        fh:close()
                    end
                end
            end
        end
        return route_batch(headers, raw_body)
    end

    -- =====================================================================
    -- STEP 0: Read body and build the canonical RequestRecord.
    --
    -- The record is the single source of truth for facts about this request.
    -- It contains ONLY strings and scalars — no Lua tables — so downstream
    -- phases (body_filter, log) cannot mutate request state via shared refs.
    -- The decoded `req` table is local to this function and is never stashed
    -- on ngx.ctx; if downstream phases need a field, it goes on the record.
    --
    -- See COUNCIL_GATEWAY_CONTRACT.md for header semantics.
    -- =====================================================================
    ngx.req.read_body()
    local raw_body = ngx.req.get_body_data()

    -- Body-file fallback for requests > client_body_buffer_size
    if not raw_body then
        local body_file = ngx.req.get_body_file()
        if body_file then
            local fh, err = io.open(body_file, "r")
            if fh then
                raw_body = fh:read("*a")
                fh:close()
            else
                ngx.log(ngx.ERR, "router: failed to read body file: ", err)
            end
        end
    end

    if not raw_body or raw_body == "" then
        ngx.log(ngx.WARN, "router: empty request body")
        return json_error(400, "empty request body", nil, "ERR_EMPTY_BODY")
    end

    local req, err = cjson.decode(raw_body)
    if not req then
        ngx.log(ngx.WARN, "router: invalid JSON: ", err)
        return json_error(400, "invalid JSON", nil, "ERR_INVALID_JSON")
    end

    local alias = req.model or ""
    if alias == "" then
        return json_error(400, "model field required", nil, "ERR_MODEL_REQUIRED")
    end

    -- Detect Responses API shape BEFORE any normalization mutates the request.
    -- A request is Responses-shape iff it has `input` (string or array) and no
    -- `messages`. Presence of `instructions` alone does not flip this — the
    -- chat/completions shape never uses `instructions`, but we tolerate clients
    -- that supply both `messages` + `instructions` by treating it as chat.
    -- This flag is propagated to the record so cost.lua and the cache-hit path
    -- know to re-emit responses in Responses output[] shape.
    local is_responses_api = (req.input ~= nil) and (req.messages == nil)

    -- =====================================================================
    -- ASM SHAPE GATE: Structural limits check
    -- Drop massive/malformed requests before hitting the Rust decontaminator
    -- =====================================================================
    local shape_err = shape_gate.validate(req, alias, #raw_body)
    if shape_err then
        ngx.log(ngx.WARN, "router: shape gate blocked: ", shape_err.error)
        -- Phase 3 metrics stub: gw_shape_gate_violations_total
        local metrics = ngx.shared.gw_metrics
        if metrics then
            metrics:incr("shape_gate_violations_total:" .. shape_err.field, 1, 0)
        end
        return json_error(400, shape_err.error, "shape_gate_violation", "ERR_SHAPE_GATE")
    end

    -- Normalize stream field: accept true, 1, "true" — reject others
    local is_streaming = req.stream == true or req.stream == 1 or req.stream == "true"
    if req.stream and not is_streaming then
        return json_error(400, "stream must be true or false", "invalid_request", "ERR_STREAM_INVALID")
    end
    if is_streaming and not streaming_enabled then
        -- Council aliases (§5.4, §9 test #4) MUST surface 400 streaming_unsupported
        -- regardless of the global gate. They are deliberately non-streaming and
        -- own their own error contract; let the council branch / translator
        -- produce ERR_STREAMING_UNSUPPORTED.
        if not string.match(alias, "^council%-") then
            return json_error(501, "streaming not enabled — set GW_ENABLE_STREAMING=1",
                              "not_implemented", "ERR_STREAMING_DISABLED")
        end
    end

    -- Phase 5 complete: Responses API streaming wraps chat.completion.chunk
    -- SSE into response.* events in body_filter (via lib/responses_stream.lua).
    -- Native Responses upstreams (path ends /v1/responses) pass through.

    -- Extract the last user message's text content for guard/policy scanning.
    -- This is captured ONCE and stored as a string on the record; downstream
    -- phases never re-derive from the (possibly translator-mutated) req table.
    --
    -- Handles both shapes:
    --   - chat-completions: req.messages[i].content as string OR array of parts
    --   - responses API:    req.input as string OR array of {role, content}
    --
    -- extract_text_content sets ngx.ctx.gw_content_depth_exceeded on
    -- over-depth multimodal nesting (spec §12.6 / P1 #18). Caught below and
    -- surfaced as 400 instead of silently treating the request as text-less.
    ngx.ctx.gw_content_depth_exceeded = nil
    local last_user_content = nil
    if req.messages then
        for i = #req.messages, 1, -1 do
            if req.messages[i].role == "user" then
                last_user_content = extract_text_content(req.messages[i].content)
                break
            end
        end
    elseif req.input then
        if type(req.input) == "string" then
            last_user_content = req.input
        elseif type(req.input) == "table" then
            for i = #req.input, 1, -1 do
                local item = req.input[i]
                if type(item) == "table" and item.role == "user" then
                    last_user_content = extract_text_content(item.content)
                    break
                end
            end
        end
    end
    if ngx.ctx.gw_content_depth_exceeded then
        return json_error(400, "Multimodal content nesting exceeded depth cap.",
                          "invalid_request_error", "ERR_CONTENT_DEPTH_EXCEEDED")
    end

    -- =====================================================================
    -- Council depth-header capture-strip (spec §5.6).
    --
    -- X-Council-* and X-Parent-Request-Id arriving from an external caller
    -- are NEVER trusted on entry. We stash their values, clear them from
    -- the request so no downstream phase (or upstream provider call) can
    -- observe them, and only restore them if the auth-resolved key has
    -- service_role == "council" AND key_id == COUNCIL_GATEWAY_KEY_ID. Two
    -- factors required: an admin can't elevate a key just by setting
    -- service_role; the key_id must ALSO match the env var. See §5.6 for
    -- the rationale.
    -- =====================================================================
    local _pre_strip_headers = ngx.req.get_headers()
    local stashed = {
        depth         = _pre_strip_headers["x-council-depth"],
        session_id    = _pre_strip_headers["x-council-session-id"],
        request_id    = _pre_strip_headers["x-council-request-id"],
        parent_req_id = _pre_strip_headers["x-parent-request-id"],
        transport_id  = _pre_strip_headers["x-council-transport-id"]
                        or _pre_strip_headers["x-council-original-provider"],
    }
    for _, h in ipairs({ "X-Council-Depth", "X-Council-Session-Id",
                         "X-Council-Request-ID", "X-Parent-Request-Id",
                         "X-Council-Transport-ID",
                         "X-Council-Original-Provider" }) do
        ngx.req.clear_header(h)
    end

    -- Read contract headers (strings only). Defaults match COUNCIL_GATEWAY_CONTRACT.md.
    local headers = ngx.req.get_headers()
    local sensitivity      = headers["x-sensitivity-level"]   or "GREEN"
    local council_role     = headers["x-council-role"]        or "none"
    local routing_strategy = headers["x-routing-strategy"]    or ""
    local budget_key       = headers["x-budget-key"]          or ""
    local sovereign_mode   = false
    local sov_hdr = headers["x-sovereign-mode"]
    if sov_hdr and (sov_hdr == "true" or sov_hdr == "1" or sov_hdr == "TRUE") then
        sovereign_mode = true
    end

    local client_ip = ngx.var.remote_addr or "127.0.0.1"

    -- T5: source-IP trust for sensitive headers removed (now purely auth-gated; see raw_key/auth_result below).
    -- Una authed requests 401 before sensitive headers can affect routing/budget/sovereign decisions.

    -- MCP passthrough — bypass decon only for loopback callers with X-Tool-Format: mcp
    local mcp_passthrough = false
    local tool_format = headers["x-tool-format"]
    if tool_format and type(tool_format) == "string" and tool_format:lower() == "mcp" then
        if client_ip == "127.0.0.1" or client_ip == "::1" then
            mcp_passthrough = true
        else
            ngx.log(ngx.WARN, "router: X-Tool-Format: mcp ignored from non-loopback client_ip=", client_ip)
        end
    end

    -- =====================================================================
    -- AUTHENTICATION & RATE LIMITING
    -- =====================================================================
    local auth_header = headers["authorization"]
    local x_api_key = headers["x-api-key"]
    local raw_key = ""

    if auth_header and auth_header:match("^Bearer%s+(.+)") then
        raw_key = auth_header:match("^Bearer%s+(.+)")
    elseif x_api_key then
        raw_key = x_api_key
    end

    if raw_key == "" then
        local m = ngx.shared.gw_metrics
        if m then m:incr("auth_rejections_total:missing_key", 1, 0) end
        return json_error(401, "Authentication required. Provide 'Authorization: Bearer <key>' or 'X-API-Key: <key>'", "unauthorized", "ERR_AUTH_MISSING")
    end

    local auth_result = sidecar.auth_check(raw_key, client_ip)

    if not auth_result.allowed then
        local m = ngx.shared.gw_metrics
        if auth_result.rate_limit_limit and auth_result.rate_limit_limit > 0 and auth_result.rate_limit_remaining == 0 then
            if m then m:incr("auth_rejections_total:rate_limit", 1, 0) end
            ngx.header["X-RateLimit-Limit"] = auth_result.rate_limit_limit
            ngx.header["X-RateLimit-Remaining"] = 0
            ngx.header["X-RateLimit-Reset"] = auth_result.rate_limit_reset
            return json_error(429, auth_result.reason or "Rate limit exceeded", "rate_limit_exceeded", "ERR_RATE_LIMIT")
        end
        if m then m:incr("auth_rejections_total:invalid_key", 1, 0) end
        return json_error(401, auth_result.reason or "Invalid API key", "unauthorized", "ERR_AUTH_INVALID")
    end

    -- Inject rate limit headers for successful requests
    ngx.header["X-RateLimit-Limit"] = auth_result.rate_limit_limit or 0
    ngx.header["X-RateLimit-Remaining"] = auth_result.rate_limit_remaining or 0
    ngx.header["X-RateLimit-Reset"] = auth_result.rate_limit_reset or 0

    -- Override budget_key with the securely mapped one from the Auth config.
    -- Prevents client spoofing of X-Budget-Key header.
    if auth_result.budget_key and auth_result.budget_key ~= "" then
        budget_key = auth_result.budget_key
    end

    -- Capture the resolved per-key identity. Empty string when authenticated
    -- but the sidecar didn't return a key_id (older sidecar / pre-Task-1
    -- compatibility); ledger writes pass nil for that case so the column
    -- stores NULL rather than "".
    local caller_key = (auth_result.key_id and auth_result.key_id ~= "") and auth_result.key_id or ""

    -- IP policy check (after auth, before guard). CIDR-based gate evaluated
    -- in the sidecar; fail-closed (sidecar unreachable -> 403). The default
    -- policy passes external IPs — this is an opt-in deny mechanism.
    local ip_result = sidecar.ip_check(client_ip)
    if ip_result and not ip_result.allowed then
        local m = ngx.shared.gw_metrics
        if m then m:incr("ip_gate_blocks_total", 1, 0) end
        return json_error(403, ip_result.reason or "IP address blocked", "ip_blocked", "ERR_IP_BLOCKED")
    end

    -- Clear sensitive auth headers so they don't accidentally leak upstream
    ngx.req.clear_header("Authorization")
    ngx.req.clear_header("X-API-Key")

    -- Build the canonical RequestRecord. Strings and scalars only — frozen at ingress.
    -- Derived fields (resolved_model, provider, pricing) get appended after route resolution.
    ngx.ctx.gw = {
        record = {
            t0                          = ngx.now(),
            request_id                  = ngx.var.request_id,
            raw_body                    = raw_body,           -- frozen string, used for cache key
            alias                       = alias,              -- e.g. "opus" — pre-resolution name from client
            last_user_content           = last_user_content,  -- string or nil; for guard/policy
            sensitivity                 = sensitivity,        -- "GREEN" | "YELLOW" | "RED"
            council_role                = council_role,       -- ledger metadata tag, no behavior
            routing_strategy            = routing_strategy,
            budget_key                  = budget_key,
            caller_key                  = caller_key,        -- per-key audit identity (key_id from /auth/check)
            -- §5.6: identity bag for downstream council restore + ledger
            -- annotation. `service_role` is the immutable AuthKey role tag
            -- (only "council" today), distinct from the admin-mutable
            -- `budget_key`. Two-factor: the restore guard requires
            -- BOTH service_role == "council" AND key_id == COUNCIL_GATEWAY_KEY_ID.
            auth                        = {
                key_id       = auth_result.key_id or "",
                service_role = auth_result.service_role,
            },
            parent_council_request_id   = "",
            requested_transport         = "",
            message_count               = message_count(req),
            is_streaming                = is_streaming,
            is_responses_api            = is_responses_api,  -- Phase 5: re-emit output[] on response
            mcp_passthrough             = mcp_passthrough,   -- Phase 5: decon bypass for MCP
            -- Derived (filled in below):
            resolved_model              = nil,
            provider                    = nil,
            pricing                     = nil,
            needs_response_translation  = false,
        },
    }
    local record = ngx.ctx.gw.record

    -- =====================================================================
    -- Council header restore (spec §5.6).
    --
    -- Dual condition: service_role == "council" AND key_id matches the
    -- env-pinned COUNCIL_GATEWAY_KEY_ID. Re-provisioning a key with the role
    -- but a different key_id will NOT restore — the operator must update
    -- the env var too, making this a deliberate two-step privileged op.
    -- =====================================================================
    local COUNCIL_GATEWAY_KEY_ID = os.getenv("COUNCIL_GATEWAY_KEY_ID")
    local is_council_service = council_transport.is_trusted_council(
        record.auth.service_role,
        record.auth.key_id,
        COUNCIL_GATEWAY_KEY_ID
    )
    local transport_restore_denied = stashed.transport_id ~= nil and not is_council_service

    if is_council_service then
        if stashed.depth         then ngx.req.set_header("X-Council-Depth", stashed.depth) end
        if stashed.session_id    then ngx.req.set_header("X-Council-Session-Id", stashed.session_id) end
        if stashed.request_id    then ngx.req.set_header("X-Council-Request-ID", stashed.request_id) end
        if stashed.parent_req_id then ngx.req.set_header("X-Parent-Request-Id", stashed.parent_req_id) end
        record.parent_council_request_id = stashed.parent_req_id or ""
        record.requested_transport = stashed.transport_id or ""
    end

    -- =====================================================================
    -- Ledger: request_received — fires for EVERY request that parsed past
    -- JSON validation, BEFORE any short-circuit (guard / cache / route /
    -- budget / policy) can ngx.exit. This is the chain's open-end. Every
    -- request_received pairs with exactly ONE terminating event later in
    -- this function or in cost.lua's outbound_response.
    --
    -- Payload contains an Aurelius minimum: the body's SHA-256 (so an
    -- auditor can re-verify if the body is preserved elsewhere), its byte
    -- length (so caps and outliers are visible), and the message count
    -- (gives request shape without storing content). The hash is computed
    -- inside the timer to keep ~5ms off the hot path.
    -- =====================================================================
    local fb_request_id  = record.request_id
    local fb_alias_open  = record.alias
    local fb_raw_body    = record.raw_body
    local fb_raw_body_sz = #raw_body
    local fb_msg_count   = record.message_count
    local fb_sensitivity = record.sensitivity
    local fb_council     = record.council_role
    local fb_caller_key  = record.caller_key
    ledger_schedule("request_received", fb_request_id, function(premature)
        if premature then return end
        ledger_record("client", fb_alias_open, {
            request_id          = fb_request_id,
            raw_body_sha256     = hash.body_sha256_hex(fb_raw_body),
            raw_body_size_bytes = fb_raw_body_sz,
            message_count       = fb_msg_count,
        }, {
            action       = "request_received",
            request_id   = fb_request_id,
            sensitivity  = fb_sensitivity,
            council_role = fb_council,
        }, fb_caller_key)
    end)

    -- Never reinterpret a denied exact-transport request as an ordinary smart
    -- route. This turns setup/key drift into a loud failure instead of silently
    -- changing the provider behind a governed Council proceeding.
    if transport_restore_denied then
        local denied_alias = record.alias
        local denied_req_id = record.request_id
        local denied_caller = record.caller_key
        ledger_schedule("route_decide", denied_req_id, function(premature)
            if premature then return end
            ledger_record("gateway", denied_alias, { request_id = denied_req_id }, {
                action = "route_decide",
                decision = "rejected",
                request_id = denied_req_id,
                reason = "council_transport_identity_denied",
            }, denied_caller)
        end)
        return json_error(403,
            "Council transport identity was not authorized for this key",
            "forbidden", "ERR_COUNCIL_TRANSPORT_IDENTITY")
    end

    if record.mcp_passthrough then
        ngx.log(ngx.INFO, "router: MCP passthrough — decon bypass for request_id=", record.request_id)
        local m = ngx.shared.gw_metrics
        if m then m:incr("mcp_passthrough_total", 1, 0) end
    end
    -- Internal council seat/chair calls: when the caller is the
    -- service-role council key AND a parent_council_request_id was
    -- restored (set only after the §5.6 strip-restore for trusted
    -- service-role callers), we're scanning an internal LLM→LLM message
    -- with prior-model output in its context. Decon is tuned for
    -- external user input and false-positives on synthesized transcripts
    -- (persona-driven prompts, adversarial seat outputs). Skip to keep
    -- council chair synthesis viable end-to-end; the outer council
    -- request was already decon'd at session entry.
    local council_internal = record.parent_council_request_id ~= nil
                             and record.parent_council_request_id ~= ""
    if council_internal then
        ngx.log(ngx.INFO, "router: council-internal — decon bypass for request_id=",
                record.request_id, " parent=", record.parent_council_request_id)
        local m = ngx.shared.gw_metrics
        if m then m:incr("council_internal_decon_bypass_total", 1, 0) end
    end
    if last_user_content and not record.mcp_passthrough and not council_internal then
        local guard_result = sidecar.guard_input(last_user_content, "gateway")
        if guard_result and guard_result.blocked then
            ngx.log(ngx.WARN, "router: guard blocked request: ",
                    guard_result.blocked_reason or "threat detected")

            -- Log the blocked request to the ledger asynchronously.
            -- Capture frozen scalars from the record so the closure does not
            -- reach back into mutable Lua tables across phase boundaries.
            local guard_alias      = record.alias
            local guard_reason     = guard_result.blocked_reason
            local guard_req_id     = record.request_id
            local guard_caller_key = record.caller_key
            ledger_schedule("guard_input", guard_req_id, function(premature)
                if premature then return end
                ledger_record("gateway", guard_alias, {
                    request_id = guard_req_id,
                }, {
                    action     = "guard_input",
                    decision   = "blocked",
                    request_id = guard_req_id,
                    reason     = guard_reason,
                }, guard_caller_key)
            end)

            -- Phase 3 metrics stub: gw_decon_blocks_total
            local metrics = ngx.shared.gw_metrics
            if metrics then
                metrics:incr("decon_blocks_total:" .. (guard_result.verdict or "unknown"), 1, 0)
            end

            return json_error(403, "request blocked by security guard: " ..
                    (guard_result.blocked_reason or "threat detected"),
                    "content_filter", "ERR_GUARD_BLOCKED")
        end
    end

    -- =====================================================================
    -- STEP 2: Cache check — short-circuit on hit.
    -- Streaming requests skip cache entirely — a cached JSON response
    -- served to an SSE-expecting client would break the wire format.
    -- Cache key is hashed from (alias, raw_body) on the Rust side. Hashing
    -- the literal request bytes (not a re-encoded JSON form) avoids
    -- canonicalization drift between cjson and serde_json. See
    -- sidecar-rs/src/cache.rs::generate_cache_key.
    --
    -- The cache stores NATIVE upstream shape; we re-run translate_response
    -- on hit so the wire emitted to the client matches what a fresh request
    -- would have produced. The translator_version field on the entry
    -- invalidates anything written under a different translator pipeline.
    -- =====================================================================
    local cache_result
    -- Council models (council-triage, council-warroom) own idempotency
    -- via the peek/lock/claim path in their dedicated branch later in route().
    -- L1 cache short-circuits BEFORE that branch and would absorb identical-
    -- body replays without writing the council_replay ledger row or surfacing
    -- the X-Idempotency-Replay marker. Skip L1 cache for council models so
    -- the council branch owns the replay path end-to-end.
    local is_council_model = record.alias and string.sub(record.alias, 1, 8) == "council-"
    if is_streaming then
        ngx.log(ngx.DEBUG, "router: cache skip — streaming request")
    elseif is_council_model then
        ngx.log(ngx.DEBUG, "router: cache skip — council model (idempotency owned by council branch)")
    else
    cache_result = sidecar.cache_check(
        record.alias, record.raw_body, translator.TRANSLATOR_VERSION
    )
    -- Demote-to-miss guard: B1's fix made the cache intentionally store
    -- *native* upstream shape, with re-translation on hit being load-bearing.
    -- An entry with an empty provider field cannot be re-translated (the
    -- translator key is required); shipping the native shape to a client
    -- expecting normalized JSON would silently re-introduce the B1 corruption
    -- class. Treat empty-provider hits as MISSES — fall through to STEP 3 —
    -- and emit a structured ERR so producer-side bugs are observable.
    if cache_result and cache_result.hit
            and (not cache_result.provider or cache_result.provider == "") then
        ngx.log(ngx.ERR, cjson.encode({
            event       = "cache_hit_empty_provider_demoted_to_miss",
            request_id  = record.request_id,
            alias       = record.alias,
        }))
        cache_result = nil
    end
    if cache_result and cache_result.hit and record.requested_transport ~= "" then
        local transport_ok = council_transport.matches(
            record.requested_transport, cache_result.provider)
        if not transport_ok then
            ngx.log(ngx.INFO, "router: exact-transport cache mismatch; treating as miss")
            cache_result = nil
        end
    end
    if cache_result and cache_result.hit then
        local m = ngx.shared.gw_metrics
        if m then m:incr("cache_outcomes:hit", 1, 0) end
        local cached_provider = cache_result.provider
        local payload = cache_result.response
        if translator.needs_response_translation(cached_provider) then
            payload = translator.translate_response(cached_provider, payload)
        end
        -- Phase 5: re-emit Responses API shape on cache hits when the client
        -- sent Responses-shape. The cache holds NATIVE upstream bodies; after
        -- per-provider translation to OpenAI chat.completion shape, denormalize
        -- one more time for Responses clients. Idempotent for passthrough
        -- providers (xAI/OpenAI) where native is already output[].
        if is_responses_api then
            payload = translator.denormalize_messages_to_responses(payload)
        end
        -- Encode ONCE — we hash the same bytes the client receives, so the
        -- ledger's response_body_sha256 is the literal wire body, not a
        -- re-encoding that might differ from what was emitted.
        local wire_bytes = cjson.encode(payload)
        ngx.log(ngx.INFO, "router: cache hit alias=", record.alias,
                " provider=", cached_provider)

        local fb_alias_hit    = record.alias
        local fb_req_id_hit   = record.request_id
        local fb_sens_hit     = record.sensitivity
        local fb_role_hit     = record.council_role
        local fb_provider     = cached_provider
        local fb_caller_hit   = record.caller_key
        ledger_schedule("cache_check", fb_req_id_hit, function(premature)
            if premature then return end
            ledger_record(fb_provider, "client", {
                request_id           = fb_req_id_hit,
                response_body_sha256 = hash.body_sha256_hex(wire_bytes),
                response_size_bytes  = #wire_bytes,
            }, {
                action       = "cache_check",
                decision     = "hit",
                request_id   = fb_req_id_hit,
                provider     = fb_provider,
                sensitivity  = fb_sens_hit,
                council_role = fb_role_hit,
            }, fb_caller_hit)
        end)

        ngx.status = 200
        ngx.header["Content-Type"] = "application/json"
        ngx.header["X-Cache"] = "HIT"
        ngx.say(wire_bytes)
        return ngx.exit(200)
    end
    end -- else (non-streaming cache check)

    -- Cache miss counter (streaming requests skip cache entirely)
    if not is_streaming then
        local m = ngx.shared.gw_metrics
        if m then m:incr("cache_outcomes:miss", 1, 0) end
    end

    -- =====================================================================
    -- STEP 3: Smart routing — sidecar selects best backend.
    -- The decoded `req` table is sent to the sidecar for task classification;
    -- the gateway must not stash it on ctx after this point — the canonical
    -- request identity lives on `record` as raw_body + alias.
    -- =====================================================================
    local routing, route_err
    local resolved_name, model_cfg

    if record.requested_transport ~= "" then
        -- An authenticated Council transport request is an exact dispatch,
        -- not a routing hint. Resolve the requested alias directly so smart
        -- routing and model fallback cannot silently change the pair.
        model_cfg, resolved_name = config.resolve_model(record.alias)
    else
        routing, route_err = sidecar.route_decide(
            record.alias,
            req,
            record.routing_strategy,
            record.sensitivity,
            sovereign_mode
        )
    end

    if model_cfg then
        -- Exact transport path already resolved above.
    elseif routing then
        -- Sidecar provided a routing decision
        resolved_name = routing.model_id
        model_cfg = config.get_model(resolved_name)

        if not model_cfg then
            -- Model is in sidecar registry but not in Lua config —
            -- trust sidecar but need provider info
            ngx.log(ngx.WARN, "router: sidecar returned model '",
                    resolved_name, "' not in Lua config, falling back")
            model_cfg, resolved_name = config.resolve_model(record.alias)
        end
    else
        -- Sidecar unavailable — fallback to direct Lua routing
        ngx.log(ngx.WARN, "router: sidecar route failed (", route_err or "unknown",
                "), using direct routing")
        model_cfg, resolved_name = config.resolve_model(record.alias)
    end

    if not model_cfg then
        ngx.log(ngx.WARN, "router: unknown model: ", record.alias)

        local fb_alias_rej    = record.alias
        local fb_req_id_rej   = record.request_id
        local fb_sens_rej     = record.sensitivity
        local fb_caller_rej   = record.caller_key
        ledger_schedule("route_decide", fb_req_id_rej, function(premature)
            if premature then return end
            ledger_record("gateway", fb_alias_rej, {
                request_id = fb_req_id_rej,
            }, {
                action      = "route_decide",
                decision    = "rejected",
                request_id  = fb_req_id_rej,
                reason      = "unknown_model",
                sensitivity = fb_sens_rej,
            }, fb_caller_rej)
        end)

        return json_error(400, "unknown model: " .. record.alias, nil, "ERR_MODEL_UNKNOWN")
    end

    if record.requested_transport ~= "" then
        if sovereign_mode and not council_transport.is_local_provider(model_cfg.provider) then
            local fb_transport = record.requested_transport
            local fb_provider = model_cfg.provider
            local fb_alias = record.alias
            local fb_req_id = record.request_id
            local fb_caller = record.caller_key
            ledger_schedule("route_decide", fb_req_id, function(premature)
                if premature then return end
                ledger_record("gateway", fb_alias, { request_id = fb_req_id }, {
                    action = "route_decide",
                    decision = "rejected",
                    request_id = fb_req_id,
                    reason = "sovereign_transport_external",
                    requested_transport = fb_transport,
                    resolved_provider = fb_provider,
                }, fb_caller)
            end)
            return json_error(403,
                "requested Council transport is not local in Sovereign mode",
                "forbidden", "ERR_COUNCIL_TRANSPORT_SOVEREIGN")
        end
        local transport_ok, transport_reason = council_transport.matches(
            record.requested_transport, model_cfg.provider)
        if not transport_ok then
            local fb_transport = record.requested_transport
            local fb_provider = model_cfg.provider
            local fb_alias = record.alias
            local fb_req_id = record.request_id
            local fb_caller = record.caller_key
            ledger_schedule("route_decide", fb_req_id, function(premature)
                if premature then return end
                ledger_record("gateway", fb_alias, { request_id = fb_req_id }, {
                    action = "route_decide",
                    decision = "rejected",
                    request_id = fb_req_id,
                    reason = transport_reason,
                    requested_transport = fb_transport,
                    resolved_provider = fb_provider,
                }, fb_caller)
            end)
            return json_error(400,
                "requested Council transport '" .. record.requested_transport
                    .. "' cannot serve model '" .. record.alias .. "'",
                "invalid_request", "ERR_COUNCIL_TRANSPORT_UNAVAILABLE")
        end
    end

    -- Phase 2 streaming provider rules:
    --   openai, xai:  inject stream_options.include_usage=true (verified)
    --   nvidia:       allow streaming, do NOT inject stream_options
    --   anthropic:    inject stream=true in translated body (handled by translator)
    --   vertex:       swap path to streamGenerateContent?alt=sse (handled below)
    --   claude-cli:   explicit 501 — CLI pipe, not SSE endpoint
    --   gemini-cli:   explicit 501 — CLI pipe, not SSE endpoint
    --   local:        allow (OpenAI-compatible like Ollama/vLLM)
    if is_streaming then
        local p = model_cfg.provider
        if p == "claude-cli" or p == "gemini-cli" then
            return json_error(501,
                "streaming not supported for " .. p .. " (CLI pipe, not SSE endpoint)",
                "not_implemented", "ERR_STREAM_UNSUPPORTED")
        end
        if p == "openai" or p == "xai" then
            req.stream_options = req.stream_options or {}
            req.stream_options.include_usage = true
        end
        -- Anthropic: stream=true is set in the translated body by translate_request
        if p == "anthropic" then
            req.stream = true
        end
    end

    -- =====================================================================
    -- STEP 4: Budget — pre-flight cost check (uses record.budget_key)
    -- =====================================================================
    if record.budget_key and record.budget_key ~= "" then
        -- Estimate cost: rough calc using output pricing × 1K tokens as estimate
        local pricing = model_cfg.pricing or {}
        local estimated_cost = ((pricing.output or 0) * 1000) / 1000000  -- 1K output tokens

        local budget_result = sidecar.budget_check(record.budget_key, estimated_cost)
        if budget_result and not budget_result.allowed then
            ngx.log(ngx.WARN, "router: budget exceeded for key=", record.budget_key,
                    " reason=", budget_result.reason or "")

            local fb_bk           = record.budget_key
            local fb_alias_b      = record.alias
            local fb_req_id_b     = record.request_id
            local fb_reason_b     = budget_result.reason
            local fb_caller_b     = record.caller_key
            ledger_schedule("budget_check", fb_req_id_b, function(premature)
                if premature then return end
                ledger_record("gateway", fb_alias_b, {
                    request_id = fb_req_id_b,
                    budget_key = fb_bk,
                }, {
                    action     = "budget_check",
                    decision   = "blocked",
                    request_id = fb_req_id_b,
                    budget_key = fb_bk,
                    reason     = fb_reason_b,
                }, fb_caller_b)
            end)

            return json_error(429, "budget exceeded: " .. (budget_result.reason or "limit reached"),
                    "budget_exceeded", "ERR_BUDGET_EXCEEDED")
        end
    end

    -- =====================================================================
    -- STEP 5: Policy — sensitivity firewall
    -- =====================================================================
    if record.last_user_content then
        local policy_result = sidecar.policy_evaluate(
            model_cfg.provider, record.last_user_content, record.sensitivity)
        if policy_result and not policy_result.allowed and not policy_result.dry_run then
            ngx.log(ngx.WARN, "router: policy blocked — provider=", model_cfg.provider,
                    " level=", policy_result.level or "?",
                    " signals=", cjson.encode(policy_result.detected_signals))

            local fb_alias_p     = record.alias
            local fb_req_id_p    = record.request_id
            local fb_provider    = model_cfg.provider
            local fb_level       = policy_result.level
            local fb_reason_p    = policy_result.reason
            local fb_sens_p      = record.sensitivity
            local fb_caller_p    = record.caller_key
            ledger_schedule("policy_evaluate", fb_req_id_p, function(premature)
                if premature then return end
                ledger_record("gateway", fb_alias_p, {
                    request_id = fb_req_id_p,
                    provider   = fb_provider,
                    level      = fb_level,
                }, {
                    action      = "policy_evaluate",
                    decision    = "blocked",
                    request_id  = fb_req_id_p,
                    provider    = fb_provider,
                    sensitivity = fb_sens_p,
                    reason      = fb_reason_p,
                }, fb_caller_p)
            end)

            return json_error(403, "content sensitivity policy violation: " ..
                    (policy_result.reason or "provider not allowed at detected sensitivity level"),
                    "policy_violation", "ERR_POLICY_VIOLATION")
        end
    end

    -- =====================================================================
    -- STEP 5b: Council endpoint gate (spec §5.4, §5.6)
    --
    -- Applies only to model_cfg.provider == "council". Sequence:
    --   1. Feature flag check (GW_ENABLE_COUNCIL_ENDPOINT=1).
    --   2. Re-entry rejection: any X-Council-Depth >= 1 that survived the
    --      restore phase means an authenticated council-service caller is
    --      recursing — reject with 409 council_reentry_blocked. Hard cap at
    --      depth >= 3 surfaces a second code so an auditor can tell apart
    --      "deliberate single recursion" from "depth runaway".
    --   3. Idempotency-Key required (Stripe semantics).
    --   4. Body-hash idempotency peek/lock/claim.
    --
    -- The replay branch ngx.exit()s from access phase, mirroring the cache
    -- hit pattern at line ~895.
    -- =====================================================================
    if model_cfg.provider == "council" then
        if os.getenv("GW_ENABLE_COUNCIL_ENDPOINT") ~= "1" then
            return json_error(501,
                "Council endpoint not enabled. Set GW_ENABLE_COUNCIL_ENDPOINT=1.",
                "gateway_error", "ERR_COUNCIL_DISABLED")
        end

        -- Streaming MUST be rejected BEFORE depth/idem/lock/claim — otherwise a
        -- streaming request would acquire a concurrency slot and leak a
        -- Pending entry before bouncing at translate time. Defense-in-depth:
        -- the council translator also returns "streaming_unsupported", but
        -- that path only fires post-mutation. Spec §7 / §9 test #4.
        if is_streaming then
            return json_error(400,
                "Council models do not support streaming.",
                "invalid_request_error", "ERR_STREAMING_UNSUPPORTED")
        end

        -- Restored X-Council-Depth (if any) drives recursion / hard cap.
        -- An external (non-service-identity) caller already had the header
        -- stripped by §5.6, so this read is structurally guarded.
        local depth = tonumber(ngx.req.get_headers()["x-council-depth"]) or 0
        if depth >= 3 then
            return json_error(409, "Council depth hard cap exceeded.",
                "invalid_request_error", "ERR_COUNCIL_DEPTH_CAP")
        end
        if depth >= 1 then
            return json_error(409, "Recursive council invocation blocked.",
                "invalid_request_error", "ERR_COUNCIL_REENTRY")
        end

        local req_headers = ngx.req.get_headers()
        local idem = req_headers["idempotency-key"]
        if not idem or idem == "" then
            return json_error(400,
                "Idempotency-Key header required for council models.",
                "invalid_request_error", "ERR_IDEMPOTENCY_KEY_REQUIRED")
        end

        -- Use the caller's per-key audit identity for both concurrency and
        -- idempotency namespacing. Fall back to budget_key when no caller_key
        -- has been resolved (defensive — auth_check usually populates one).
        local caller_ns = (record.caller_key ~= "" and record.caller_key)
                          or record.budget_key or "default"
        local body_sha = hash.body_sha256_hex(record.raw_body or "")

        -- PEEK — read-only. Returns: stored | pending | conflict | miss.
        local peek, peek_err = sidecar.council_idempotency_peek(caller_ns, idem, body_sha)
        if peek_err then
            ngx.log(ngx.ERR, "council: idempotency peek failed: ", peek_err)
            return json_error(503, "council idempotency check unavailable",
                "server_error", "ERR_COUNCIL_UNAVAILABLE")
        end
        if peek and peek.conflict then
            return json_error(409,
                "Idempotency-Key reused with a different request body.",
                "invalid_request_error", "ERR_IDEMPOTENCY_CONFLICT")
        end
        if peek and peek.pending then
            return json_error(409,
                "An identical request is already in flight for this Idempotency-Key.",
                "invalid_request_error", "ERR_IDEMPOTENCY_CONFLICT")
        end
        if peek and peek.hit and peek.cached_response then
            -- REPLAY PATH (§6.3). Stash everything cost.account_replay needs
            -- and hand off to it; emits cached body synchronously and defers
            -- the ledger row via timer (cosockets prohibited in log phase).
            record.council_replay      = true
            record.council_replay_idem = idem
            local hdrs = (peek.cached_response.headers or {})
            record.council_replay_orig_session_id =
                hdrs["X-Council-Session-Id"] or hdrs["x-council-session-id"] or ""
            record.council_replay_cached = peek.cached_response
            -- P0-3: surface original_request_id + response_body_sha256 from
            -- the stored entry so account_replay can write a non-repudiation-
            -- complete council_replay ledger row. The sidecar omits these
            -- fields when an old entry (pre-P0-3) is being replayed; the
            -- ledger row will then carry empty strings and account_replay
            -- falls back to re-hashing the cached body.
            record.council_replay_orig_request_id = peek.original_request_id or ""
            record.council_replay_resp_sha        = peek.response_body_sha256 or ""
            record.provider              = "council"
            record.resolved_model        = resolved_name
            -- account_replay calls ngx.exit; control does not return.
            require("cost").account_replay(record)
            return
        end

        -- LOCK — concurrency cap. Acquired BEFORE inserting Pending so a
        -- 429 here cannot leak Pending state. FIX-1: capture the grant_id
        -- returned by the sidecar so subsequent unlocks remove the exact
        -- slot — not any one, which would race the sweeper.
        local lock_res, lock_err = sidecar.council_lock(caller_ns)
        if lock_err or not lock_res or not lock_res.granted then
            return json_error(429,
                "Max concurrent council sessions (2) reached.",
                "rate_limit_error", "ERR_COUNCIL_CONCURRENCY")
        end
        record.council_locked   = true
        record.council_grant_id = lock_res.grant_id or ""

        -- P2-B: client-disconnect unlock. Without this hook, a client that
        -- aborts mid-deliberation (browser tab close, ctrl-c, upstream
        -- timeout shedding) kills the request handler before log_by_lua
        -- runs — the matching sidecar.council_unlock in cost.lua never
        -- fires and the slot leaks until the sidecar's ~110s TTL sweeper
        -- reclaims it. Under bursty cancellation this 429s the caller for
        -- ~2 min for no reason.
        --
        -- Snapshot caller_ns + grant_id as locals BEFORE registering — do
        -- NOT read record.council_grant_id inside the handler. Record
        -- state can evolve between registration and abort-fire (e.g. the
        -- claim-conflict path below resets council_locked synchronously).
        --
        -- on_abort's handler runs in a fake-request context where cosocket
        -- I/O IS permitted (that's the API's whole point) — call the
        -- module-pinned sidecar_council_unlock directly, no timer hop.
        --
        -- Requires `lua_check_client_abort on;` in the /v1/ location
        -- block; without it ngx.on_abort returns nil + an error. Log and
        -- continue — the log-phase unlock in cost.lua is still the
        -- fallback for the non-aborted (normal) completion path.
        local _abort_ns  = caller_ns
        local _abort_gid = record.council_grant_id
        local _abort_rec = record
        local ok_abort, abort_err = ngx.on_abort(function()
            -- Bail if a synchronous path already released the slot (e.g.
            -- the claim-conflict branch a few lines below). Sidecar
            -- treats duplicate per-grant unlock as a benign no-op, but
            -- we'd rather avoid the wasted UDS round-trip.
            if not _abort_rec.council_locked then return end
            -- Set flags FIRST — if log_by_lua races in, it must see
            -- council_aborted (and council_locked=false) before we
            -- start the cosocket call.
            _abort_rec.council_aborted = true
            _abort_rec.council_locked  = false
            local _, unlock_err = sidecar_council_unlock(_abort_ns, _abort_gid)
            if unlock_err then
                ngx.log(ngx.ERR, "council_unlock on client abort failed: ", unlock_err)
            end
            -- NOTE: the Pending idempotency entry is NOT cleared here.
            -- That requires a Store-or-Fail decision that depends on
            -- response state we don't have at abort time, so the entry
            -- still leaks until the sidecar sweeper reclaims it. This
            -- fix targets the concurrency-slot leak only (P2-B scope);
            -- idem leak is a separate task.
        end)
        if not ok_abort then
            ngx.log(ngx.WARN,
                "ngx.on_abort registration failed (council slot will rely on log-phase + sweeper fallback): ",
                abort_err or "unknown")
        end

        -- CLAIM — owner-aware Pending insert. TOCTOU vs. peek is bounded:
        -- claim returns conflict=true if a concurrent claimer won.
        local claim, claim_err = sidecar.council_idempotency_claim(
            caller_ns, idem, body_sha, record.request_id)
        if claim_err or (claim and claim.conflict) then
            local _, unlock_err = sidecar.council_unlock(
                caller_ns, record.council_grant_id)
            if unlock_err then
                ngx.log(ngx.WARN, "council: unlock after claim-conflict failed: ", unlock_err)
            end
            record.council_locked = false
            return json_error(409,
                "Concurrent claim won for this Idempotency-Key.",
                "invalid_request_error", "ERR_IDEMPOTENCY_CONFLICT")
        end

        record.council_idem_key = idem
        record.council_body_sha = body_sha
        record.council_caller_ns = caller_ns

        -- Stamp the upstream call to council-rs with the gateway's own
        -- request_id as the parent. Council-rs picks this up in
        -- /api/deliberate (§6.5) and threads it through RequestContext to
        -- every seat call. Those seat calls then come back through the
        -- gateway with X-Parent-Request-Id set, triggering both the
        -- parent_council_request_id linkage AND the council-internal decon
        -- bypass — which keeps the chair synthesis transcript (persona-heavy,
        -- can include adversarial seat output) from tripping the input
        -- guard on the way back through.
        ngx.req.set_header("X-Council-Request-ID", record.request_id)
        ngx.req.set_header("X-Parent-Request-Id", record.request_id)
        ngx.req.set_header("X-Council-Depth", "0")
    end

    -- =====================================================================
    -- STEP 6: Resolve provider + translate body (Rosetta layer)
    -- =====================================================================
    local provider = config.get_provider(model_cfg)
    if not provider then
        ngx.log(ngx.ERR, "router: no provider config for: ", model_cfg.provider)
        return json_error(502, "provider not configured", "gateway_error", "ERR_PROVIDER_MISSING")
    end

    -- Get API key. Vertex normally uses an ADC OAuth token from the sidecar,
    -- so defer its missing-static-key error until after that token attempt.
    local api_key = config.get_api_key(model_cfg.provider)
    if api_key == "" and model_cfg.provider ~= "vertex" then
        ngx.log(ngx.ERR, "router: no API key for provider: ", model_cfg.provider)
        return json_error(502, "API key not configured for provider", "gateway_error", "ERR_PROVIDER_KEY_MISSING")
    end

    -- Build target URL. Keep the catalog/cost identity stable while allowing
    -- an operator's Vertex deployment to use its provider-specific wire ID.
    -- This override is deliberately limited to the canonical Gemini Pro seat;
    -- utility models retain their independently configured IDs.
    local path = model_cfg.path or "/v1/chat/completions"
    local upstream_model = resolved_name

    -- Vertex uses path templates with project/location/model substitution
    if model_cfg.provider == "vertex" then
        if resolved_name == "gemini-3.1-pro-preview" then
            local configured_model = os.getenv("VERTEX_GEMINI_MODEL") or ""
            if configured_model ~= ""
                and configured_model ~= "your-model-id"
                and configured_model ~= "change-me"
                and configured_model ~= "changeme" then
                upstream_model = configured_model
            end
        end
        path = translator.resolve_vertex_path(path, upstream_model)
        -- Streaming: swap generateContent → streamGenerateContent?alt=sse
        if is_streaming then
            path = path:gsub(":generateContent$", ":streamGenerateContent")
            if not path:find("?", 1, true) then
                path = path .. "?alt=sse"
            else
                path = path .. "&alt=sse"
            end
        end
    end

    local target = provider.base_url .. path

    -- Extract host from base_url for Host header
    local host = provider.base_url:match("https?://([^/]+)")

    -- =====================================================================
    -- TRANSLATE: Convert request body to provider-native format
    -- All providers go through the translator — the bridge handles
    -- messages↔input conversion for OpenAI/xAI, and full body rewrite
    -- for Anthropic/Vertex.
    -- =====================================================================
    req.model = upstream_model
    translator.set_target_path(path)  -- tells bridge which format to emit
    translator.set_budget_key(record.budget_key)

    -- Phase 5: Responses API request normalization.
    -- If the client sent Responses shape (input[] + optionally instructions +
    -- max_output_tokens), convert to canonical chat-completions shape FIRST
    -- so per-provider translators (anthropic, vertex, nvidia) see a uniform
    -- input. The openai_bridge will re-convert to input[] for upstreams whose
    -- path is /v1/responses (xAI, OpenAI). instructions becomes a synthetic
    -- {role:"system", content: instructions} prepended to messages — preserved
    -- through messages→input conversion as a system-role item.
    if record.is_responses_api then
        translator.normalize_responses_to_messages(req)
    end

    -- SpecOps: Gate council_auto_escalate by tenant policy. In v0.1.0 this is simply
    -- requiring an authenticated caller (caller_key or budget_key). Forward-compatible
    -- with v0.2.0 capability tokens.
    if req.council_auto_escalate then
        if (not record.caller_key or record.caller_key == "") and (not record.budget_key or record.budget_key == "") then
            req.council_auto_escalate = nil
        end
    end

    local translated_body, translate_err, extra_headers
    translated_body, translate_err, extra_headers = translator.translate_request(
        model_cfg.provider, req, upstream_model
    )
    if translate_err then
        -- Council translator surfaces structured error codes for the cases
        -- the gateway wants to expose as 400s (per spec §7). Other
        -- translators today only return generic strings — those still map
        -- to 500 ERR_TRANSLATE_FAILED.
        if translate_err == "streaming_unsupported" then
            return json_error(400,
                "Council models do not support streaming.",
                "invalid_request_error", "ERR_STREAMING_UNSUPPORTED")
        end
        if translate_err == "content_depth_exceeded" then
            return json_error(400,
                "Multimodal content nesting exceeded depth cap.",
                "invalid_request_error", "ERR_CONTENT_DEPTH_EXCEEDED")
        end
        ngx.log(ngx.ERR, "router: translation failed for ", model_cfg.provider,
                ": ", translate_err)
        return json_error(500, "body translation failed: " .. translate_err,
                "gateway_error", "ERR_TRANSLATE_FAILED")
    end
    ngx.req.set_body_data(cjson.encode(translated_body))

    -- Set nginx variables for proxy_pass
    ngx.var.target_url  = target
    ngx.var.target_host = host or ""

    -- Strip client-supplied auth headers before setting provider auth
    -- Prevents client API keys from leaking to upstream providers
    ngx.req.clear_header("x-api-key")
    ngx.req.clear_header("anthropic-version")

    -- Set auth (provider-specific header handling).
    --
    -- Spec §5.2a (P0 #2 regression guard): generalize the branching so a
    -- provider that uses neither `Authorization` nor `x-api-key`
    -- (e.g. council's `X-Gateway-Auth`) still authenticates correctly.
    -- The first two branches are unchanged from pre-§5.2a behavior; the
    -- else-branch is the new path. Anthropic + xAI smoke regression is
    -- covered by spec §9 test #18.
    local auth_header_name = provider.auth_header or "Authorization"
    local auth_value = (provider.auth_prefix or "Bearer ") .. api_key

    if auth_header_name == "x-api-key" then
        -- Anthropic uses x-api-key instead of Authorization
        ngx.req.set_header("x-api-key", api_key)
        ngx.var.auth_value = ""  -- clear Authorization
    elseif auth_header_name == "Authorization" then
        ngx.var.auth_value = auth_value
    else
        -- Custom auth header (e.g. council's X-Gateway-Auth). Use the
        -- provider's auth_prefix verbatim — "" yields the raw token, which
        -- is what X-Gateway-Auth expects.
        ngx.req.set_header(auth_header_name, (provider.auth_prefix or "") .. api_key)
        ngx.var.auth_value = ""
    end

    -- Set provider-specific extra headers (e.g. anthropic-version)
    if extra_headers then
        for k, v in pairs(extra_headers) do
            ngx.req.set_header(k, v)
        end
    end
    if provider.extra_headers then
        for k, v in pairs(provider.extra_headers) do
            ngx.req.set_header(k, v)
        end
    end

    -- P0-1: inject the host-side CLI proxies' shared-secret bearer when
    -- present. CLAUDE_PROXY_TOKEN / CODEX_PROXY_TOKEN are set in the gateway
    -- container env; the matching token is set in the proxy process env on
    -- the host. When unset (loopback-only proxy), the proxy disables auth
    -- and we skip the header. The proxy enforces with constant-time compare.
    if model_cfg.provider == "claude-cli" then
        local tok = os.getenv("CLAUDE_PROXY_TOKEN")
        if tok and tok ~= "" then
            ngx.req.set_header("X-Proxy-Auth", "Bearer " .. tok)
        end
    elseif model_cfg.provider == "gpt-cli" then
        local tok = os.getenv("CODEX_PROXY_TOKEN")
        if tok and tok ~= "" then
            ngx.req.set_header("X-Proxy-Auth", "Bearer " .. tok)
        end
    elseif model_cfg.provider == "gemini-cli" then
        local tok = os.getenv("GEMINI_PROXY_TOKEN")
        if tok and tok ~= "" then
            ngx.req.set_header("X-Proxy-Auth", "Bearer " .. tok)
        end
    end

    -- For Vertex, try to fetch a fresh ADC token from the sidecar.
    -- This handles the 1-hour expiration. If it fails, fall back to the
    -- static VERTEX_ADC_TOKEN or whatever was set as `api_key` in config.
    if model_cfg.provider == "vertex" then
        local token_resp, err = sidecar.vertex_token()
        if token_resp and token_resp.token then
            ngx.var.auth_value = "Bearer " .. token_resp.token
            ngx.log(ngx.DEBUG, "router: using fresh Vertex ADC token (source: ",
                    token_resp.source, ")")
        else
            ngx.log(ngx.WARN, "router: failed to fetch fresh Vertex token (",
                    err, ") — falling back to static API key config")
            if api_key == "" then
                ngx.log(ngx.ERR, "router: no Vertex ADC token or static API key configured")
                return json_error(502, "API key not configured for provider",
                        "gateway_error", "ERR_PROVIDER_KEY_MISSING")
            end
        end
    end

    -- =====================================================================
    -- Append derived fields to the canonical record. We do NOT replace
    -- ngx.ctx.gw — that would drop the frozen ingress state. Downstream
    -- phases (body_filter, log) read everything off `record`.
    --
    -- Critically, we do NOT stash the decoded `req` table on the record:
    -- the translator above mutated it in place to provider-native shape,
    -- and any later cache_store / cost-extraction must hash or parse the
    -- frozen `record.raw_body` (the client-canonical bytes) instead.
    -- =====================================================================
    record.resolved_model              = resolved_name
    record.provider                    = model_cfg.provider
    record.pricing                     = model_cfg.pricing or {}
    record.requested_model             = routing and routing.requested_model or record.alias
    record.effective_model             = routing and routing.effective_model or resolved_name
    record.routing_score               = routing and routing.score or nil
    record.routing_strategy_resolved   = routing and routing.strategy or nil
    record.needs_response_translation  = translator.needs_response_translation(model_cfg.provider)

    -- For streaming, determine which provider's translator to use for SSE
    -- chunk translation. Normally this is the provider itself (anthropic, vertex).
    -- Chaos test models override via _stream_translate so we can test the
    -- translation pipeline with the mock server.
    record.stream_translate_as         = model_cfg._stream_translate or model_cfg.provider
    record.upstream_path               = model_cfg.path or "/v1/chat/completions"

    -- Default the budget key for ledger metadata if the caller didn't supply one.
    if record.budget_key == "" then
        record.budget_key = "default"
    end

    -- Set X-Routed-Model and X-Routed-Provider response headers for
    -- downstream observability and fallback telemetry.
    ngx.header["X-Routed-Model"]    = resolved_name
    ngx.header["X-Routed-Provider"] = model_cfg.provider
    if routing and routing.requested_model and routing.requested_model ~= resolved_name then
        ngx.header["X-Routed-Fallback"] = "true"
    end
    if sovereign_mode then
        ngx.header["X-Sovereign-Mode"] = "active"
    end

    ngx.log(ngx.INFO, "router: ", record.alias, " → ", resolved_name,
            " → ", model_cfg.provider, " @ ", target,
            routing and (" [" .. (routing.strategy or "?") .. " score=" ..
            string.format("%.3f", routing.score or 0) .. "]") or " [direct]",
            record.needs_response_translation and " [translated]" or "",
            sovereign_mode and " [SOVEREIGN]" or "")
end

--- Authenticated OpenAI-compatible model catalog used by governed Council
--- preflight. Readiness may make bounded, non-generating credential/catalog
--- probes, but never reserves budget or dispatches a paid completion.
function _M.models()
    if ngx.req.get_method() ~= "GET" then
        return json_error(405, "Method not allowed", "invalid_request", "ERR_METHOD_NOT_ALLOWED")
    end

    local ok = authenticate_request(ngx.req.get_headers())
    if not ok then return end

    local readiness_cache = {}
    local function advertised_model_set(body)
        local decoded_ok, decoded = pcall(cjson.decode, body or "")
        if not decoded_ok or type(decoded) ~= "table" then
            return nil
        end
        -- OpenAI-compatible providers commonly return `data`; xAI's catalog
        -- currently returns the same model rows under `models`.
        local rows = type(decoded.data) == "table" and decoded.data or decoded.models
        if type(rows) ~= "table" then return nil end
        local models = {}
        for _, row in ipairs(rows) do
            if type(row) == "table" and type(row.id) == "string" then
                models[row.id] = true
            end
        end
        return next(models) and models or nil
    end

    local function provider_readiness(provider_name, model_id)
        provider_name = provider_name or ""
        if readiness_cache[provider_name] then
            local cached = readiness_cache[provider_name]
            if cached.models and cached.ready and not cached.models[model_id] then
                return false, "model_unsupported"
            end
            return cached.ready, cached.reason
        end

        local ready, reason, advertised_models = false, "credentials_unavailable", nil
        if provider_name == "xai"
            or provider_name == "openai"
            or provider_name == "nvidia" then
            local provider_cfg = config.providers[provider_name] or {}
            local base_url = provider_cfg.base_url or ""
            local api_key = config.get_api_key(provider_name)
            if base_url ~= "" and api_key ~= "" then
                local auth_header = provider_cfg.auth_header or "Authorization"
                local auth_value = (provider_cfg.auth_prefix or "Bearer ") .. api_key
                local httpc = http.new()
                -- Catalog endpoints are non-generating but can be materially
                -- slower than completion front doors. Do not mark a valid key
                -- unready merely because provider discovery took a few seconds.
                httpc:set_timeout(15000)
                local res, probe_err = httpc:request_uri(base_url .. "/v1/models", {
                    method = "GET",
                    keepalive = false,
                    headers = { [auth_header] = auth_value },
                })
                ready = res ~= nil and res.status >= 200 and res.status < 300
                if ready then
                    advertised_models = advertised_model_set(res.body)
                    ready = advertised_models ~= nil
                end
                if res == nil then
                    reason = "credential_probe_unreachable"
                    ngx.log(ngx.WARN, "router: provider catalog probe failed for ",
                        provider_name, ": ", probe_err or "unknown transport error")
                elseif res.status == 401 or res.status == 403 then
                    reason = "credentials_rejected"
                elseif not ready then
                    reason = "credential_probe_invalid"
                else
                    reason = "credentials_verified"
                end
            end
        elseif provider_name == "vertex" then
            local project = os.getenv("VERTEX_PROJECT") or ""
            local project_ready = project ~= ""
                and project ~= "your-gcp-project"
                and project ~= "your-project-id"
            local token_resp = sidecar.vertex_token()
            ready = project_ready
                and token_resp ~= nil and token_resp.token ~= nil and token_resp.token ~= ""
            reason = ready and "adc_ready"
                or (project_ready and "credentials_unavailable" or "project_unavailable")
        elseif provider_name == "claude-cli"
            or provider_name == "gpt-cli"
            or provider_name == "gemini-cli" then
            local provider_cfg = config.providers[provider_name] or {}
            local base_url = provider_cfg.base_url or ""
            local token_env = provider_name == "claude-cli" and "CLAUDE_PROXY_TOKEN"
                or provider_name == "gpt-cli" and "CODEX_PROXY_TOKEN"
                or "GEMINI_PROXY_TOKEN"
            local proxy_token = os.getenv(token_env) or ""
            if base_url ~= "" and proxy_token ~= "" then
                local httpc = http.new()
                httpc:set_timeout(750)
                local res = httpc:request_uri(base_url .. "/v1/models", {
                    method = "GET",
                    keepalive = false,
                    headers = { ["X-Proxy-Auth"] = "Bearer " .. proxy_token },
                })
                ready = res ~= nil and res.status == 200
                if ready then
                    advertised_models = advertised_model_set(res.body)
                    ready = advertised_models ~= nil
                end
            end
            if proxy_token == "" then
                reason = "proxy_auth_unavailable"
            elseif not ready then
                reason = "proxy_unreachable_or_invalid"
            else
                reason = "proxy_ready"
            end
        elseif provider_name == "local" then
            local provider_cfg = config.providers[provider_name] or {}
            local base_url = provider_cfg.base_url or ""
            if base_url ~= "" then
                local httpc = http.new()
                httpc:set_timeout(750)
                local res = httpc:request_uri(base_url .. "/v1/models", {
                    method = "GET",
                    keepalive = false,
                })
                ready = res ~= nil and res.status >= 200 and res.status < 300
            end
            reason = ready and "local_ready" or "local_unreachable"
        elseif provider_name == "chaos" then
            ready, reason = true, "test_provider"
        else
            ready = config.get_api_key(provider_name) ~= ""
            reason = ready and "credentials_configured" or "credentials_unavailable"
        end

        local provider_ready = ready or advertised_models ~= nil
        readiness_cache[provider_name] = {
            ready = provider_ready,
            reason = reason,
            models = advertised_models,
        }
        if advertised_models and provider_ready and not advertised_models[model_id] then
            return false, "model_unsupported"
        end
        return provider_ready, reason
    end

    local data = {}
    local seen = {}
    for model_id, model_cfg in pairs(config.models) do
        local ready, readiness = provider_readiness(model_cfg.provider, model_id)
        table.insert(data, {
            id = model_id,
            object = "model",
            owned_by = model_cfg.provider or "gateway",
            transports = council_transport.advertised_for_provider(model_cfg.provider),
            ready = ready,
            readiness = readiness,
        })
        seen[model_id] = true
    end
    -- Aliases are valid request model IDs too. Advertising them keeps Council
    -- preflight aligned with config.resolve_model() rather than rejecting an
    -- identifier the paid route would accept.
    for alias, resolved in pairs(config.aliases) do
        if not seen[alias] then
            local model_cfg = config.models[resolved] or {}
            local ready, readiness = provider_readiness(model_cfg.provider, resolved)
            table.insert(data, {
                id = alias,
                object = "model",
                owned_by = model_cfg.provider or "gateway-alias",
                transports = council_transport.advertised_for_provider(model_cfg.provider),
                ready = ready,
                readiness = readiness,
            })
        end
    end
    table.sort(data, function(a, b) return a.id < b.id end)

    ngx.status = 200
    ngx.header["Content-Type"] = "application/json"
    ngx.say(cjson.encode({ object = "list", data = data }))
    return ngx.exit(200)
end

return _M
