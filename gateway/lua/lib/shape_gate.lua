-- ==========================================================================
-- shape_gate.lua — Structural payload validation
--
-- Enforces configurable limits on request structure (messages array length,
-- string size, tools count) before hitting the Rust decontaminator.
-- This prevents oversized payloads from wasting compute or triggering
-- excessive regex processing times.
-- ==========================================================================

local _M = {}
local config = require("config")

--- Validates a request against shape limits
-- @param req table Decoded request JSON
-- @param model_id string The resolved model ID
-- @param raw_body_len number Length of the raw body in bytes
-- @return table|nil nil on pass, {error="...", field="...", limit=N, actual=N} on violation
function _M.validate(req, model_id, raw_body_len)
    if not req or type(req) ~= "table" then
        return nil
    end

    local limits = config.shape_limits or { default = {} }
    local model_limits = (limits.models and limits.models[model_id]) or {}
    local default_limits = limits.default or {}

    local function get_limit(key)
        return model_limits[key] or default_limits[key]
    end

    -- 1. Total body size
    local max_total_body_bytes = get_limit("max_total_body_bytes")
    if max_total_body_bytes and raw_body_len and raw_body_len > max_total_body_bytes then
        return {
            error = string.format("request exceeds structural limit: total body size %d > max %d", raw_body_len, max_total_body_bytes),
            field = "total_body_bytes",
            limit = max_total_body_bytes,
            actual = raw_body_len
        }
    end

    -- 2. Messages array length
    local max_messages = get_limit("max_messages")
    if max_messages and req.messages and type(req.messages) == "table" then
        local count = #req.messages
        if count > max_messages then
            return {
                error = string.format("request exceeds structural limit: messages[] count %d > max %d", count, max_messages),
                field = "messages",
                limit = max_messages,
                actual = count
            }
        end

        -- 3. Per-message content size
        local max_message_bytes = get_limit("max_message_bytes")
        if max_message_bytes then
            for i, msg in ipairs(req.messages) do
                if type(msg.content) == "string" then
                    if #msg.content > max_message_bytes then
                        return {
                            error = string.format("request exceeds structural limit: messages[%d].content size %d > max %d", i, #msg.content, max_message_bytes),
                            field = "message_bytes",
                            limit = max_message_bytes,
                            actual = #msg.content
                        }
                    end
                elseif type(msg.content) == "table" then
                    -- Rough estimate for complex contents (vision, etc)
                    -- For accurate bytes we'd need to re-encode, but we don't want to burn compute here.
                    -- If it's a table, we trust the total_body_bytes check to catch gross oversized payloads.
                end
            end
        end
    end

    -- 4. Tools count
    local max_tools = get_limit("max_tools")
    if max_tools and req.tools and type(req.tools) == "table" then
        local count = #req.tools
        if count > max_tools then
            return {
                error = string.format("request exceeds structural limit: tools[] count %d > max %d", count, max_tools),
                field = "tools",
                limit = max_tools,
                actual = count
            }
        end
    end

    return nil
end

return _M
