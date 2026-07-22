-- ==========================================================================
-- lib/providers.lua — Provider-specific response parsing.
--
-- Each provider returns usage data in a different schema. This module
-- normalizes them all to {tokens_in, tokens_out, cached_in}.
-- ==========================================================================

local _M = {}

-- ---------------------------------------------------------------------------
-- Grok (xAI) — v1/responses format
-- Token paths:
--   usage.input_tokens (or usage.prompt_tokens)
--   usage.output_tokens (or usage.completion_tokens)
--   usage.input_tokens_details.cached_tokens (or usage.cached_input_tokens)
-- ---------------------------------------------------------------------------
local function parse_xai(resp)
    local u = resp.usage or {}
    local cached = 0
    local details = u.input_tokens_details
    if details and details.cached_tokens then
        cached = details.cached_tokens
    else
        cached = u.cached_input_tokens or 0
    end
    return {
        tokens_in  = u.input_tokens or u.prompt_tokens or 0,
        tokens_out = u.output_tokens or u.completion_tokens or 0,
        cached_in  = cached,
    }
end

-- ---------------------------------------------------------------------------
-- OpenAI (GPT) — v1/responses format (same as xAI, different cached path)
-- Token paths:
--   usage.input_tokens (or usage.prompt_tokens)
--   usage.output_tokens (or usage.completion_tokens)
--   usage.input_tokens_details.cached_tokens (or usage.cached_input_tokens)
-- ---------------------------------------------------------------------------
local function parse_openai(resp)
    -- Identical structure to xAI (both use v1/responses)
    return parse_xai(resp)
end

-- ---------------------------------------------------------------------------
-- Claude (Anthropic) — /v1/messages format
-- Token paths:
--   usage.input_tokens
--   usage.output_tokens
--   usage.cache_read_input_tokens   ← different field name!
-- ---------------------------------------------------------------------------
local function parse_anthropic(resp)
    local u = resp.usage or {}
    return {
        tokens_in  = u.input_tokens or 0,
        tokens_out = u.output_tokens or 0,
        cached_in  = u.cache_read_input_tokens or 0,
    }
end

-- ---------------------------------------------------------------------------
-- Gemini (Vertex AI) — generateContent format
-- Token paths:
--   usageMetadata.promptTokenCount
--   usageMetadata.candidatesTokenCount
--   (no cached tokens in Vertex response — tracked separately)
-- ---------------------------------------------------------------------------
local function parse_vertex(resp)
    local u = resp.usageMetadata or {}
    return {
        tokens_in  = u.promptTokenCount or 0,
        tokens_out = u.candidatesTokenCount or 0,
        cached_in  = 0,  -- Vertex doesn't expose cache hit in response
    }
end

-- ---------------------------------------------------------------------------
-- Dispatcher
-- ---------------------------------------------------------------------------
local parsers = {
    xai       = parse_xai,
    openai    = parse_openai,
    anthropic = parse_anthropic,
    vertex    = parse_vertex,
    nvidia    = parse_openai,   -- NIM uses OpenAI-compatible format
    -- CLI proxies emit OpenAI chat.completions shape. parse_xai's
    -- prompt_tokens/completion_tokens fallback handles their usage shape.
    ["claude-cli"] = parse_openai,
    ["gpt-cli"]    = parse_openai,
    -- council-rs /api/deliberate returns OpenAI chat.completions shape with
    -- usage = {prompt_tokens, completion_tokens, total_tokens} — same parser.
    council        = parse_openai,
}

-- Increment a labeled counter in the gw_metrics shared dict, guarded so a
-- missing dict (e.g. unit tests) never breaks the hot path.
local function incr_metric(key, label)
    local m = ngx.shared.gw_metrics
    if m then m:incr(key .. ":" .. (label or "unknown"), 1, 0) end
end

--- Extract normalized usage from a provider response.
-- @param provider string — provider name (xai, openai, anthropic, vertex)
-- @param resp table — decoded response body
-- @return table {tokens_in, tokens_out, cached_in}
--
-- The return table is intentionally PURE — only the three canonical numeric
-- fields. Failure signalling lives on the side via the gw_metrics counters
-- (provider_parser_missing_total / provider_parser_error_total) and the
-- ERR_USAGE_PARSE_FAILED log line. Returning `_missing_parser = true` on
-- the result table
-- which could leak into ledger payloads if any caller spread the table into
-- a payload literal.
function _M.extract_usage(provider, resp)
    local parser = parsers[provider]
    if not parser then
        -- Missing parser silently zeroed leaf accounting in §6.4 aggregation —
        -- the exact failure mode that masked Phase 0.5's missing gpt-cli /
        -- claude-cli / council parsers until end-to-end ledger inspection.
        -- Convert to a loud operational signal so future BYOK providers can't
        -- regress this without an alert firing.
        ngx.log(ngx.ERR, "ERR_USAGE_PARSE_FAILED: no parser for provider=", provider)
        incr_metric("provider_parser_missing_total", provider)
        return { tokens_in = 0, tokens_out = 0, cached_in = 0 }
    end
    local ok, result = pcall(parser, resp)
    if not ok then
        ngx.log(ngx.ERR, "ERR_USAGE_PARSE_FAILED: parse error provider=",
            provider, " err=", result)
        incr_metric("provider_parser_error_total", provider)
        return { tokens_in = 0, tokens_out = 0, cached_in = 0 }
    end
    return result
end

return _M
