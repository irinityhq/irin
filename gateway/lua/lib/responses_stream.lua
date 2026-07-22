-- responses_stream.lua — Converts chat.completion.chunk SSE into Responses API events.
--
-- Stateful wrapper: accumulates text/tool-call deltas from chat.completion.chunk
-- frames and emits the full Responses API event sequence:
--   response.created → response.in_progress → response.output_item.added →
--   response.content_part.added → response.output_text.delta (×N) →
--   response.output_text.done → response.content_part.done →
--   response.output_item.done → response.completed
--
-- For tool calls:
--   response.output_item.added → response.function_call_arguments.delta (×N) →
--   response.function_call_arguments.done → response.output_item.done

local cjson = require "cjson.safe"

local _M = {}
local _mt = { __index = _M }

function _M.new(opts)
    local self = {
        model       = opts.model or "unknown",
        response_id = opts.response_id or ("resp_" .. tostring(ngx.now())),
        seq         = 0,
        started     = false,
        items       = {},        -- track output items for done events
        text_buf    = {},        -- accumulated text for output_text.done
        tool_bufs   = {},        -- per-item_id accumulated arguments
        current_item_id    = nil,
        current_item_type  = nil, -- "message" or "function_call"
        current_output_idx = 0,
    }
    return setmetatable(self, _mt)
end

local function sse_event(event_type, data)
    return "event: " .. event_type .. "\ndata: " .. data .. "\n\n"
end

function _M:next_seq()
    self.seq = self.seq + 1
    return self.seq
end

function _M:emit_start()
    if self.started then return "" end
    self.started = true

    local lines = {}

    -- response.created
    lines[#lines + 1] = sse_event("response.created", cjson.encode({
        type = "response.created",
        sequence_number = self:next_seq(),
        response = {
            id     = self.response_id,
            object = "response",
            status = "in_progress",
            model  = self.model,
            output = {},
        },
    }))

    -- response.in_progress
    lines[#lines + 1] = sse_event("response.in_progress", cjson.encode({
        type = "response.in_progress",
        sequence_number = self:next_seq(),
        response = {
            id     = self.response_id,
            object = "response",
            status = "in_progress",
            model  = self.model,
        },
    }))

    return table.concat(lines)
end

function _M:emit_text_item_start()
    local item_id = "msg_" .. tostring(self.current_output_idx)
    self.current_item_id   = item_id
    self.current_item_type = "message"
    self.text_buf          = {}

    self.items[#self.items + 1] = {
        id   = item_id,
        type = "message",
        output_index = self.current_output_idx,
    }

    local lines = {}

    -- response.output_item.added
    lines[#lines + 1] = sse_event("response.output_item.added", cjson.encode({
        type = "response.output_item.added",
        sequence_number = self:next_seq(),
        output_index = self.current_output_idx,
        item = {
            id     = item_id,
            type   = "message",
            role   = "assistant",
            status = "in_progress",
        },
    }))

    -- response.content_part.added
    lines[#lines + 1] = sse_event("response.content_part.added", cjson.encode({
        type = "response.content_part.added",
        sequence_number = self:next_seq(),
        item_id = item_id,
        output_index = self.current_output_idx,
        content_index = 0,
        part = { type = "output_text", text = "" },
    }))

    return table.concat(lines)
end

function _M:emit_tool_item_start(tool_call)
    local item_id = tool_call.id or ("fc_" .. tostring(self.current_output_idx))
    local fn = tool_call["function"] or {}

    self.current_output_idx = self.current_output_idx + 1
    self.current_item_id    = item_id
    self.current_item_type  = "function_call"
    self.tool_bufs[item_id] = {}

    self.items[#self.items + 1] = {
        id   = item_id,
        type = "function_call",
        name = fn.name,
        output_index = self.current_output_idx - 1,
    }

    return sse_event("response.output_item.added", cjson.encode({
        type = "response.output_item.added",
        sequence_number = self:next_seq(),
        output_index = self.current_output_idx - 1,
        item = {
            id      = item_id,
            type    = "function_call",
            call_id = tool_call.id,
            name    = fn.name,
            status  = "in_progress",
        },
    }))
end

--- Feed a decoded chat.completion.chunk object. Returns SSE lines to emit.
function _M:feed(chunk)
    if not chunk then return "" end

    local lines = {}

    -- Emit start events on first real chunk
    lines[#lines + 1] = self:emit_start()

    local choices = chunk.choices
    if not choices or #choices == 0 then
        return table.concat(lines)
    end

    local choice = choices[1]
    local delta  = choice.delta or {}

    -- Text content delta
    if delta.content and delta.content ~= "" then
        -- Start a message item if we haven't yet
        if self.current_item_type ~= "message" then
            lines[#lines + 1] = self:emit_text_item_start()
        end

        self.text_buf[#self.text_buf + 1] = delta.content

        lines[#lines + 1] = sse_event("response.output_text.delta", cjson.encode({
            type = "response.output_text.delta",
            sequence_number = self:next_seq(),
            item_id = self.current_item_id,
            output_index = self.current_output_idx,
            content_index = 0,
            delta = delta.content,
        }))
    end

    -- Tool calls
    if delta.tool_calls and type(delta.tool_calls) == "table" then
        for _, tc in ipairs(delta.tool_calls) do
            local fn = tc["function"] or {}

            -- New tool call item
            if fn.name then
                -- Close any open text item first
                if self.current_item_type == "message" then
                    lines[#lines + 1] = self:close_text_item()
                end
                lines[#lines + 1] = self:emit_tool_item_start(tc)
            end

            -- Arguments delta
            if fn.arguments and fn.arguments ~= "" then
                local item_id = tc.id or self.current_item_id
                if self.tool_bufs[item_id] then
                    self.tool_bufs[item_id][#self.tool_bufs[item_id] + 1] = fn.arguments
                end

                local out_idx = 0
                for _, it in ipairs(self.items) do
                    if it.id == item_id then
                        out_idx = it.output_index
                        break
                    end
                end

                lines[#lines + 1] = sse_event("response.function_call_arguments.delta", cjson.encode({
                    type = "response.function_call_arguments.delta",
                    sequence_number = self:next_seq(),
                    item_id = item_id,
                    output_index = out_idx,
                    delta = fn.arguments,
                }))
            end
        end
    end

    -- finish_reason present → item is done
    if choice.finish_reason then
        if self.current_item_type == "message" then
            lines[#lines + 1] = self:close_text_item()
        elseif self.current_item_type == "function_call" then
            lines[#lines + 1] = self:close_tool_item()
        end
    end

    return table.concat(lines)
end

function _M:close_text_item()
    if not self.current_item_id then return "" end

    local full_text = table.concat(self.text_buf)
    local item_id   = self.current_item_id
    local out_idx   = self.current_output_idx

    local lines = {}

    -- response.output_text.done
    lines[#lines + 1] = sse_event("response.output_text.done", cjson.encode({
        type = "response.output_text.done",
        sequence_number = self:next_seq(),
        item_id = item_id,
        output_index = out_idx,
        content_index = 0,
        text = full_text,
    }))

    -- response.content_part.done
    lines[#lines + 1] = sse_event("response.content_part.done", cjson.encode({
        type = "response.content_part.done",
        sequence_number = self:next_seq(),
        item_id = item_id,
        output_index = out_idx,
        content_index = 0,
        part = { type = "output_text", text = full_text },
    }))

    -- response.output_item.done
    lines[#lines + 1] = sse_event("response.output_item.done", cjson.encode({
        type = "response.output_item.done",
        sequence_number = self:next_seq(),
        output_index = out_idx,
        item = {
            id     = item_id,
            type   = "message",
            role   = "assistant",
            status = "completed",
            content = {{
                type = "output_text",
                text = full_text,
            }},
        },
    }))

    -- Store final content for response.completed output[]
    for _, it in ipairs(self.items) do
        if it.id == item_id then
            it.content = {{ type = "output_text", text = full_text }}
            it.role = "assistant"
            it.status = "completed"
            break
        end
    end

    self.current_item_type = nil
    self.current_output_idx = self.current_output_idx + 1
    return table.concat(lines)
end

function _M:close_tool_item()
    if not self.current_item_id then return "" end

    local item_id  = self.current_item_id
    local out_idx  = 0
    local name     = nil

    for _, it in ipairs(self.items) do
        if it.id == item_id then
            out_idx = it.output_index
            name    = it.name
            break
        end
    end

    local full_args = ""
    if self.tool_bufs[item_id] then
        full_args = table.concat(self.tool_bufs[item_id])
        self.tool_bufs[item_id] = nil
    end

    local lines = {}

    -- response.function_call_arguments.done
    lines[#lines + 1] = sse_event("response.function_call_arguments.done", cjson.encode({
        type = "response.function_call_arguments.done",
        sequence_number = self:next_seq(),
        item_id = item_id,
        output_index = out_idx,
        call_id = item_id,
        name = name,
        arguments = full_args,
    }))

    -- response.output_item.done
    lines[#lines + 1] = sse_event("response.output_item.done", cjson.encode({
        type = "response.output_item.done",
        sequence_number = self:next_seq(),
        output_index = out_idx,
        item = {
            id        = item_id,
            type      = "function_call",
            call_id   = item_id,
            name      = name,
            arguments = full_args,
            status    = "completed",
        },
    }))

    -- Store final arguments for response.completed output[]
    for _, it in ipairs(self.items) do
        if it.id == item_id then
            it.arguments = full_args
            it.call_id = item_id
            it.status = "completed"
            break
        end
    end

    self.current_item_type = nil
    return table.concat(lines)
end

--- Emit the response.completed event with usage. Call at EOF.
function _M:finish(usage)
    local lines = {}

    -- Close any open items that didn't get a finish_reason
    if self.current_item_type == "message" then
        lines[#lines + 1] = self:close_text_item()
    elseif self.current_item_type == "function_call" then
        lines[#lines + 1] = self:close_tool_item()
    end

    -- Emit start if we never got any chunks (empty stream)
    lines[#lines + 1] = self:emit_start()

    local resp_usage = nil
    if usage then
        resp_usage = {
            input_tokens  = usage.prompt_tokens or usage.input_tokens or 0,
            output_tokens = usage.completion_tokens or usage.output_tokens or 0,
            total_tokens  = usage.total_tokens
                or ((usage.prompt_tokens or usage.input_tokens or 0)
                    + (usage.completion_tokens or usage.output_tokens or 0)),
        }
    end

    -- Build output[] from accumulated items
    local output = {}
    for _, it in ipairs(self.items) do
        if it.type == "message" then
            output[#output + 1] = {
                id      = it.id,
                type    = "message",
                role    = it.role or "assistant",
                status  = it.status or "completed",
                content = it.content or {},
            }
        elseif it.type == "function_call" then
            output[#output + 1] = {
                id        = it.id,
                type      = "function_call",
                call_id   = it.call_id or it.id,
                name      = it.name,
                arguments = it.arguments or "{}",
                status    = it.status or "completed",
            }
        end
    end

    lines[#lines + 1] = sse_event("response.completed", cjson.encode({
        type = "response.completed",
        sequence_number = self:next_seq(),
        response = {
            id     = self.response_id,
            object = "response",
            status = "completed",
            model  = self.model,
            output = output,
            usage  = resp_usage,
        },
    }))

    return table.concat(lines)
end

return _M
