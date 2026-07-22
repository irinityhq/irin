-- ==========================================================================
-- lib/content.lua — multimodal-aware text extraction.
--
-- Pulled out of router.lua so other call sites (notably the council
-- translator at lua/translator.lua) can reuse the recursive descent without
-- re-implementing the depth cap.
--
-- Per spec §5.2 / §12.6 / P1 #18: over-depth returns
-- `nil, "content_depth_exceeded"` instead of silently truncating. The
-- caller is responsible for translating the error back to a 400.
--
-- Image / audio / file / tool_use parts are deliberately skipped — this
-- helper extracts SCANNABLE TEXT only (input-guard, council prompt build).
-- tool_result.content has two legal shapes per the Anthropic spec:
--   (a) string                           — plain text from the tool
--   (b) [{type:"text", text:"..."}, ...] — structured parts (mirrors
--                                          message content)
-- The recursive descent handles both. Depth cap defeats adversarial
-- nesting; 4 covers every legitimate shape we have seen.
-- ==========================================================================

local _M = {}

local MAX_CONTENT_DEPTH = 4

--- Extract concatenated text from a content field that may be a string or a
-- structured content array (multimodal / tool-call shapes). Returns
-- `(text, nil)` on success, `(nil, "content_depth_exceeded")` on over-depth,
-- or `(nil, nil)` when no scannable text is present (image-only payload,
-- empty array, etc.).
function _M.extract_text(c, depth)
    depth = depth or 0
    if depth > MAX_CONTENT_DEPTH then
        -- Hard 400 per §12.6. No silent truncate. Caller translates to error.
        return nil, "content_depth_exceeded"
    end
    if type(c) == "string" then
        return c, nil
    end
    if type(c) ~= "table" then
        return nil, nil
    end
    local parts = {}
    for _, part in ipairs(c) do
        if type(part) == "table" then
            -- OpenAI / Anthropic / Vertex all use `text` for plain text parts.
            if type(part.text) == "string" then
                parts[#parts + 1] = part.text
            elseif part.type == "tool_result" then
                local sub, err = _M.extract_text(part.content, depth + 1)
                if err then return nil, err end
                if sub then parts[#parts + 1] = sub end
            end
        end
    end
    if #parts == 0 then return nil, nil end
    return table.concat(parts, "\n\n"), nil
end

_M.MAX_CONTENT_DEPTH = MAX_CONTENT_DEPTH

return _M
