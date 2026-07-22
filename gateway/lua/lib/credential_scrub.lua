-- ==========================================================================
-- credential_scrub.lua — Outbound response secret redaction
--
-- Scans the normalized OpenAI-format response body (produced by the
-- translator layer) for known secret patterns (AWS, GitHub, GCP, etc.).
-- Redacts matches and flags the record to prevent cache storage and
-- alert operators.
-- ==========================================================================

local _M = {}

-- Compile patterns at module load time for performance
-- T2 fix: no {n} (Lua patterns lack regex {} quantifier) -> use + / classes. Added modern formats gateway actually brokers (xai-, nvapi-, sk-ant-, ya29., github_pat_ etc). Post-send scrub limitation noted in audits.
local patterns = {
    { name = "aws_access_key", pattern = "AKIA[A-Z0-9]+" },
    { name = "private_key",    pattern = "%-%-%-%-%-BEGIN[A-Z%s]+PRIVATE KEY%-%-%-%-%-.-%-%-%-%-%-END[A-Z%s]+PRIVATE KEY%-%-%-%-%-" },
    { name = "slack_token",    pattern = "xox[baprs]%-[a-zA-Z0-9%-_]+" },
    { name = "github_pat",     pattern = "ghp_[A-Za-z0-9]+" },
    { name = "openai_key",     pattern = "sk%-[A-Za-z0-9%-_]+" },
    { name = "gitlab_pat",     pattern = "glpat%-[A-Za-z0-9%-_]+" },
    { name = "gcp_api_key",    pattern = "AIza[0-9A-Za-z%-_]+" },
    -- modern formats actually brokered (missed by original)
    { name = "xai_key",        pattern = "xai%-[A-Za-z0-9%-_]+" },
    { name = "anthropic_key",  pattern = "sk%-ant%-[A-Za-z0-9%-_]+" },
    { name = "nvidia_key",     pattern = "nvapi%-[A-Za-z0-9%-_]+" },
    { name = "google_oauth",   pattern = "ya29%.[A-Za-z0-9%-_]+" },
    { name = "github_pat_v2",  pattern = "github_pat_[A-Za-z0-9%-_]+" }
}

--- Scans text for credentials and redacts them.
-- @param text string The text to scan
-- @return table { scrubbed_text = string, redactions = number, matched = table }
function _M.scrub(text)
    if not text or text == "" then
        return { scrubbed_text = text, redactions = 0, matched = {} }
    end

    local current_text = text
    local total_redactions = 0
    local matched_patterns = {}

    for _, p in ipairs(patterns) do
        -- lua pattern matching doesn't return count of replacements easily
        -- so we do a loop to count them accurately
        local count = 0
        local replacement = "[REDACTED:" .. p.name .. "]"

        -- gmatch check first avoids string.gsub overhead if pattern not present
        if current_text:match(p.pattern) then
            current_text, count = current_text:gsub(p.pattern, replacement)
            if count > 0 then
                total_redactions = total_redactions + count
                matched_patterns[p.name] = (matched_patterns[p.name] or 0) + count
            end
        end
    end

    return {
        scrubbed_text = current_text,
        redactions = total_redactions,
        matched = matched_patterns
    }
end

return _M
