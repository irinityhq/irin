package.path = "./?.lua;" .. package.path

local transport = require("lua.lib.council_transport")
local failures = 0
local function check(cond, msg)
    if cond then
        print("  ok   - " .. msg)
    else
        failures = failures + 1
        print("  FAIL - " .. msg)
    end
end

check(transport.is_trusted_council("council", "key-1", "key-1"),
      "transport metadata accepts the pinned Council identity")
check(not transport.is_trusted_council("user", "key-1", "key-1"),
      "transport metadata rejects a non-Council role")
check(not transport.is_trusted_council("council", "key-2", "key-1"),
      "transport metadata rejects a different Council key")
check(not transport.is_trusted_council("council", "key-1", ""),
      "transport metadata rejects an unpinned Council key")
check(transport.is_local_provider("claude-cli"),
      "Sovereign mode recognizes a local CLI adapter")
check(not transport.is_local_provider("anthropic"),
      "Sovereign mode rejects an external API adapter")

local exact = {
    grok_api = "xai",
    claude_api = "anthropic",
    claude_code = "claude-cli",
    codex_cli = "gpt-cli",
    openai_api = "openai",
    gemini_vertex = "vertex",
    gemini_cli = "gemini-cli",
}
for id, provider in pairs(exact) do
    check(transport.matches(id, provider), id .. " matches only its concrete provider")
    check(not transport.matches(id, "different-provider"), id .. " rejects provider drift")
end

for _, id in ipairs({ "grok_build", "grok_hermes", "gemini_agy" }) do
    local ok, reason = transport.matches(id, "xai")
    check(not ok and reason == "transport_unsupported", id .. " fails closed without an adapter")
end

check(transport.matches("grok_cli", "xai"), "legacy grok_cli keeps governed compatibility")
check(transport.matches("claude", "claude-cli"), "legacy claude accepts historical CLI route")
check(transport.matches("claude", "anthropic"), "legacy claude accepts historical API route")
check(transport.matches("nvidia", "nvidia"), "Gateway-native transport remains exact")
check(not transport.matches("nvidia", "xai"), "Gateway-native transport cannot drift")

local xai_ids = table.concat(transport.advertised_for_provider("xai"), ",")
check(xai_ids:find("grok_api", 1, true) ~= nil, "catalog advertises canonical xAI transport")
check(xai_ids:find("grok_build", 1, true) == nil, "catalog omits unsupported Grok Build adapter")

if failures > 0 then
    io.stderr:write(string.format("\n%d council transport test(s) failed\n", failures))
    os.exit(1)
end
print("\nOK: exact Council transport mapping")
