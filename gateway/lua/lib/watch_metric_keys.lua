-- Length-prefixed key encoding + Prometheus label escaping for watch metrics.
--
-- Pure string helpers (no ngx). Used by lua/cost.lua poller/render so hostile
-- tenant/sentinel names with ":" or quote characters cannot mis-split keys
-- or corrupt the /metrics exposition.

local _M = {}

--- Encode zero or more string parts as length-prefixed segments: <n>:<bytes>...
function _M.lp_encode(parts)
    local chunks = {}
    for i = 1, #parts do
        local p = parts[i]
        if type(p) ~= "string" then
            return nil
        end
        chunks[#chunks + 1] = string.format("%d:%s", #p, p)
    end
    return table.concat(chunks, "")
end

--- Decode a full length-prefixed blob into a list of strings, or nil on error.
function _M.lp_decode_all(s)
    if type(s) ~= "string" then
        return nil
    end
    local out = {}
    local i = 1
    local n = #s
    while i <= n do
        local colon = s:find(":", i, true)
        if not colon then
            return nil
        end
        local len = tonumber(s:sub(i, colon - 1))
        if not len or len < 0 or len ~= math.floor(len) then
            return nil
        end
        local start = colon + 1
        local finish = start + len - 1
        if finish > n then
            return nil
        end
        out[#out + 1] = s:sub(start, finish)
        i = finish + 1
    end
    return out
end

--- Escape a Prometheus label value: \, ", and newline.
function _M.prom_escape_label(s)
    if type(s) ~= "string" then
        return ""
    end
    return (s:gsub("\\", "\\\\"):gsub("\"", "\\\""):gsub("\n", "\\n"))
end

--- True when key is a sticky labeled watch series we must clear each poll.
function _M.is_labeled_watch_key(key)
    if type(key) ~= "string" then
        return false
    end
    return key:find("^watch_temperature:", 1) ~= nil
        or key:find("^watch_sentinel_fires:", 1) ~= nil
        or key:find("^watch_sentinel_last_tick_ms:", 1) ~= nil
        or key:find("^watch_sentinel_ticks:", 1) ~= nil
end

return _M
