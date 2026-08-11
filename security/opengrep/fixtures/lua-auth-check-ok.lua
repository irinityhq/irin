-- Positive fixture: production-shaped top-level raw_key + ip.
local function ok_auth_check(raw_key, ip)
  return sidecar_post("/auth/check", {
    raw_key = raw_key,
    ip = ip,
  })
end
