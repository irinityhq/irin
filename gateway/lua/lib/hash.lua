-- ==========================================================================
-- lib/hash.lua — SHA-256 helpers for ledger payloads.
--
-- Used by router.lua (request bodies, cache-hit response bodies) and
-- cost.lua (outbound response bodies). The ledger stores a hex digest, never
-- the body itself, so payloads stay small and audit-walkable.
--
-- Hashing happens INSIDE timer closures, off the hot path — a 2MB body costs
-- ~5ms which we don't want to add to client-visible latency.
-- ==========================================================================

local sha256        = require "resty.sha256"
local resty_string  = require "resty.string"

local _M = {}

--- Hash a raw string body. Returns hex digest (64 chars) or "" for empty input.
-- The empty-string convention lets callers always emit a string field without
-- an `if` guard at every call site.
function _M.body_sha256_hex(body)
    if not body or body == "" then
        return ""
    end
    local h = sha256:new()
    if not h then
        return ""
    end
    h:update(body)
    local digest = h:final()
    if not digest then
        return ""
    end
    return resty_string.to_hex(digest)
end

return _M
