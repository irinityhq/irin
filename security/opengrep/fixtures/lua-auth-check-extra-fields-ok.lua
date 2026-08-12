-- Positive fixture: required top-level keys plus an unrelated extra field
-- must remain clean (exact one/two-field exclusions were a false red).
local function ok_auth_check_with_extra(raw_key, ip, request_id)
  return sidecar_post("/auth/check", {
    raw_key = raw_key,
    ip = ip,
    request_id = request_id,
  })
end
