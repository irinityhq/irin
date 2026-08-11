-- Negative fixture: key name only in a Lua line comment must still fire
-- the missing top-level raw_key rule (lexical comment skip).
local function comment_only_raw_key()
  return sidecar_post("/auth/check", {
    -- raw_key = not_a_real_field
    ip = client_ip,
  })
end
