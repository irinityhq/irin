-- Negative fixture (PR #19): nested metadata.raw_key must NOT satisfy
-- the top-level raw_key contract for /auth/check.
local function nested_raw_key_false_negative()
  return sidecar_post("/auth/check", {
    metadata = { raw_key = value },
    ip = client_ip,
  })
end
