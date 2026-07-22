-- ==========================================================================
-- lib/sse.lua — Stateful Server-Sent Events (SSE) frame parser.
--
-- Handles the full SSE spec relevant to LLM provider streams:
--   - Named events: `event: <name>`
--   - Data lines: `data: <json>`
--   - Multi-line data continuations (joined with \n)
--   - CRLF normalization (\r\n → \n)
--   - SSE comments (`: ...`) — logged in strict mode, always skipped
--   - `retry:` lines — ignored
--   - `[DONE]` sentinel detection
--   - Heartbeat / empty-line handling
--   - Truncated/invalid JSON resilience
--
-- This replaces the ad-hoc `sse_line_buf` accumulation pattern in cost.lua
-- with a proper stateful parser that survives TCP fragmentation across
-- body_filter invocations.
--
-- Usage:
--   local sse = require("lib.sse")
--   local parser = sse.new({ strict = true })
--   local frames = parser:feed(chunk)
--   -- frames: array of { event = string|nil, data = string, done = bool }
-- ==========================================================================

local _M = {}
local _mt = { __index = _M }

-- Max buffer size to prevent unbounded growth from a malicious/broken upstream
local MAX_BUF_SIZE = 262144 -- 256KB

--- Create a new SSE parser instance.
-- @param opts table|nil  Options: { strict = bool }
--   strict: if true, log malformed frames and unexpected lines
-- @return parser instance
function _M.new(opts)
    opts = opts or {}
    return setmetatable({
        _buf = "",
        _strict = opts.strict or false,
        _event_name = nil,
        _data_lines = {},
    }, _mt)
end

--- Reset the in-progress frame state (event name + data lines).
local function reset_frame(self)
    self._event_name = nil
    self._data_lines = {}
end

--- Dispatch a completed frame from accumulated state.
-- Returns a frame table or nil if no data was accumulated.
local function dispatch_frame(self)
    if #self._data_lines == 0 then
        reset_frame(self)
        return nil
    end

    local raw_data = table.concat(self._data_lines, "\n")
    local is_done = (raw_data == "[DONE]")

    local frame = {
        event = self._event_name,
        data  = raw_data,
        done  = is_done,
    }

    reset_frame(self)
    return frame
end

--- Process a single SSE line.
-- SSE spec: lines starting with "data:", "event:", "id:", "retry:", or ":"
-- An empty line dispatches the accumulated event.
-- @return frame table or nil
local function process_line(self, line)
    -- Empty line = end of event (dispatch)
    if line == "" then
        return dispatch_frame(self)
    end

    -- SSE comment (starts with ":")
    if line:sub(1, 1) == ":" then
        if self._strict then
            ngx.log(ngx.DEBUG, "sse: comment: ", line:sub(2))
        end
        return nil
    end

    -- Parse "field: value" or "field:value" (space after colon is optional per spec)
    local field, value = line:match("^([^:]+):%s?(.*)")
    if not field then
        -- Line with no colon — spec says treat entire line as field name, ignore
        if self._strict then
            -- Log length only, never raw content: this fires before the
            -- credential scrubber runs (T24 redaction).
            ngx.log(ngx.WARN, "sse: malformed line (no colon), length ", #line)
        end
        return nil
    end

    if field == "data" then
        self._data_lines[#self._data_lines + 1] = value
    elseif field == "event" then
        self._event_name = value
    elseif field == "id" then
        -- SSE spec: last event ID. Not used by LLM providers, ignore.
        if self._strict then
            ngx.log(ngx.DEBUG, "sse: id field: ", value)
        end
    elseif field == "retry" then
        -- SSE spec: reconnection time. Not relevant for proxy. Ignore.
        if self._strict then
            ngx.log(ngx.DEBUG, "sse: retry field: ", value)
        end
    else
        if self._strict then
            -- Field name is a protocol token (safe); the value may carry
            -- unscrubbed upstream content, so log its length only (T24 redaction).
            ngx.log(ngx.WARN, "sse: unknown field '", field, "' (value length ", #value, ")")
        end
    end

    return nil
end

--- Feed a chunk of raw bytes into the parser.
-- Returns an array of completed frames. May return an empty array if the
-- chunk did not complete any frames (e.g., TCP fragment mid-line).
--
-- @param chunk string  Raw bytes from the upstream SSE stream
-- @return table  Array of frame tables: { event, data, done }
function _M.feed(self, chunk)
    if not chunk or chunk == "" then
        return {}
    end

    -- Normalize CRLF → LF
    chunk = chunk:gsub("\r\n", "\n"):gsub("\r", "\n")

    -- Append to buffer
    self._buf = self._buf .. chunk

    -- Safety: prevent unbounded buffer growth
    if #self._buf > MAX_BUF_SIZE then
        ngx.log(ngx.WARN, "sse: buffer exceeded ", MAX_BUF_SIZE,
                " bytes — flushing incomplete data")
        -- Try to salvage: flush everything as if it ended
        local salvaged = dispatch_frame(self)
        self._buf = ""
        if salvaged then return { salvaged } end
        return {}
    end

    local frames = {}

    -- Process all complete lines (terminated by \n).
    -- Incomplete trailing data stays in _buf for the next feed().
    while true do
        local nl_pos = self._buf:find("\n", 1, true)
        if not nl_pos then
            break
        end

        local line = self._buf:sub(1, nl_pos - 1)
        self._buf = self._buf:sub(nl_pos + 1)

        local frame = process_line(self, line)
        if frame then
            frames[#frames + 1] = frame
        end
    end

    return frames
end

--- Flush any remaining buffered data.
-- Call at EOF to dispatch any incomplete frame (e.g., upstream closed
-- without a trailing empty line).
-- @return table  Array of frame tables (0 or 1 elements)
function _M.flush(self)
    local frames = {}

    -- Process any remaining partial line in the buffer
    if self._buf ~= "" then
        process_line(self, self._buf)
        self._buf = ""
    end

    -- Dispatch any accumulated but un-dispatched frame
    local frame = dispatch_frame(self)
    if frame then
        frames[#frames + 1] = frame
    end

    return frames
end

return _M
