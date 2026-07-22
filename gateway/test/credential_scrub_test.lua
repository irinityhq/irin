-- ==========================================================================
-- credential_scrub_test.lua — regression tripwire for lua/lib/credential_scrub.lua
--
-- WHY: credential_scrub is the ONLY guard stopping an upstream-leaked secret
-- from being persisted into the ledger hash / response cache / audit export
-- (call sites: lua/cost.lua body_filter, 5x). Without this regression test, a
-- broken matcher would leak a credential into a signed ledger row and pass CI
-- green. This is that missing tripwire.
--
-- DISCIPLINE: asserts the SECURITY INVARIANT (no raw secret survives) and the
-- FALSE-POSITIVE INVARIANT (clean text untouched). It deliberately does NOT
-- assert per-matcher attribution counts, because matcher ORDER shadows names
-- (openai_key `sk-` runs before anthropic_key `sk-ant-`, so `sk-ant-...`
-- redacts as openai_key). Attribution is fragile; non-leakage is the contract.
--
-- RUN: `lua test/credential_scrub_test.lua` from the gateway repo root
--      (or `make lua-unit`). No nginx/openresty runtime needed — _M.scrub
--      takes a plain string and has no ngx.* dependency.
-- ==========================================================================

package.path = "./?.lua;" .. package.path
local scrub = require("lua.lib.credential_scrub").scrub

local failures = 0
local function check(cond, msg)
    if cond then
        print("  ok   - " .. msg)
    else
        failures = failures + 1
        print("  FAIL - " .. msg)
    end
end

-- Secret-shaped fixtures (synthetic; not real credentials).
local SECRETS = {
    { name = "aws_access_key", raw = "AKIAIOSFODNN7EXAMPLE" },
    { name = "private_key",    raw = "-----BEGIN RSA PRIVATE KEY-----\nMIIBfake==\n-----END RSA PRIVATE KEY-----" },
    { name = "slack_token",    raw = "xoxb-123456789012-abcdEFGHijklMNOP" },
    { name = "github_pat",     raw = "ghp_FAKEtestkey0000" },
    { name = "github_pat_v2",  raw = "github_pat_11ABCDEFG0aBcDeFgHiJkL_mnopQRStuv" },
    { name = "openai_key",     raw = "sk-abcdEFGH1234567890abcdEFGH" },
    { name = "anthropic_key",  raw = "sk-ant-api03-abcdEFGH1234567890" },
    { name = "gitlab_pat",     raw = "glpat-FAKEtestkey00" },
    { name = "gcp_api_key",    raw = "AIzaSyABCDEFGHIJKLMNOPQRSTUVWXYZ012345678" },
    { name = "xai_key",        raw = "xai-abcdEFGH1234567890abcdEFGH" },
    { name = "nvidia_key",     raw = "nvapi-abcdEFGH1234567890abcdEFGH" },
    { name = "google_oauth",   raw = "ya29.a0AfH6SMBexampleTokenValue123" },
}

-- 1. SECURITY INVARIANT: every known secret shape is redacted out of the body.
--    The raw secret substring must NOT survive, and a redaction must be counted.
print("[1] security invariant — no raw secret survives")
for _, s in ipairs(SECRETS) do
    local body = "prefix " .. s.raw .. " suffix"
    local r = scrub(body)
    -- A distinctive core of the secret must be gone (skip the common `sk-`/AIza
    -- prefix; assert on the high-entropy tail that only the raw secret carries).
    local tail = s.raw:sub(-8)
    check(not r.scrubbed_text:find(tail, 1, true),
          s.name .. ": raw secret tail not present in scrubbed output")
    check(r.redactions >= 1, s.name .. ": at least one redaction counted")
end

-- 2. FALSE-POSITIVE INVARIANT: clean text is returned byte-identical, 0 redactions.
print("[2] false-positive invariant — clean text untouched")
for _, clean in ipairs({
    "the quick brown fox jumps over the lazy dog",
    "GET /v1/chat/completions HTTP/1.1",
    '{"model":"council-triage","messages":[{"role":"user","content":"hi"}]}',
    "",
}) do
    local r = scrub(clean)
    check(r.scrubbed_text == clean and r.redactions == 0,
          "clean stays clean: " .. (clean == "" and "<empty>" or clean:sub(1, 32)))
end

-- 3. MULTI-SECRET: several secrets in one body are all removed.
print("[3] multiple secrets in one body")
do
    local body = "k AKIAIOSFODNN7EXAMPLE then ghp_FAKEtestkey0000 done"
    local r = scrub(body)
    check(not r.scrubbed_text:find("AKIAIOSFODNN7EXAMPLE", 1, true), "aws key removed")
    check(not r.scrubbed_text:find("ghp_FAKE", 1, true), "github pat removed")
    check(r.redactions >= 2, "both secrets counted (got " .. r.redactions .. ")")
end

-- 4. KNOWN-SHADOWING REGRESSION GUARD: sk-ant- is currently redacted (as
--    openai_key, due to matcher order). We assert it is REDACTED — not which
--    name wins — so a future re-order that accidentally stops redacting it
--    trips this. If you intentionally fix attribution, update this assert.
print("[4] sk-ant- shadowing — redacted regardless of attribution")
do
    local r = scrub("auth sk-ant-api03-SECRETtail12345 end")
    check(not r.scrubbed_text:find("SECRETtail12345", 1, true),
          "anthropic-style key redacted despite sk-/sk-ant- shadowing")
end

print("")
if failures == 0 then
    print("PASS credential_scrub (" .. #SECRETS .. " secret shapes + clean + multi + shadow)")
    os.exit(0)
else
    print("FAIL credential_scrub — " .. failures .. " assertion(s) failed")
    os.exit(1)
end
