-- Negative fixture: key name only inside a string literal must still fire
-- the missing top-level raw_key rule (lexical string skip).
local function string_only_raw_key()
  return sidecar_post("/auth/check", {
    note = "raw_key = fake",
    ip = client_ip,
  })
end
