-- ==========================================================================
-- cost.lua — Token extraction + cost accounting.
--
-- Two phases:
--   body_filter: capture upstream response chunks. For translated providers
--                we keep BOTH the native upstream body (for cost extraction
--                with the provider-specific parser) AND the normalized
--                OpenAI-shape body (what we emit to the client and cache).
--   log:         parse tokens from the native body, compute cost, emit
--                metrics, push feedback to the sidecar (cache, ledger, budget).
--
-- All request facts come off `ngx.ctx.gw.record` — the canonical RequestRecord
-- frozen at access entry. The `req` table the translator mutated is NEVER
-- read here; cost extraction runs against `record.raw_body` (request) and
-- `gw_response_buf_native` (response) so that translation can never poison
-- accounting.
-- ==========================================================================

local cjson            = require "cjson.safe"
local providers        = require "lib.providers"
local translator       = require "translator"
local sidecar          = require "sidecar"
local hash             = require "lib.hash"
local ledger           = require "lib.ledger"
local credential_scrub = require "lib.credential_scrub"
local responses_stream = require "lib.responses_stream"

-- Module-level function bindings for timer closures. Capturing
-- `sidecar.route_outcome` etc. INSIDE a closure would resolve the table
-- field at call time — meaning a hot-reload that swaps the sidecar module
-- table (or a test that monkey-patches it) would silently route timer
-- traffic to the new implementation, possibly mid-request. Binding the
-- function references HERE pins them at module load. The closures hold
-- function pointers, not table references.
--
-- CI lint policy: `sidecar\.` should never appear inside an
-- `ngx.timer.at` body (enforced by test/lint-timer-closures.sh). Use the
-- module-level locals below or the lib.ledger helper.
local sidecar_route_outcome = sidecar.route_outcome
local sidecar_budget_record = sidecar.budget_record
local sidecar_cache_store   = sidecar.cache_store
-- Council cleanup (spec §5.5). Bound at module load — the timer closures
-- below MUST NOT call `sidecar.council_*(...)` at run time (timer-closure
-- lint at test/lint-timer-closures.sh enforces no `sidecar.` references
-- inside ngx.timer.at bodies; hot-reload would otherwise silently divert
-- in-flight cleanup writes between the access and log phases).
local sidecar_council_unlock     = sidecar.council_unlock
local sidecar_council_idem_store = sidecar.council_idempotency_store
local sidecar_council_idem_fail  = sidecar.council_idempotency_fail
local sidecar_council_stats      = sidecar.council_stats
local sidecar_watch_stats        = sidecar.watch_stats
local ledger_record         = ledger.record_with_retry
local ledger_schedule       = ledger.schedule

local _M = {}

-- Cap response body capture at 1MB to prevent OOM
local MAX_RESPONSE_CAPTURE = 1048576  -- 1MB

function _M._extract_sse_usage(text)
    for line in text:gmatch("data:%s*([^\r\n]+)") do
        local trimmed = line:gsub("%s+$", "")
        if trimmed ~= "[DONE]" then
            local data = cjson.decode(trimmed)
            if data then
                local u = nil
                if type(data.usage) == "table" then
                    u = data.usage
                elseif data.response and type(data.response.usage) == "table" then
                    u = data.response.usage
                end
                if u and ((u.prompt_tokens or u.input_tokens or 0) > 0
                       or (u.completion_tokens or u.output_tokens or 0) > 0
                       or (u.total_tokens or 0) > 0) then
                    ngx.ctx.gw_streaming_usage = u
                end
            end
        end
    end
end

-- Helper: extract JSON from a translated SSE line ("data: {...}\n\n") and
-- feed it to the Responses stream wrapper. Returns wrapped event lines or "".
local function self_wrap_sse_line(wrap_ctx, sse_line)
    if not wrap_ctx or not sse_line then return "" end
    -- Skip [DONE] sentinel — wrapper emits response.completed at finish()
    if sse_line:find("[DONE]", 1, true) then return "" end
    -- Extract JSON payload after "data: "
    local json_str = sse_line:match("data:%s*(.-)%s*$")
    if not json_str or json_str == "" then return "" end
    local data = cjson.decode(json_str)
    if not data then return "" end
    return wrap_ctx:feed(data) or ""
end

-- ---------------------------------------------------------------------------
-- BODY FILTER PHASE — accumulate chunks into a NATIVE buffer; if the
-- provider needs response translation, also produce a NORMALIZED buffer
-- and emit that to the client at EOF.
--
-- Why two buffers: providers report tokens in their native shape (Anthropic
-- uses `usage.input_tokens`/`output_tokens`; Vertex uses `usageMetadata.*`;
-- OpenAI uses `usage.prompt_tokens`/`completion_tokens`). If we ran cost
-- extraction over the already-normalized body, the native fields would be
-- gone and accounting would silently report $0 — exactly the Anthropic bug
-- this refactor is fixing.
--
-- SSE STREAMING PATH (Phase 1): For streaming responses, chunks are
-- forwarded to the client immediately AND accumulated for usage extraction
-- at EOF. The final SSE data event carries `usage` (injected by
-- stream_options.include_usage=true in router.lua). At EOF we scan the
-- accumulated body for the last usage object.
-- ---------------------------------------------------------------------------
function _M.capture_body()
    -- Skip if we already exceeded the cap
    if ngx.ctx.gw_response_capped then return end

    local chunk = ngx.arg[1]
    local eof   = ngx.arg[2]  -- true when this is the last chunk

    local gw     = ngx.ctx.gw
    local record = gw and gw.record or nil

    -- SSE streaming: forward every chunk to client immediately.
    -- For passthrough providers (OpenAI/xAI/NVIDIA), chunks flow unmodified.
    -- For translated providers (Anthropic/Vertex), each chunk is parsed into
    -- SSE frames and re-emitted as OpenAI-shaped SSE via the translator.
    -- Native upstream bytes are accumulated for the ledger hash regardless.
    --
    -- The sse.lua parser replaces the ad-hoc sse_line_buf accumulation from
    -- Phase 1, handling named events, multi-line data, CRLF, and comments.
    if record and record.is_streaming then
        local chunks = ngx.ctx.gw_response_chunks
        if not chunks then
            chunks = {}
            ngx.ctx.gw_response_chunks = chunks
            ngx.ctx.gw_response_len    = 0
        end

        local stream_provider = record.stream_translate_as or record.provider
        local needs_translation = translator.needs_stream_translation(stream_provider)

        -- Responses API wrapping: when client sent Responses shape and upstream
        -- emits chat.completion.chunk (not native /v1/responses), wrap output
        -- into response.* events via lib/responses_stream.
        local upstream_path = record.upstream_path or "/v1/chat/completions"
        local responses_native = upstream_path:sub(-10) == "/responses"
        local needs_responses_wrap = record.is_responses_api and not responses_native

        if chunk and chunk ~= "" then
            local new_len = ngx.ctx.gw_response_len + #chunk
            if new_len > MAX_RESPONSE_CAPTURE then
                if not ngx.ctx.gw_response_capped then
                    ngx.ctx.gw_response_capped = true
                    ngx.log(ngx.WARN, "cost: streaming response exceeded 1MB cap, "
                            .. "usage extraction disabled")
                end
            else
                chunks[#chunks + 1]     = chunk
                ngx.ctx.gw_response_len = new_len
            end

            if not ngx.ctx.gw_response_capped then
                -- Initialize SSE parser on first chunk
                if not ngx.ctx.sse_parser then
                    local sse = require("lib.sse")
                    ngx.ctx.sse_parser = sse.new({ strict = true })
                end

                -- Initialize Responses wrapper on first chunk (if needed)
                if needs_responses_wrap and not ngx.ctx.responses_wrap_ctx then
                    ngx.ctx.responses_wrap_ctx = responses_stream.new({
                        model       = record.resolved_model,
                        response_id = "resp_" .. (record.request_id or tostring(ngx.now())),
                    })
                end

                if needs_translation then
                    -- Provider-aware path: parse SSE frames, translate to OpenAI shape
                    if not ngx.ctx.stream_translate_ctx then
                        ngx.ctx.stream_translate_ctx = {
                            model = record.resolved_model,
                        }
                    end

                    local frames = ngx.ctx.sse_parser:feed(chunk)
                    local out_lines = {}

                    for _, frame in ipairs(frames) do
                        local sse_line, usage = translator.translate_stream_chunk(
                            stream_provider, frame, ngx.ctx.stream_translate_ctx
                        )
                        if usage then
                            ngx.ctx.gw_streaming_usage = {
                                prompt_tokens     = usage.input_tokens or 0,
                                completion_tokens = usage.output_tokens or 0,
                                total_tokens      = (usage.input_tokens or 0) + (usage.output_tokens or 0),
                            }
                        end
                        if sse_line then
                            if needs_responses_wrap then
                                -- Wrap: parse the chat.completion.chunk and feed to wrapper
                                local wrapped = self_wrap_sse_line(ngx.ctx.responses_wrap_ctx, sse_line)
                                if wrapped and wrapped ~= "" then
                                    out_lines[#out_lines + 1] = wrapped
                                end
                            else
                                out_lines[#out_lines + 1] = sse_line
                            end
                        end
                    end

                    if #out_lines > 0 then
                        ngx.arg[1] = table.concat(out_lines)
                    else
                        ngx.arg[1] = nil
                    end
                else
                    -- Passthrough path: OpenAI/xAI/NVIDIA
                    local frames = ngx.ctx.sse_parser:feed(chunk)

                    if needs_responses_wrap then
                        -- Parse each frame and wrap into Responses events
                        local out_lines = {}
                        for _, frame in ipairs(frames) do
                            if frame.done then
                                -- [DONE] sentinel — skip, we emit response.completed at EOF
                            elseif frame.data then
                                local data = cjson.decode(frame.data)
                                if data then
                                    local u = nil
                                    if type(data.usage) == "table" then
                                        u = data.usage
                                    elseif data.response and type(data.response.usage) == "table" then
                                        u = data.response.usage
                                    end
                                    if u and ((u.prompt_tokens or u.input_tokens or 0) > 0
                                           or (u.completion_tokens or u.output_tokens or 0) > 0
                                           or (u.total_tokens or 0) > 0) then
                                        ngx.ctx.gw_streaming_usage = u
                                    end
                                    -- Feed to wrapper (skip usage-only chunks with no choices)
                                    local wrapped = ngx.ctx.responses_wrap_ctx:feed(data)
                                    if wrapped and wrapped ~= "" then
                                        out_lines[#out_lines + 1] = wrapped
                                    end
                                end
                            end
                        end
                        if #out_lines > 0 then
                            ngx.arg[1] = table.concat(out_lines)
                        else
                            ngx.arg[1] = nil
                        end
                    else
                        -- Original passthrough: parse for usage only, chunk unchanged
                        for _, frame in ipairs(frames) do
                            if not frame.done and frame.data then
                                local data = cjson.decode(frame.data)
                                if data then
                                    local u = nil
                                    if type(data.usage) == "table" then
                                        u = data.usage
                                    elseif data.response and type(data.response.usage) == "table" then
                                        u = data.response.usage
                                    end
                                    if u and ((u.prompt_tokens or u.input_tokens or 0) > 0
                                           or (u.completion_tokens or u.output_tokens or 0) > 0
                                           or (u.total_tokens or 0) > 0) then
                                        ngx.ctx.gw_streaming_usage = u
                                    end
                                end
                            end
                        end
                    end
                end
            end
        end

        if eof then
            -- Flush any remaining buffered data in the parser
            if ngx.ctx.sse_parser and not ngx.ctx.gw_response_capped then
                local frames = ngx.ctx.sse_parser:flush()
                if needs_translation and ngx.ctx.stream_translate_ctx then
                    local out_lines = {}
                    for _, frame in ipairs(frames) do
                        local sse_line, usage = translator.translate_stream_chunk(
                            stream_provider, frame, ngx.ctx.stream_translate_ctx
                        )
                        if usage then
                            ngx.ctx.gw_streaming_usage = {
                                prompt_tokens     = usage.input_tokens or 0,
                                completion_tokens = usage.output_tokens or 0,
                                total_tokens      = (usage.input_tokens or 0) + (usage.output_tokens or 0),
                            }
                        end
                        if sse_line then
                            if needs_responses_wrap then
                                local wrapped = self_wrap_sse_line(ngx.ctx.responses_wrap_ctx, sse_line)
                                if wrapped and wrapped ~= "" then
                                    out_lines[#out_lines + 1] = wrapped
                                end
                            else
                                out_lines[#out_lines + 1] = sse_line
                            end
                        end
                    end
                    if #out_lines > 0 then
                        local existing = ngx.arg[1] or ""
                        ngx.arg[1] = existing .. table.concat(out_lines)
                    end
                elseif needs_responses_wrap then
                    local out_lines = {}
                    for _, frame in ipairs(frames) do
                        if not frame.done and frame.data then
                            local data = cjson.decode(frame.data)
                            if data then
                                local u = data.usage or (data.response and data.response.usage)
                                if u and ((u.prompt_tokens or u.input_tokens or 0) > 0
                                       or (u.completion_tokens or u.output_tokens or 0) > 0) then
                                    ngx.ctx.gw_streaming_usage = u
                                end
                                local wrapped = ngx.ctx.responses_wrap_ctx:feed(data)
                                if wrapped and wrapped ~= "" then
                                    out_lines[#out_lines + 1] = wrapped
                                end
                            end
                        end
                    end
                    if #out_lines > 0 then
                        local existing = ngx.arg[1] or ""
                        ngx.arg[1] = existing .. table.concat(out_lines)
                    end
                else
                    for _, frame in ipairs(frames) do
                        if not frame.done and frame.data then
                            local data = cjson.decode(frame.data)
                            if data then
                                local u = data.usage or (data.response and data.response.usage)
                                if u and ((u.prompt_tokens or u.input_tokens or 0) > 0
                                       or (u.completion_tokens or u.output_tokens or 0) > 0) then
                                    ngx.ctx.gw_streaming_usage = u
                                end
                            end
                        end
                    end
                end
            end

            -- Emit response.completed for Responses-wrapped streams
            if needs_responses_wrap and ngx.ctx.responses_wrap_ctx then
                local final_lines = ngx.ctx.responses_wrap_ctx:finish(ngx.ctx.gw_streaming_usage)
                if final_lines and final_lines ~= "" then
                    local existing = ngx.arg[1] or ""
                    ngx.arg[1] = existing .. final_lines
                end
                ngx.ctx.responses_wrap_ctx = nil
            end

            local full_sse = table.concat(chunks)
            -- Streaming credential scrub: we cannot unsend chunks that already went
            -- to the client, but we MUST scrub the ledger hash buffer and flag the
            -- request so we don't cache leaked credentials or store them in audit logs.
            local scrub_res = credential_scrub.scrub(full_sse)
            if scrub_res.redactions > 0 then
                ngx.ctx.gw_credentials_redacted = true
                ngx.ctx.gw_credentials_matched = scrub_res.matched
                ngx.log(ngx.WARN, "cost: streaming response leaked credentials, redacting from ledger/cache (",
                        scrub_res.redactions, " matches)")
                full_sse = scrub_res.scrubbed_text
            end

            ngx.ctx.gw_response_buf_native     = full_sse
            ngx.ctx.gw_response_buf_normalized = full_sse
            ngx.ctx.gw_response_chunks         = nil
            ngx.ctx.sse_parser                 = nil
            ngx.ctx.stream_translate_ctx       = nil
        end
        return
    end

    -- Non-streaming path: accumulate chunks in an array for O(N) concat at EOF.
    local chunks = ngx.ctx.gw_response_chunks
    if not chunks then
        chunks = {}
        ngx.ctx.gw_response_chunks = chunks
        ngx.ctx.gw_response_len    = 0
    end

    if chunk and chunk ~= "" then
        local new_len = ngx.ctx.gw_response_len + #chunk
        if new_len > MAX_RESPONSE_CAPTURE then
            ngx.ctx.gw_response_capped = true
            ngx.log(ngx.WARN, "cost: response body exceeded 1MB cap, token tracking disabled")
            return
        end
        chunks[#chunks + 1]        = chunk
        ngx.ctx.gw_response_len    = new_len
    end

    if not record then return end

    -- Phase 5: Responses API clients expect output[] shape on the way back.
    -- For passthrough providers whose native shape is already output[]
    -- (xAI/OpenAI on /v1/responses path), the denormalize call is a no-op.
    -- For providers that translate to chat.completion shape (Anthropic/Vertex)
    -- and for chat.completion-native providers (NVIDIA/claude-cli), we re-emit
    -- the response as output[] so the client sees a uniform Responses API shape.
    local needs_responses_reshape = record.is_responses_api
        and ngx.status >= 200 and ngx.status < 400

    if record.needs_response_translation and ngx.status >= 200 and ngx.status < 400 then
        if not eof then
            ngx.arg[1] = nil
        else
            local full_native = table.concat(chunks)
            ngx.ctx.gw_response_buf_native = full_native
            if full_native ~= "" then
                local resp = cjson.decode(full_native)
                if resp then
                    local translated = translator.translate_response(record.provider, resp)
                    if needs_responses_reshape then
                        translated = translator.denormalize_messages_to_responses(translated)
                    end
                    local normalized = cjson.encode(translated)

                    local scrub_res = credential_scrub.scrub(normalized)
                    if scrub_res.redactions > 0 then
                        ngx.ctx.gw_credentials_redacted = true
                        ngx.ctx.gw_credentials_matched = scrub_res.matched
                        ngx.log(ngx.WARN, "cost: translated response leaked credentials, redacting (",
                                scrub_res.redactions, " matches)")
                        normalized = scrub_res.scrubbed_text
                    end

                    ngx.arg[1] = normalized
                    ngx.ctx.gw_response_buf_normalized = normalized
                    ngx.log(ngx.INFO, "cost: response translated from ",
                            record.provider, " format",
                            needs_responses_reshape and " [responses-api]" or "")
                else
                    ngx.log(ngx.WARN, "cost: failed to decode native response for translation")
                    ngx.arg[1] = full_native
                    ngx.ctx.gw_response_buf_normalized = full_native
                end
            end
        end
    elseif needs_responses_reshape then
        -- Passthrough provider (no translate_response needed) but client
        -- requested Responses shape. Decode native, re-emit as output[].
        -- Idempotent for upstreams that already returned output[] (xAI/OpenAI
        -- on /v1/responses path) — denormalize_messages_to_responses returns
        -- the input unchanged when resp.output already exists.
        if not eof then
            ngx.arg[1] = nil
        else
            local full_native = table.concat(chunks)
            ngx.ctx.gw_response_buf_native = full_native
            if full_native ~= "" then
                local resp = cjson.decode(full_native)
                if resp then
                    local reshaped = translator.denormalize_messages_to_responses(resp)
                    local normalized = cjson.encode(reshaped)

                    local scrub_res = credential_scrub.scrub(normalized)
                    if scrub_res.redactions > 0 then
                        ngx.ctx.gw_credentials_redacted = true
                        ngx.ctx.gw_credentials_matched = scrub_res.matched
                        ngx.log(ngx.WARN, "cost: responses-shape response leaked credentials, redacting (",
                                scrub_res.redactions, " matches)")
                        normalized = scrub_res.scrubbed_text
                    end

                    ngx.arg[1] = normalized
                    ngx.ctx.gw_response_buf_normalized = normalized
                else
                    -- Decode failed — pass through native bytes; the client
                    -- gets the upstream body unchanged. Log so the failure
                    -- is observable rather than silent.
                    ngx.log(ngx.WARN, "cost: responses-api passthrough decode failed; returning native body")
                    local scrub_res = credential_scrub.scrub(full_native)
                    local normalized = scrub_res.scrubbed_text
                    if scrub_res.redactions > 0 then
                        ngx.ctx.gw_credentials_redacted = true
                        ngx.ctx.gw_credentials_matched = scrub_res.matched
                    end
                    ngx.arg[1] = normalized
                    ngx.ctx.gw_response_buf_normalized = normalized
                end
            end
        end
    else
        if eof then
            local full_native = table.concat(chunks)
            ngx.ctx.gw_response_buf_native     = full_native

            local scrub_res = credential_scrub.scrub(full_native)
            local normalized = scrub_res.scrubbed_text
            if scrub_res.redactions > 0 then
                ngx.ctx.gw_credentials_redacted = true
                ngx.ctx.gw_credentials_matched = scrub_res.matched
                ngx.log(ngx.WARN, "cost: response leaked credentials, redacting (",
                        scrub_res.redactions, " matches)")
                ngx.arg[1] = normalized
            end

            ngx.ctx.gw_response_buf_normalized = normalized

            -- Council body snapshot (spec §6.2 / P0 #8). Freeze the
            -- normalized bytes onto the record so log-phase cleanup has a
            -- known-good source for the idempotency-cache entry. Council
            -- is a passthrough provider — native == normalized in this
            -- branch — but capturing here defends against future per-provider
            -- post-processing on this code path.
            if record and record.provider == "council" then
                record.council_response_body = normalized
                if not normalized or #normalized < 2 then
                    ngx.log(ngx.ERR, "council body capture: empty body for ",
                            record.request_id or "?")
                end
            end
        end
    end
end

-- ---------------------------------------------------------------------------
-- Council cleanup helper (spec §5.5).
--
-- Defers the unlock + idempotency Store/Fail UDS calls into a timer so they
-- run OUTSIDE the log_by_lua phase where cosockets are prohibited (P0 #2).
-- All function references are bound at module load via the
-- `sidecar_council_*` locals defined at the top of this module — the
-- timer-closure lint (test/lint-timer-closures.sh) fails the build if
-- `sidecar.X(...)` appears anywhere inside `ngx.timer.at`.
--
-- `success` drives the terminal-state transition:
--   * success == true   → Stored (replayable for 24h)
--   * success == false  → Failed (Pending released; retry-able after 60s)
-- ---------------------------------------------------------------------------
local function schedule_council_cleanup(caller_ns, idem_key, body_sha,
                                        success, response_body, headers,
                                        owner_request_id, response_body_sha256,
                                        grant_id)
    local ok, err = ngx.timer.at(0, function(premature)
        if premature then return end
        local _, unlock_err = sidecar_council_unlock(caller_ns, grant_id or "")
        if unlock_err then
            ngx.log(ngx.ERR, "council_unlock failed: ", unlock_err)
        end
        if idem_key and idem_key ~= "" then
            if success then
                local _, store_err = sidecar_council_idem_store(
                    caller_ns, idem_key, body_sha,
                    {
                        status    = 200,
                        body_json = response_body or "",
                        headers   = headers or {},
                    },
                    86400,
                    owner_request_id or "",
                    response_body_sha256 or "")
                if store_err then
                    ngx.log(ngx.ERR, "council_idem_store failed: ", store_err)
                end
            else
                local _, fail_err = sidecar_council_idem_fail(caller_ns, idem_key)
                if fail_err then
                    ngx.log(ngx.ERR, "council_idem_fail failed: ", fail_err)
                end
            end
        end
    end)
    if not ok then
        -- P0-4: when ngx.timer.at refuses (timer pool full / shutting down),
        -- the cleanup never fires — the council concurrency slot leaks.
        -- The sidecar sweeper reclaims after PENDING_TTL+30s, but that's
        -- slow. Emit a metric so operators see the symptom directly and
        -- can correlate with council_active_swept_total on the sidecar.
        ngx.log(ngx.ERR,
            "ERR_COUNCIL_CLEANUP_TIMER_REJECTED: ngx.timer.at rejected: ",
            err or "unknown",
            " (slot will be reclaimed by sidecar sweeper after ~110s)")
        local m = ngx.shared.gw_metrics
        if m then m:incr("council_cleanup_timer_rejected_total:unknown", 1, 0) end
    end
end

-- ---------------------------------------------------------------------------
-- Replay accounting (spec §6.3).
--
-- Emits the cached response synchronously (the client is waiting for it) but
-- defers the ledger write via the existing `ledger.schedule` pattern so the
-- access-phase exit doesn't block on a cosocket. Adds the
-- X-Idempotency-Replay header so callers can distinguish replays from fresh
-- responses.
-- ---------------------------------------------------------------------------
function _M.account_replay(record)
    local request_id = record.request_id
    local idem_key   = record.council_replay_idem
    local orig_sid   = record.council_replay_orig_session_id or ""
    local cached     = record.council_replay_cached or {}
    local caller_ns  = record.caller_key
    local alias      = record.alias

    -- P0-3: pin both fingerprints + the original request_id on the replay
    -- ledger row so the non-repudiation pair `(raw_body_sha256,
    -- response_body_sha256)` is preserved across replays. The replay's
    -- raw_body_sha256 is the SHA of THIS request's body (already guaranteed
    -- to match the original via §5.7's idempotency-conflict invariant —
    -- mismatched bodies under the same key return 409). response_body_sha256
    -- is pulled from the stored entry; falls back to a freshly computed sha
    -- of the cached body if the sidecar didn't surface it (older entries
    -- written before P0-3 landed).
    local raw_body_sha    = hash.body_sha256_hex(record.raw_body or "")
    local resp_sha        = record.council_replay_resp_sha
    if (not resp_sha or resp_sha == "") and cached.body_json then
        resp_sha = hash.body_sha256_hex(cached.body_json)
    end
    local orig_request_id = record.council_replay_orig_request_id or ""

    -- Ledger row for the replay. account_replay rows have kind="council_replay",
    -- no parent linkage, and zero cost — so they're counted as zero by the
    -- §6.4 aggregation rule without skewing totals. Council discriminators
    -- (kind, council_session_id, idempotency_key, wrapper_cost_usd) live in
    -- PAYLOAD so the §6.4 SQL `json_extract(payload, '$.kind')` matches —
    -- mirrors what G-2 did for the wrapper emission below.
    ledger_schedule("council_replay", request_id, function(premature)
        if premature then return end
        ledger_record("client", alias, {
            request_id           = request_id,
            raw_body_sha256      = raw_body_sha,
            raw_body_size_bytes  = #(record.raw_body or ""),
            message_count        = 0,
            kind                 = "council_replay",
            council_session_id   = orig_sid,
            idempotency_key      = idem_key,
            wrapper_cost_usd     = 0.0,
            response_body_sha256 = resp_sha or "",
            original_request_id  = orig_request_id,
        }, {
            action              = "council_replay",
            request_id          = request_id,
            provider            = "council",
        }, caller_ns)
    end)

    ngx.status = cached.status or 200
    if type(cached.headers) == "table" then
        for k, v in pairs(cached.headers) do
            ngx.header[k] = v
        end
    end
    ngx.header["X-Idempotency-Replay"] = "true"
    ngx.header["Content-Type"] = "application/json"
    -- Mark so the log phase's account() skips emitting a spurious
    -- council_wrapper row after our ngx.exit. Without this guard the chain
    -- gets a duplicate event per replay (cost=0, latency=3ms) and any
    -- kind-grouped aggregation double-counts the replay.
    record.is_replay_exit = true
    -- ngx.print (NOT ngx.say) so we don't append a trailing newline. The
    -- original response — buffered through body_filter and emitted by
    -- nginx — has no trailing newline; ngx.say would add one, making the
    -- replay body exactly one byte longer than the fresh response and
    -- violating P0 #8 ("response body must byte-equal test #1").
    ngx.print(cached.body_json or "{}")
    return ngx.exit(ngx.status)
end

-- ---------------------------------------------------------------------------
-- LOG PHASE — parse tokens, calculate cost, write metrics, sidecar feedback
-- ---------------------------------------------------------------------------
function _M.account()
    local gw     = ngx.ctx.gw
    local record = gw and gw.record or nil
    if not record then return end

    -- G-5b: when account_replay() already wrote the canonical council_replay
    -- row and ngx.exit'd, nginx still fires this log phase. Skip — otherwise
    -- we emit a spurious council_wrapper (cost=0, latency=3ms) and the chain
    -- gets a duplicate event per replay.
    if record.is_replay_exit then return end

    local latency_ms = math.floor((ngx.now() - (record.t0 or ngx.now())) * 1000)

    -- Requests that errored before route resolution (e.g., unknown model →
    -- 400, guard block → 403) never reached an upstream provider. There is
    -- nothing to outcome-report, ledger as outbound_response, or cache.
    if not record.provider then
        return
    end

    -- =====================================================================
    -- G-4: council cleanup runs BEFORE the response-parse branches. Upstream
    -- parse failures (DNS NXDOMAIN → openresty 502 HTML page, malformed
    -- JSON) would otherwise short-circuit via the `return` at lines ~760/764
    -- and skip cleanup at the bottom of this function, leaking the
    -- concurrency slot + Pending idem state permanently until a sidecar
    -- restart. Cleanup is independent of token/cost parsing: it only reads
    -- the council bookkeeping fields frozen on `record` at access time plus
    -- the captured response buffer.
    -- =====================================================================
    -- P2-B: skip cleanup when the client aborted — router.lua's
    -- ngx.on_abort handler already released the slot synchronously and
    -- set council_locked = false. The guard on council_locked alone is
    -- sufficient (the abort handler clears it), but check council_aborted
    -- explicitly for grep-ability and to document intent at the call site.
    if record.provider == "council" and record.council_locked
       and not record.council_cleanup_scheduled
       and not record.council_aborted then
        local _early_is_success = ngx.status >= 200 and ngx.status < 400
        local _early_native     = ngx.ctx.gw_response_buf_native
        local _early_norm       = ngx.ctx.gw_response_buf_normalized or _early_native
        local council_caller_ns = record.council_caller_ns
            or (record.caller_key ~= "" and record.caller_key)
            or record.budget_key or "default"
        local council_body = record.council_response_body or _early_norm
        local cleanup_success = _early_is_success
        if cleanup_success and (not council_body or #council_body < 2) then
            ngx.log(ngx.ERR, "council_idem_store skipped: empty response body for ",
                    record.request_id or "?")
            cleanup_success = false
        end
        -- P0-2: prefer the full snapshot captured at header_filter time
        -- (record.council_response_headers) over the sparse 3-header fallback.
        -- The snapshot gives a wire-byte-equal replay; the fallback is kept
        -- for code paths that bypassed the header_filter capture (e.g. an
        -- upstream parse error short-circuiting before headers were stamped).
        local council_headers = record.council_response_headers or {
            ["X-Council-Session-Id"] = record.council_session_id or "",
            ["X-Total-Cost-Usd"]     = record.council_total_cost
                                       and string.format("%.4f", record.council_total_cost) or "",
            ["X-Chair-Tokens"]       = ngx.var.upstream_http_x_chair_tokens or "",
        }
        -- P0-3: stamp owner_request_id + response_body_sha256 so future
        -- replays of this Idempotency-Key carry `original_request_id` +
        -- `response_body_sha256` in their `council_replay` ledger rows.
        local council_resp_sha = council_body
            and hash.body_sha256_hex(council_body) or ""
        schedule_council_cleanup(
            council_caller_ns,
            record.council_idem_key,
            record.council_body_sha,
            cleanup_success,
            council_body,
            council_headers,
            record.request_id,
            council_resp_sha,
            record.council_grant_id  -- FIX-1: exact-slot unlock
        )
        record.council_cleanup_scheduled = true
    end

    -- =====================================================================
    -- BATCH MODE TERMINATOR
    --
    -- Batch creation/status/cancel/list returns no token usage — the actual
    -- spend lands on the results download (where the JSONL is parsed
    -- line-by-line by the caller, since the gateway still treats the body
    -- as opaque). We skip the provider usage parser entirely and emit a
    -- batch-specific terminator so the chain stays balanced (every
    -- batch_received pairs with exactly one outbound_batch).
    --
    -- Cost is intentionally 0: budget accounting for batch jobs happens
    -- out-of-band when the caller fetches /output and reconciles the JSONL.
    -- =====================================================================
    if record.batch_mode then
        local response_size = ngx.ctx.gw_response_len or 0
        local fb_request_id  = record.request_id
        local fb_provider    = record.provider
        local fb_alias       = record.alias
        local fb_caller_key  = record.caller_key
        local fb_status      = ngx.status
        local fb_latency     = latency_ms
        local fb_op          = record.batch_op or "unknown"
        local fb_resp_size   = response_size

        -- Module-level binding to ledger.record_with_retry; safe inside timer.
        ledger_schedule("outbound_batch", fb_request_id, function(premature)
            if premature then return end
            ledger_record(fb_provider, "client", {
                request_id          = fb_request_id,
                tokens_in           = 0,
                tokens_out          = 0,
                cached_in           = 0,
                cost_usd            = 0,
                latency_ms          = fb_latency,
                status              = fb_status,
                response_size_bytes = fb_resp_size,
                response_body_sha256 = "",
            }, {
                action      = "outbound_batch",
                request_id  = fb_request_id,
                provider    = fb_provider,
                batch_mode  = true,
                batch_op    = fb_op,
            }, fb_caller_key)
        end)

        local audit = cjson.encode({
            request_id  = record.request_id,
            provider    = record.provider,
            batch_mode  = true,
            batch_op    = record.batch_op,
            budget_key  = record.budget_key,
            status      = ngx.status,
            latency_ms  = latency_ms,
            resp_bytes  = response_size,
            timestamp   = ngx.localtime(),
        })
        ngx.log(ngx.INFO, "cost: ", audit)

        local metrics = ngx.shared.gw_metrics
        if metrics then
            metrics:incr("requests:batch:" .. (record.provider or "unknown"), 1, 0)
            metrics:incr("batch_ops_total:" .. (record.batch_op or "unknown"), 1, 0)
        end
        return
    end

    -- Response-capped path: the body exceeded 1MB and we stopped buffering,
    -- so we cannot parse provider-native usage. Pre-fix this branch
    -- early-returned, leaving the ledger and budget WITHOUT a record of
    -- exactly the largest requests an auditor would most want to inspect.
    --
    -- New behavior: degrade gracefully. Estimate input tokens from the
    -- request body byte length (rough: ~4 chars/token for English),
    -- under-estimate output as 0 (we genuinely don't know — over-estimating
    -- would over-bill the caller), and mark the audit/ledger entry as
    -- capped=true. Cache_store is the only thing we still skip on this
    -- path — we have no parsed response to cache.
    local capped = ngx.ctx.gw_response_capped == true
    local native_body     = ngx.ctx.gw_response_buf_native
    local normalized_body = ngx.ctx.gw_response_buf_normalized or native_body

    local usage
    local is_streaming = record.is_streaming
    if is_streaming and ngx.ctx.gw_streaming_usage then
        local su = ngx.ctx.gw_streaming_usage
        usage = {
            tokens_in  = su.prompt_tokens     or su.input_tokens  or 0,
            tokens_out = su.completion_tokens  or su.output_tokens or 0,
            cached_in  = (su.prompt_tokens_details
                         and su.prompt_tokens_details.cached_tokens)
                         or (su.input_tokens_details
                         and su.input_tokens_details.cached_tokens)
                         or su.cached_input_tokens
                         or 0,
        }
    elseif is_streaming then
        local raw_len = (record.raw_body and #record.raw_body) or 0
        local resp_len = (native_body and #native_body) or 0
        usage = {
            tokens_in  = math.floor(raw_len / 4),
            tokens_out = math.floor(resp_len / 16),
            cached_in  = 0,
        }
        ngx.log(ngx.WARN, "cost: streaming response without usage data — ",
                "estimate tokens_in≈", usage.tokens_in,
                " tokens_out≈", usage.tokens_out,
                " model=", record.resolved_model or "unknown")
    elseif capped then
        local raw_len = (record.raw_body and #record.raw_body) or 0
        usage = {
            tokens_in  = math.floor(raw_len / 4),
            tokens_out = 0,
            cached_in  = 0,
        }
        ngx.log(ngx.WARN, "cost: capped 1MB response — input-only estimate ",
                "tokens_in≈", usage.tokens_in, " model=",
                record.resolved_model or "unknown")
    else
        if not native_body or native_body == "" then return end
        local native_resp, parse_err = cjson.decode(native_body)
        if not native_resp then
            ngx.log(ngx.WARN, "cost: failed to parse native response: ", parse_err)
            return
        end
        usage = providers.extract_usage(record.provider, native_resp)
    end

    -- Calculate cost
    local pricing = record.pricing or {}
    local rate_in     = pricing.input or 0
    local rate_cached = pricing.cached_input or 0
    local rate_out    = pricing.output or 0

    local uncached_in = math.max(0, usage.tokens_in - usage.cached_in)
    local cost_usd = (uncached_in * rate_in
                      + usage.cached_in * rate_cached
                      + usage.tokens_out * rate_out) / 1000000

    -- Structured audit log (fintech-grade)
    local audit = cjson.encode({
        request_id      = record.request_id,
        model           = record.resolved_model,
        requested_model = record.requested_model,
        effective_model = record.effective_model,
        provider        = record.provider,
        budget_key      = record.budget_key,
        sensitivity     = record.sensitivity,
        council_role    = record.council_role,
        tokens_in       = usage.tokens_in,
        tokens_out      = usage.tokens_out,
        cached_in       = usage.cached_in,
        cost_usd        = math.floor(cost_usd * 1000000 + 0.5) / 1000000,
        latency_ms      = latency_ms,
        status          = ngx.status,
        capped          = capped,
        is_streaming    = is_streaming or false,
        timestamp       = ngx.localtime(),
    })
    ngx.log(ngx.INFO, "cost: ", audit)

    -- Update shared dict metrics (prometheus scrape target)
    local metrics = ngx.shared.gw_metrics
    if metrics then
        local model_key = record.resolved_model or "unknown"
        local provider_key = record.provider or "unknown"
        metrics:incr("requests:" .. model_key, 1, 0)
        metrics:incr("tokens_in:" .. model_key, usage.tokens_in, 0)
        metrics:incr("tokens_out:" .. model_key, usage.tokens_out, 0)
        metrics:incr("cost_microdollars:" .. model_key,
                      math.floor(cost_usd * 1000000 + 0.5), 0)
        metrics:incr("budget:" .. (record.budget_key or "default"),
                      math.floor(cost_usd * 1000000 + 0.5), 0)

        -- Latency histogram — LLM-API-tuned buckets (ms)
        local latency_label = provider_key .. ":" .. model_key
        local buckets = {10, 50, 100, 250, 500, 1000, 2500, 5000, 10000}
        for _, le in ipairs(buckets) do
            if latency_ms <= le then
                metrics:incr("latency_bucket:" .. latency_label .. ":" .. le, 1, 0)
            end
        end
        metrics:incr("latency_bucket:" .. latency_label .. ":+Inf", 1, 0)
        metrics:incr("latency_sum:" .. latency_label, latency_ms, 0)
        metrics:incr("latency_count:" .. latency_label, 1, 0)
    end

    -- =========================================================================
    -- SIDECAR FEEDBACK — deferred via timer (cosockets not available in log phase)
    --
    -- Closure captures only frozen scalars + parsed tables we own. The decoded
    -- request body is intentionally NOT captured: cache_store keys off
    -- (record.alias, record.raw_body) — the same bytes the cache_check used.
    -- =========================================================================
    local is_success = ngx.status >= 200 and ngx.status < 400

    local fb_provider       = record.provider
    local fb_budget_key     = record.budget_key
    local fb_cost_usd       = cost_usd
    local fb_alias          = record.alias
    local fb_raw_body       = record.raw_body
    local fb_request_id     = record.request_id
    local fb_resolved       = record.resolved_model
    local fb_effective      = record.effective_model
    local fb_sensitivity    = record.sensitivity
    local fb_council_role   = record.council_role
    local fb_caller_key     = record.caller_key
    local fb_latency_ms     = latency_ms
    local fb_status         = ngx.status
    local fb_tokens_in      = usage.tokens_in
    local fb_tokens_out     = usage.tokens_out
    local fb_cached_in      = usage.cached_in
    -- Wire bytes the client received — what response_body_sha256 hashes.
    -- For passthrough providers (OpenAI/xAI without translation),
    -- normalized==native; for translated providers (Anthropic/Vertex),
    -- this is the post-translate OpenAI shape that was emitted.
    local fb_normalized_body = normalized_body
    -- We cache the NATIVE upstream response. On cache hit, router.lua re-runs
    -- translate_response so the wire shape matches what a fresh request would
    -- have produced. Storing native preserves provider-specific fields
    -- (Anthropic stop_reason, Vertex safetyRatings, groundingMetadata) for
    -- forensics and lets the cache survive translator improvements that
    -- bump TRANSLATOR_VERSION. Capped responses skip cache_store — we have
    -- no parsed payload to cache.
    local fb_native_for_cache   = (is_success and not capped and not is_streaming and not ngx.ctx.gw_credentials_redacted) and native_body or nil
    local fb_translator_version = translator.TRANSLATOR_VERSION
    local fb_capped             = capped

    -- Council ledger annotations (§6.6). Branch on wrapper-vs-leaf:
    --   * provider == "council"            → council_wrapper kind. Wrapper
    --                                        cost is the upstream-reported
    --                                        total (already includes chair).
    --   * parent_council_request_id != ""  → leaf row (a seat call that
    --                                        flowed back through the gateway
    --                                        with X-Parent-Request-Id set by
    --                                        council-rs). Linked to the
    --                                        wrapper for the §6.4 aggregation.
    local fb_council_kind         = nil
    local fb_council_session_id   = nil
    local fb_council_chair_tokens = nil
    local fb_council_wrapper_cost = nil
    local fb_parent_council_req   = nil
    if record.provider == "council" then
        fb_council_kind         = "council_wrapper"
        fb_council_session_id   = record.council_session_id or ""
        fb_council_chair_tokens = record.council_chair_tokens or 0
        fb_council_wrapper_cost = record.council_total_cost
    elseif record.parent_council_request_id and record.parent_council_request_id ~= "" then
        fb_council_kind       = "leaf"
        fb_parent_council_req = record.parent_council_request_id
    end

    -- Council cleanup is now scheduled early in account() (G-4 fix) so the
    -- early-return on parse failure doesn't leak the concurrency slot. The
    -- block below is the legacy site, preserved as a no-op fallback in case
    -- some success-path field set after the early-block run influenced the
    -- store payload. With `council_cleanup_scheduled` set, it's skipped.
    -- P2-B: same client-abort short-circuit as the early-cleanup block
    -- at line ~702. The on_abort handler in router.lua releases the slot
    -- synchronously, so this legacy fallback site must also skip.
    if record.provider == "council" and record.council_locked
       and not record.council_cleanup_scheduled
       and not record.council_aborted then
        local council_caller_ns = record.council_caller_ns
            or (record.caller_key ~= "" and record.caller_key)
            or record.budget_key or "default"
        local council_body = record.council_response_body or normalized_body
        local cleanup_success = is_success
        if cleanup_success and (not council_body or #council_body < 2) then
            ngx.log(ngx.ERR, "council_idem_store skipped: empty response body for ",
                    record.request_id or "?")
            cleanup_success = false
        end
        -- P0-2: prefer the full snapshot captured at header_filter time
        -- (record.council_response_headers) over the sparse 3-header fallback.
        -- The snapshot gives a wire-byte-equal replay; the fallback is kept
        -- for code paths that bypassed the header_filter capture (e.g. an
        -- upstream parse error short-circuiting before headers were stamped).
        local council_headers = record.council_response_headers or {
            ["X-Council-Session-Id"] = record.council_session_id or "",
            ["X-Total-Cost-Usd"]     = record.council_total_cost
                                       and string.format("%.4f", record.council_total_cost) or "",
            ["X-Chair-Tokens"]       = ngx.var.upstream_http_x_chair_tokens or "",
        }
        -- P0-3: stamp owner_request_id + response_body_sha256 so future
        -- replays of this Idempotency-Key carry `original_request_id` +
        -- `response_body_sha256` in their `council_replay` ledger rows.
        local council_resp_sha = council_body
            and hash.body_sha256_hex(council_body) or ""
        schedule_council_cleanup(
            council_caller_ns,
            record.council_idem_key,
            record.council_body_sha,
            cleanup_success,
            council_body,
            council_headers,
            record.request_id,
            council_resp_sha,
            record.council_grant_id  -- FIX-1: exact-slot unlock
        )
        record.council_cleanup_scheduled = true
    end

    if ngx.ctx.gw_credentials_redacted then
        -- Metric for credentials scrubbed (Phase 3 stub)
        local metrics = ngx.shared.gw_metrics
        if metrics and ngx.ctx.gw_credentials_matched then
            for pattern, count in pairs(ngx.ctx.gw_credentials_matched) do
                metrics:incr("credentials_redacted_total:" .. pattern, count, 0)
            end
        end
    end

    ledger_schedule("outbound_response", fb_request_id, function(premature)
        if premature then return end

        -- Report routing outcome for per-family health tracking / circuit breakers.
        -- Uses resolved model ID (not provider) so the sidecar can derive the
        -- correct (provider, family) health key.
        -- Module-level bindings, not `sidecar.X(...)`, so closure holds
        -- function refs rather than table refs (see top of module).
        sidecar_route_outcome(
            fb_resolved or fb_provider,
            is_success,
            fb_latency_ms,
            is_success and nil or ("HTTP " .. tostring(fb_status))
        )

        -- Record actual spend against budget
        if fb_budget_key and fb_budget_key ~= "default" and fb_cost_usd > 0 then
            sidecar_budget_record(fb_budget_key, fb_cost_usd)
        end

        -- Record outbound LLM response in Audit Ledger (with retry).
        -- Sovereignty scoring is intentionally NOT included here — sovereignty
        -- is IRIN's concern, not the gateway's. The /guard/sovereignty
        -- endpoint remains available for IRIN to invoke directly.
        --
        -- response_body_sha256 hashes the NORMALIZED (post-translate) wire
        -- bytes — what the client actually received. This is the load-bearing
        -- field for non-repudiation: paired with request_received's
        -- raw_body_sha256, an auditor can prove "request X produced response
        -- Y" without the gateway storing either body. The native body is
        -- discarded after parsing; only its hash survives on the chain.
        local ledger_payload = {
            request_id           = fb_request_id,
            tokens_in            = fb_tokens_in,
            tokens_out           = fb_tokens_out,
            cached_in            = fb_cached_in,
            cost_usd             = fb_cost_usd,
            latency_ms           = fb_latency_ms,
            status               = fb_status,
            response_body_sha256 = fb_normalized_body
                                    and hash.body_sha256_hex(fb_normalized_body)
                                    or "",
            response_size_bytes  = fb_normalized_body and #fb_normalized_body or 0,
        }
        local ledger_metadata = {
            action          = "outbound_response",
            request_id      = fb_request_id,
            resolved_model  = fb_resolved,
            effective_model = fb_effective,
            sensitivity     = fb_sensitivity,
            council_role    = fb_council_role,
            capped          = fb_capped,
            is_streaming    = is_streaming or false,
            tokens_estimated = fb_capped,
        }
        -- §6.6 — council wrapper or leaf annotation. `kind` drives the
        -- §6.4 SQL aggregation: wrapper rows (no parent) count toward
        -- totals; leaf rows (parent linked) are excluded so seat costs
        -- aren't double-counted alongside the wrapper.
        if fb_council_kind then
            -- Council fields live in PAYLOAD (not metadata) so spec §6.4 SQL
            -- aggregation (json_extract(payload, '$.parent_council_request_id'))
            -- works as written. Metadata stays a header-style annotation;
            -- payload is the event body.
            ledger_payload.kind = fb_council_kind
            if fb_council_kind == "council_wrapper" then
                ledger_payload.council_session_id = fb_council_session_id
                ledger_payload.chair_tokens       = fb_council_chair_tokens
                if fb_council_wrapper_cost then
                    ledger_payload.wrapper_cost_usd = fb_council_wrapper_cost
                end
            elseif fb_council_kind == "leaf" then
                ledger_payload.parent_council_request_id = fb_parent_council_req
            end
        end
        ledger_record(fb_provider, "client", ledger_payload, ledger_metadata, fb_caller_key)

        -- Cache successful responses keyed on (alias, raw_body) — same
        -- key cache_check used at access time. We store the NATIVE body
        -- plus the provider and translator version so router.lua can
        -- re-translate on hit. This makes cache hits indistinguishable
        -- from fresh requests in wire shape, and lets us invalidate stale
        -- entries after any translator change without a manual sweep.
        if fb_native_for_cache and fb_alias and fb_raw_body and fb_provider then
            local resp_decoded = cjson.decode(fb_native_for_cache)
            if resp_decoded then
                sidecar_cache_store(
                    fb_alias, fb_raw_body, resp_decoded,
                    fb_provider, fb_translator_version, nil
                )
            end
        end
    end)
end

-- ---------------------------------------------------------------------------
-- COUNCIL STATS POLLER
-- ---------------------------------------------------------------------------
-- The sidecar holds the canonical values of `active_swept_total`,
-- `unlock_missing_grant_total`, and the current active-locks gauges. Until
-- the sidecar has its own /metrics endpoint, the gateway polls
-- `/council/stats` on a 30s timer (worker 0 only) and mirrors the snapshot
-- into the gw_metrics shared dict so the existing prometheus() renderer
-- can surface them. Failures are best-effort: a missed poll just keeps the
-- previously-cached values until the next tick.

local COUNCIL_STATS_POLL_INTERVAL = 30  -- seconds

function _M.poll_council_stats()
    local stats, err = sidecar_council_stats()
    if not stats then
        ngx.log(ngx.WARN, "council_stats poll failed: ", err)
        return
    end
    local metrics = ngx.shared.gw_metrics
    if not metrics then return end
    metrics:set("council_active_swept_total",         stats.active_swept_total         or 0)
    metrics:set("council_unlock_missing_grant_total", stats.unlock_missing_grant_total or 0)
    metrics:set("council_active_locks",               stats.active_locks               or 0)
    metrics:set("council_active_caller_keys",         stats.active_caller_keys         or 0)
    metrics:set("council_stored_bytes",               stats.stored_bytes               or 0)
end

-- ---------------------------------------------------------------------------
-- WATCH STATS POLLER
-- ---------------------------------------------------------------------------
-- Mirrors the council_stats poller. Sidecar exposes audit-infra + persist-
-- failure counters via JSON /watch/stats; the gateway mirrors them into
-- gw_metrics so prometheus() emits gw_watch_audit_infra_errors_total and
-- gw_watch_persist_failures_total. Closes the silent-unscrape gap: without
-- this poller, /watch/stats is reachable but invisible on /metrics, so SRE
-- sees zero watch_* counters and assumes "no incidents."
function _M.poll_watch_stats()
    local stats, err = sidecar_watch_stats()
    if not stats then
        ngx.log(ngx.WARN, "watch_stats poll failed: ", err)
        return
    end
    local metrics = ngx.shared.gw_metrics
    if not metrics then return end
    metrics:set("watch_audit_infra_errors_total", stats.audit_infra_errors_total or 0)
    metrics:set("watch_persist_failures_total",   stats.persist_failures_total   or 0)
    -- Snapshot gauge for records parked in pending_hard_kill_persist limbo.
    -- Pairs with persist_failures_total (counter) so SRE can see both
    -- the rate of new failures and the current depth of the pending pool.
    metrics:set("watch_pending_pending_records",  stats.pending_pending_records  or 0)
    -- Sibling counter to persist_failures_total: that one counts first-fail events inside
    -- record_failure; this one counts retry-loop attempts that also failed.
    -- Rising delta = the DB is still broken from the retry's POV.
    metrics:set("watch_pending_retry_failures_total", stats.pending_retry_failures_total or 0)
    -- Age in ms of the oldest pending record. First-set Instant
    -- semantics (retries don't restamp) so this gauge monotonically rises
    -- until persist or admin clear. Answers "how stuck is the worst record"
    -- with a single scrape, no histogram needed.
    metrics:set("watch_pending_oldest_age_ms",       stats.pending_oldest_age_ms       or 0)
    -- The sidecar's arming telemetry is exposed on /watch/stats and mirrored
    -- here so the
    -- runbook's promised gw_watch_* names scraped as ABSENT on /metrics.
    -- Map every arming-gate field. NOTE the JSON field for the lease counter
    -- is `lease_expired_during_deliberation` (no `_total` suffix on the
    -- wire); the Prometheus name carries the suffix per convention.
    metrics:set("watch_lease_expired_during_deliberation_total",
                stats.lease_expired_during_deliberation or 0)
    metrics:set("watch_dup_charge_alarm_total",          stats.dup_charge_alarm_total          or 0)
    metrics:set("watch_directive_ttl_expired_total",     stats.directive_ttl_expired_total     or 0)
    metrics:set("watch_directive_max_delivery_exceeded_total", stats.directive_max_delivery_exceeded_total or 0)
    metrics:set("watch_directive_clock_skew_rejected_total", stats.directive_clock_skew_rejected_total or 0)
    metrics:set("watch_spend_today_usd",                 stats.spend_today_usd                 or 0)
    metrics:set("watch_spend_cap_usd",                   stats.spend_cap_usd                   or 0)
    metrics:set("watch_kill_switch_latency_ms",          stats.kill_switch_latency_ms          or 0)
    metrics:set("watch_kill_switch_latency_max_ms",      stats.kill_switch_latency_max_ms      or 0)
    metrics:set("watch_recon_divergence_total",          stats.recon_divergence_total          or 0)
    metrics:set("watch_settle_ceiling_overshoot_total",  stats.settle_ceiling_overshoot_total  or 0)
    metrics:set("watch_spend_gauge_read_failures_total", stats.spend_gauge_read_failures_total or 0)
    metrics:set("watch_kill_switch_drain_timeout_total", stats.kill_switch_drain_timeout_total or 0)
    -- Unauthenticated 401 arm rejections are
    -- counted here instead of appended to the unprunable arm_audit chain.
    metrics:set("watch_arm_rejected_unauth_total",       stats.arm_rejected_unauth_total       or 0)
end

-- Schedule both pollers from init_worker. Only worker 0 polls — otherwise N
-- workers would multiply sidecar load without adding fidelity. Called once
-- per worker process; the per-tick callback runs in worker 0's event loop.
function _M.init_worker()
    if ngx.worker.id() ~= 0 then return end
    -- Kick off an immediate poll so /metrics is populated before the first
    -- 30s tick, then schedule the recurring timer.
    local ok, err = ngx.timer.at(0, function() _M.poll_council_stats() end)
    if not ok then
        ngx.log(ngx.ERR, "council_stats initial timer.at rejected: ", err)
    end
    local ok2, err2 = ngx.timer.every(COUNCIL_STATS_POLL_INTERVAL,
        function() _M.poll_council_stats() end)
    if not ok2 then
        ngx.log(ngx.ERR, "council_stats recurring timer.every rejected: ", err2)
    end

    -- The watch-plane poller uses the same cadence; both pollers share the
    -- 30s tick budget.
    local ok3, err3 = ngx.timer.at(0, function() _M.poll_watch_stats() end)
    if not ok3 then
        ngx.log(ngx.ERR, "watch_stats initial timer.at rejected: ", err3)
    end
    local ok4, err4 = ngx.timer.every(COUNCIL_STATS_POLL_INTERVAL,
        function() _M.poll_watch_stats() end)
    if not ok4 then
        ngx.log(ngx.ERR, "watch_stats recurring timer.every rejected: ", err4)
    end
end

-- ---------------------------------------------------------------------------
-- PROMETHEUS — /metrics endpoint
-- ---------------------------------------------------------------------------
function _M.prometheus()
    local metrics = ngx.shared.gw_metrics
    ngx.header["Content-Type"] = "text/plain"

    if not metrics then
        ngx.say("# no metrics available")
        return
    end

    -- Ensure the core watch-plane metrics are always
    -- present in the dict so the value lines are emitted even on a brand-new
    -- gateway before the first 30s poller tick. The metrics-contract test
    -- scrapes immediately after up and expects the lines (as 0).
    for _, k in ipairs({
        "watch_audit_infra_errors_total",
        "watch_persist_failures_total",
        "watch_pending_pending_records",
        "watch_pending_retry_failures_total",
        "watch_pending_oldest_age_ms",
        "council_stored_bytes"
    }) do
        if not metrics:get(k) then
            metrics:set(k, 0)
        end
    end

    local keys = metrics:get_keys(2048)
    local output = {
        "# HELP gw_requests_total Total number of LLM requests routed",
        "# TYPE gw_requests_total counter",
        "# HELP gw_tokens_in_total Total prompt tokens processed",
        "# TYPE gw_tokens_in_total counter",
        "# HELP gw_tokens_out_total Total completion tokens generated",
        "# TYPE gw_tokens_out_total counter",
        "# HELP gw_cost_microdollars_total Total estimated cost in micro-USD",
        "# TYPE gw_cost_microdollars_total counter",
        "# HELP gw_shape_gate_violations_total Number of structural ASM gate drops",
        "# TYPE gw_shape_gate_violations_total counter",
        "# HELP gw_decon_blocks_total Number of decontaminator input blocks",
        "# TYPE gw_decon_blocks_total counter",
        "# HELP gw_credentials_redacted_total Number of secrets redacted on outbound",
        "# TYPE gw_credentials_redacted_total counter",
        "# HELP gw_auth_rejections_total Authentication rejections by reason",
        "# TYPE gw_auth_rejections_total counter",
        "# HELP gw_ip_gate_blocks_total IP policy gate blocks",
        "# TYPE gw_ip_gate_blocks_total counter",
        "# HELP gw_cache_outcomes_total Cache check outcomes",
        "# TYPE gw_cache_outcomes_total counter",
        "# HELP gw_request_duration_ms Request latency in milliseconds",
        "# TYPE gw_request_duration_ms histogram",
        "# HELP gw_errors_total Error responses by stable error code",
        "# TYPE gw_errors_total counter",
        "# HELP gw_provider_parser_missing_total Usage extracted with no parser registered",
        "# TYPE gw_provider_parser_missing_total counter",
        "# HELP gw_provider_parser_error_total Usage parser raised an exception",
        "# TYPE gw_provider_parser_error_total counter",
        "# HELP gw_council_cleanup_timer_rejected_total ngx.timer.at refused the council cleanup callback",
        "# TYPE gw_council_cleanup_timer_rejected_total counter",
        "# HELP gw_council_active_swept_total Council slots reclaimed by the sidecar TTL sweeper",
        "# TYPE gw_council_active_swept_total counter",
        "# HELP gw_council_unlock_missing_grant_total Unlock requests with empty grant_id",
        "# TYPE gw_council_unlock_missing_grant_total counter",
        "# HELP gw_council_active_locks Current active council concurrency slots across all caller_keys",
        "# TYPE gw_council_active_locks gauge",
        "# HELP gw_council_active_caller_keys Current distinct caller_keys with at least one council slot",
        "# TYPE gw_council_active_caller_keys gauge",
        "# HELP gw_council_stored_bytes Approximate bytes in the council Stored LRU",
        "# TYPE gw_council_stored_bytes gauge",
        "# HELP gw_watch_audit_infra_errors_total Watch-plane audit-pipeline infrastructure errors",
        "# TYPE gw_watch_audit_infra_errors_total counter",
        "# HELP gw_watch_persist_failures_total Watch-plane hard-kill DB upsert failures",
        "# TYPE gw_watch_persist_failures_total counter",
        "# HELP gw_watch_pending_pending_records Watch-plane records currently parked in pending_hard_kill_persist limbo",
        "# TYPE gw_watch_pending_pending_records gauge",
        "# HELP gw_watch_pending_retry_failures_total Pending hard-kill retry attempts that ended in Err or a 5s timeout",
        "# TYPE gw_watch_pending_retry_failures_total counter",
        "# HELP gw_watch_pending_oldest_age_ms Age in ms of the oldest record parked in pending_hard_kill_persist limbo; rises until persist or admin clear",
        "# TYPE gw_watch_pending_oldest_age_ms gauge",
        "# HELP gw_watch_lease_expired_during_deliberation_total Deliberation leases lost while a council call was or may have been in flight; each pairs with a reconciliation hint",
        "# TYPE gw_watch_lease_expired_during_deliberation_total counter",
        "# HELP gw_watch_dup_charge_alarm_total Settles that wrote a realized cost over an already-settled claim; any non-zero is an idempotency alarm",
        "# TYPE gw_watch_dup_charge_alarm_total counter",
        "# HELP gw_watch_directive_ttl_expired_total Staged directives swept to expired because their TTL (expires_at_ms) elapsed before the worker could dispatch them — non-zero means directives age out pre-dispatch (too-tight TTL or stalled worker)",
        "# TYPE gw_watch_directive_ttl_expired_total counter",
        "# HELP gw_watch_directive_max_delivery_exceeded_total Staged directives dead-lettered (swept to expired) for exceeding DIRECTIVE_MAX_DELIVERY_ATTEMPTS re-claims — non-zero means a poison directive (fails verify every tick) or a flapping/crashing worker was stopped by the attempt ceiling",
        "# TYPE gw_watch_directive_max_delivery_exceeded_total counter",
        "# HELP gw_watch_directive_clock_skew_rejected_total Directives refused at stage time because their created-time normalization delta exceeded MAX_ALLOWED_SKEW_MS — non-zero means a host clock glitched forward and poisoned the per-tenant monotonic floor (prior_max), and the breaker is fail-safe-refusing every later directive for that tenant",
        "# TYPE gw_watch_directive_clock_skew_rejected_total counter",
        "# HELP gw_watch_spend_today_usd Today's UTC-bucket council spend, reserved + settled, from the spend ledger",
        "# TYPE gw_watch_spend_today_usd gauge",
        "# HELP gw_watch_spend_cap_usd The enforced UTC-day council spend cap (DAILY_SPEND_CAP)",
        "# TYPE gw_watch_spend_cap_usd gauge",
        "# HELP gw_watch_kill_switch_latency_ms Wall ms from the last disarm signal to producer drain acknowledgement",
        "# TYPE gw_watch_kill_switch_latency_ms gauge",
        "# HELP gw_watch_kill_switch_latency_max_ms Max observed disarm-to-drain latency in ms since boot",
        "# TYPE gw_watch_kill_switch_latency_max_ms gauge",
        "# HELP gw_watch_recon_divergence_total Out-of-band spend reconciliations whose divergence exceeded the alarm threshold",
        "# TYPE gw_watch_recon_divergence_total counter",
        "# HELP gw_watch_settle_ceiling_overshoot_total Settles whose realized cost exceeded the reservation ceiling",
        "# TYPE gw_watch_settle_ceiling_overshoot_total counter",
        "# HELP gw_watch_spend_gauge_read_failures_total spend_today_usd gauge reads that failed; the gauge is blind, not zero, while this rises",
        "# TYPE gw_watch_spend_gauge_read_failures_total counter",
        "# HELP gw_watch_kill_switch_drain_timeout_total Disarms whose producer drain acknowledgement timed out at 5s; latency floor 5000ms recorded",
        "# TYPE gw_watch_kill_switch_drain_timeout_total counter",
        "# HELP gw_watch_arm_rejected_unauth_total Unauthenticated (401) arm stage/confirm rejections — counted instead of written to the unprunable arm_audit chain (DoS guard)",
        "# TYPE gw_watch_arm_rejected_unauth_total counter",
    }

    local metric_lines = {}

    -- Metric dispatch table: metric_name → format function
    local renderers = {
        requests = function(label, val) return string.format('gw_requests_total{model="%s"} %s', label, val) end,
        tokens_in = function(label, val) return string.format('gw_tokens_in_total{model="%s"} %s', label, val) end,
        tokens_out = function(label, val) return string.format('gw_tokens_out_total{model="%s"} %s', label, val) end,
        cost_microdollars = function(label, val) return string.format('gw_cost_microdollars_total{model="%s"} %s', label, val) end,
        budget = function(label, val) return string.format('gw_budget_spent_microdollars{key="%s"} %s', label, val) end,
        shape_gate_violations_total = function(label, val) return string.format('gw_shape_gate_violations_total{field="%s"} %s', label, val) end,
        decon_blocks_total = function(label, val) return string.format('gw_decon_blocks_total{threat="%s"} %s', label, val) end,
        credentials_redacted_total = function(label, val) return string.format('gw_credentials_redacted_total{pattern="%s"} %s', label, val) end,
        auth_rejections_total = function(label, val) return string.format('gw_auth_rejections_total{reason="%s"} %s', label, val) end,
        cache_outcomes = function(label, val) return string.format('gw_cache_outcomes_total{outcome="%s"} %s', label, val) end,
        errors_total = function(label, val) return string.format('gw_errors_total{code="%s"} %s', label, val) end,
        provider_parser_missing_total = function(label, val) return string.format('gw_provider_parser_missing_total{provider="%s"} %s', label, val) end,
        provider_parser_error_total = function(label, val) return string.format('gw_provider_parser_error_total{provider="%s"} %s', label, val) end,
        council_cleanup_timer_rejected_total = function(_, val) return string.format('gw_council_cleanup_timer_rejected_total %s', val) end,
    }

    for _, key in ipairs(keys) do
        local val = metrics:get(key)
        if val then
            -- Unlabeled counters
            if key == "ip_gate_blocks_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_ip_gate_blocks_total %s', tostring(val))
            elseif key == "council_active_swept_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_council_active_swept_total %s', tostring(val))
            elseif key == "council_unlock_missing_grant_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_council_unlock_missing_grant_total %s', tostring(val))
            elseif key == "council_active_locks" then
                metric_lines[#metric_lines + 1] = string.format('gw_council_active_locks %s', tostring(val))
            elseif key == "council_active_caller_keys" then
                metric_lines[#metric_lines + 1] = string.format('gw_council_active_caller_keys %s', tostring(val))
            elseif key == "council_stored_bytes" then
                metric_lines[#metric_lines + 1] = string.format('gw_council_stored_bytes %s', tostring(val))
            elseif key == "watch_audit_infra_errors_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_audit_infra_errors_total %s', tostring(val))
            elseif key == "watch_persist_failures_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_persist_failures_total %s', tostring(val))
            elseif key == "watch_pending_pending_records" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_pending_pending_records %s', tostring(val))
            elseif key == "watch_pending_retry_failures_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_pending_retry_failures_total %s', tostring(val))
            elseif key == "watch_pending_oldest_age_ms" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_pending_oldest_age_ms %s', tostring(val))
            elseif key == "watch_lease_expired_during_deliberation_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_lease_expired_during_deliberation_total %s', tostring(val))
            elseif key == "watch_dup_charge_alarm_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_dup_charge_alarm_total %s', tostring(val))
            elseif key == "watch_directive_ttl_expired_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_directive_ttl_expired_total %s', tostring(val))
            elseif key == "watch_directive_max_delivery_exceeded_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_directive_max_delivery_exceeded_total %s', tostring(val))
            elseif key == "watch_directive_clock_skew_rejected_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_directive_clock_skew_rejected_total %s', tostring(val))
            elseif key == "watch_spend_today_usd" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_spend_today_usd %s', tostring(val))
            elseif key == "watch_spend_cap_usd" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_spend_cap_usd %s', tostring(val))
            elseif key == "watch_kill_switch_latency_ms" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_kill_switch_latency_ms %s', tostring(val))
            elseif key == "watch_kill_switch_latency_max_ms" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_kill_switch_latency_max_ms %s', tostring(val))
            elseif key == "watch_recon_divergence_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_recon_divergence_total %s', tostring(val))
            elseif key == "watch_settle_ceiling_overshoot_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_settle_ceiling_overshoot_total %s', tostring(val))
            elseif key == "watch_spend_gauge_read_failures_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_spend_gauge_read_failures_total %s', tostring(val))
            elseif key == "watch_kill_switch_drain_timeout_total" then
                metric_lines[#metric_lines + 1] = string.format('gw_watch_kill_switch_drain_timeout_total %s', tostring(val))
            else
                -- Histogram keys: latency_bucket:provider:model:le
                local htype, provider, model, le = key:match("^(latency_bucket):([^:]+):([^:]+):(.+)$")
                if htype then
                    metric_lines[#metric_lines + 1] = string.format(
                        'gw_request_duration_ms_bucket{provider="%s",model="%s",le="%s"} %s',
                        provider, model, le, tostring(val))
                else
                    local stype, slabel = key:match("^(latency_sum):(.+)$")
                    if stype then
                        local sp, sm = slabel:match("^([^:]+):(.+)$")
                        if sp and sm then
                            metric_lines[#metric_lines + 1] = string.format(
                                'gw_request_duration_ms_sum{provider="%s",model="%s"} %s',
                                sp, sm, tostring(val))
                        end
                    else
                        local ctype, clabel = key:match("^(latency_count):(.+)$")
                        if ctype then
                            local cp, cm = clabel:match("^([^:]+):(.+)$")
                            if cp and cm then
                                metric_lines[#metric_lines + 1] = string.format(
                                    'gw_request_duration_ms_count{provider="%s",model="%s"} %s',
                                    cp, cm, tostring(val))
                            end
                        else
                            -- Simple labeled counters
                            local metric_name, label = key:match("^([^:]+):(.+)$")
                            if metric_name and label and renderers[metric_name] then
                                metric_lines[#metric_lines + 1] = renderers[metric_name](label, tostring(val))
                            end
                        end
                    end
                end
            end
        end
    end

    table.sort(metric_lines)
    for _, line in ipairs(metric_lines) do
        output[#output + 1] = line
    end

    ngx.say(table.concat(output, "\n"))
end

return _M
