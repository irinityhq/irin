-- Exact Council transport identity.
--
-- Canonical transport IDs name one concrete provider transport. Legacy IDs
-- are retained only as an explicit compatibility layer for older cabinets;
-- unsupported local OAuth adapters fail closed because Gateway has no matching
-- upstream adapter today.

local _M = {}

local EXACT = {
    grok_api      = "xai",
    claude_api    = "anthropic",
    claude_code   = "claude-cli",
    codex_cli     = "gpt-cli",
    openai_api    = "openai",
    gemini_vertex = "vertex",
    gemini_cli    = "gemini-cli",
}

local UNSUPPORTED = {
    grok_build  = true,
    grok_hermes = true,
    gemini_agy  = true,
}

-- Legacy names describe the behavior Gateway historically supplied for those
-- IDs. They are deliberately listed instead of being normalized heuristically.
local LEGACY = {
    grok       = { xai = true },
    grok_cli   = { xai = true },
    hermes_cli = { xai = true },
    claude     = { ["claude-cli"] = true, anthropic = true },
    gpt        = { ["gpt-cli"] = true, openai = true },
    gemini     = { vertex = true, ["gemini-cli"] = true },
    agy_cli    = { vertex = true },
    nim        = { nvidia = true },
}

function _M.is_trusted_council(service_role, key_id, pinned_key_id)
    return service_role == "council"
        and type(pinned_key_id) == "string"
        and pinned_key_id ~= ""
        and key_id == pinned_key_id
end

function _M.is_local_provider(provider_name)
    return provider_name == "claude-cli"
        or provider_name == "gpt-cli"
        or provider_name == "gemini-cli"
        or provider_name == "local"
        or provider_name == "council"
end

function _M.matches(transport_id, provider_name)
    if type(transport_id) ~= "string" or transport_id == "" then
        return false, "transport_required"
    end
    if UNSUPPORTED[transport_id] then
        return false, "transport_unsupported"
    end
    local expected = EXACT[transport_id]
    if expected then
        if provider_name == expected then return true end
        return false, "transport_provider_mismatch"
    end
    local allowed = LEGACY[transport_id]
    if allowed then
        if allowed[provider_name] then return true end
        return false, "legacy_transport_provider_mismatch"
    end
    -- Existing Gateway-native provider IDs (nvidia, local, council, etc.)
    -- remain valid without broadening them to a different provider.
    if transport_id == provider_name then return true end
    return false, "transport_unknown"
end

function _M.advertised_for_provider(provider_name)
    local ids = {}
    for transport_id, expected in pairs(EXACT) do
        if expected == provider_name then ids[#ids + 1] = transport_id end
    end
    for transport_id, allowed in pairs(LEGACY) do
        if allowed[provider_name] then ids[#ids + 1] = transport_id end
    end
    -- Gateway-native providers preserve their exact identity.
    if not UNSUPPORTED[provider_name] then ids[#ids + 1] = provider_name end
    table.sort(ids)
    return ids
end

return _M
