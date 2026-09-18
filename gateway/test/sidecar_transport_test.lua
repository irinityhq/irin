-- sidecar_transport_test.lua — wire-mechanics characterization for the
-- sidecar.lua transport unification (simplification series PR2).
--
-- Every sidecar.lua HTTP exchange goes through one shared helper. This
-- suite pins the observable mechanics each endpoint must keep: pool names,
-- timeouts, UDS vs TCP connect targets, exact forwarded headers, query
-- string preservation, failure shapes (API error strings vs proxy 502 JSON
-- vs forwarded upstream status on body-read failure), and connection
-- disposal (keepalive parameters vs close).
--
-- Run from gateway/: `make lua-unit`.
package.path = "./?.lua;./lua/?.lua;" .. package.path

local failures = 0
local function check(cond, msg)
    if cond then print("  ok   - " .. msg)
    else failures = failures + 1; print("  FAIL - " .. msg) end
end
local function eq(actual, expected, msg)
    check(actual == expected,
        msg .. " (expected " .. tostring(expected) .. ", got " .. tostring(actual) .. ")")
end
local function contains(hay, needle, msg)
    check(type(hay) == "string" and hay:find(needle, 1, true) ~= nil, msg)
end

-- ---- scriptable fakes ----------------------------------------------------

local script -- outcome knobs for the current exchange
local conns  -- one record per http.new()

local function reset_transport(opts)
    script = opts or {}
    conns = {}
end

package.preload["resty.http"] = function()
    return {
        new = function()
            local rec = { timeout = nil, connect = nil, request = nil,
                          keepalive = nil, closed = false }
            table.insert(conns, rec)
            return {
                set_timeout = function(_, ms) rec.timeout = ms end,
                connect = function(_, t)
                    rec.connect = t
                    if script.connect_ok == false then
                        return false, script.connect_err or "connection refused"
                    end
                    return true
                end,
                request = function(_, t)
                    rec.request = t
                    if script.request_ok == false then
                        return nil, script.request_err or "send failed"
                    end
                    local res = {
                        status = (script.res and script.res.status) or 200,
                        headers = (script.res and script.res.headers) or {},
                    }
                    function res:read_body()
                        if script.body_ok == false then
                            return nil, script.body_err or "truncated"
                        end
                        return (script.res and script.res.body) or ""
                    end
                    return res
                end,
                set_keepalive = function(_, idle, size)
                    rec.keepalive = { idle, size }
                end,
                close = function(_) rec.closed = true end,
            }
        end,
    }
end

-- Shallow deterministic encode so emitted JSON bodies stay assertable.
package.preload["cjson.safe"] = function()
    return {
        encode = function(v)
            if type(v) == "string" then return v end
            local parts = {}
            for k, val in pairs(v) do
                parts[#parts + 1] = tostring(k) .. "=" .. tostring(val)
            end
            table.sort(parts)
            return "{" .. table.concat(parts, ",") .. "}"
        end,
        decode = function(raw)
            if script.decode_result ~= nil then
                return script.decode_result
            end
            if script.decode_err ~= nil then
                return nil, script.decode_err
            end
            if type(raw) == "string" and raw:sub(1, 1) == "{" then
                return { recorded = true }
            end
            return nil, "unexpected decode"
        end,
    }
end

local ngx_out = { said = {}, printed = {} }
local ngx_req = { method = "GET", body = nil, headers = {} }
local ngx_var = { uri = "/", args = nil, request_id = "req-test" }

local function reset_ngx()
    ngx_out.said = {}
    ngx_out.printed = {}
    _G.ngx.status = nil
    _G.ngx.header = {}
end

_G.ngx = {
    log = function() end,
    INFO = 1, WARN = 2, ERR = 3,
    status = nil,
    header = {},
    say = function(...)
        table.insert(ngx_out.said, table.concat({ ... }, " "))
    end,
    print = function(...)
        table.insert(ngx_out.printed, table.concat({ ... }, " "))
    end,
    req = {
        read_body = function() end,
        get_body_data = function() return ngx_req.body end,
        get_body_file = function() return nil end,
        get_method = function() return ngx_req.method end,
        get_headers = function() return ngx_req.headers end,
    },
    var = ngx_var,
}

local function set_request(opts)
    ngx_req.method = opts.method or "GET"
    ngx_req.body = opts.body
    ngx_req.headers = opts.headers or {}
    ngx_var.uri = opts.uri or "/"
    ngx_var.args = opts.args
    ngx_var.request_id = opts.request_id or "req-test"
end

-- Force env values for sidecar.init(); false pins a variable to nil.
local real_getenv = os.getenv
local function with_env(env, fn)
    os.getenv = function(k)
        local v = env[k]
        if v ~= nil then
            if v == false then return nil end
            return v
        end
        return real_getenv(k)
    end
    local ok, err = pcall(fn)
    os.getenv = real_getenv
    if not ok then error(err, 0) end
end

-- Mount/unmount the arm-bridge marker file.
local real_io_open = io.open
local function with_arm_marker(enabled, fn)
    io.open = function(path, mode)
        if path == "/run/irin-features/arm-bridge-enabled" then
            if not enabled then return nil end
            return { close = function() end }
        end
        return real_io_open(path, mode)
    end
    local ok, err = pcall(fn)
    io.open = real_io_open
    if not ok then error(err, 0) end
end

local sidecar = require("sidecar")

local function init_clean()
    with_env({
        SIDECAR_ADDR = false, SIDECAR_TIMEOUT_MS = false,
        LEDGER_ADMIN_KEY = false, ADMIN_KEY = false,
    }, function() sidecar.init() end)
end

local HOT_ADDR = "unix:/tmp/gateway-sidecar.sock:"
local function pool(name)
    if name == "" then return "sidecar:" .. HOT_ADDR end
    return "sidecar:" .. name .. ":" .. HOT_ADDR
end

-- ---- sidecar_post / hot path ---------------------------------------------

print("sidecar_post: success mechanics")
reset_transport{ res = { status = 200, body = '{"x":1}' }, decode_result = { ok = true } }
reset_ngx()
local r, e, st = sidecar.cache_check("m", "raw", 3)
eq(e, nil, "success returns no error")
eq(st, 200, "third return is the HTTP status")
eq(conns[1].timeout, 50, "hot-path timeout is the default SIDECAR_TIMEOUT_MS")
eq(conns[1].connect.pool, pool(""), "hot-path pool name keys on the address")
eq(conns[1].connect.host, "unix:/tmp/gateway-sidecar.sock",
    "UDS address connects with the trailing colon stripped")
eq(conns[1].request.method, "POST", "sidecar_post sends POST")
eq(conns[1].request.path, "/cache/check", "request path is the endpoint path")
eq(conns[1].request.headers["Host"], "sidecar.local", "Host header from the UDS address")
eq(conns[1].request.headers["Content-Type"], "application/json", "JSON content type")
eq(conns[1].request.headers["Content-Length"], tostring(#conns[1].request.body),
    "Content-Length matches the encoded body")
check(conns[1].keepalive ~= nil and conns[1].keepalive[1] == 60000
    and conns[1].keepalive[2] == 10, "hot path keepalive is (60s, 10)")
check(not conns[1].closed, "hot path pools the connection instead of closing")

print("sidecar_post: failure shapes")
reset_transport{ connect_ok = false, connect_err = "connection refused" }
r, e = sidecar.guard_sovereignty("content")
eq(r, nil, "connect failure returns no result")
eq(e, "sidecar unreachable: connection refused", "connect failure text")

reset_transport{ request_ok = false, request_err = "send failed" }
r, e = sidecar.guard_sovereignty("content")
eq(e, "sidecar request failed: send failed", "request failure text")
check(conns[1].closed, "request failure closes the connection")
check(conns[1].keepalive == nil, "request failure does not pool")

reset_transport{ body_ok = false, body_err = "truncated" }
r, e = sidecar.guard_sovereignty("content")
eq(e, "sidecar body read failed: truncated", "body-read failure text")
check(conns[1].closed, "body-read failure closes the connection")

reset_transport{ res = { status = 200, body = "not-json" }, decode_err = "bad json" }
r, e = sidecar.guard_sovereignty("content")
eq(e, "sidecar response parse error: bad json", "decode failure text")

-- ---- ledger_record -------------------------------------------------------

print("ledger_record: X-Admin-Key contract")
reset_transport{}
r, e = sidecar.ledger_record("gw", "m", {}, {})
eq(e, "ledger admin key not configured (set LEDGER_ADMIN_KEY or ADMIN_KEY)",
    "missing admin key fails closed before transport")
eq(#conns, 0, "no transport without the admin key")

with_env({ LEDGER_ADMIN_KEY = "admin-k" }, function() sidecar.init() end)
reset_transport{ res = { status = 200 }, decode_result = { recorded = true } }
r, e = sidecar.ledger_record("gw", "m", {}, {})
eq(e, nil, "recorded=true 200 is success")
eq(conns[1].request.headers["X-Admin-Key"], "admin-k",
    "ledger_record forwards the configured admin key")

reset_transport{ res = { status = 401 }, decode_result = { error = "denied" } }
r, e = sidecar.ledger_record("gw", "m", {}, {})
eq(e, "ledger/record HTTP 401: denied", "non-2xx is failure even with a decoded body")
init_clean()

-- ---- vertex_token --------------------------------------------------------

print("vertex_token: admin-key gate")
reset_transport{}
r, e = sidecar.vertex_token()
eq(e, "ledger admin key not configured (set LEDGER_ADMIN_KEY or ADMIN_KEY)",
    "vertex_token fails closed without the admin key")
eq(#conns, 0, "vertex_token opens no transport without the admin key")

with_env({ LEDGER_ADMIN_KEY = "vertex-k" }, function() sidecar.init() end)
reset_transport{ res = { status = 200, body = '{"token":"t"}' }, decode_result = { token = "t" } }
r, e = sidecar.vertex_token()
eq(e, nil, "vertex_token success")
eq(conns[1].request.method, "GET", "vertex_token is a GET")
eq(conns[1].request.headers["X-Admin-Key"], "vertex-k", "vertex_token forwards X-Admin-Key")
eq(conns[1].connect.pool, pool(""), "vertex_token shares the hot-path pool")
check(conns[1].keepalive ~= nil and conns[1].keepalive[1] == 60000,
    "vertex_token pools the connection")

reset_transport{ res = { status = 500, body = "nope" } }
r, e = sidecar.vertex_token()
eq(e, "sidecar returned 500: nope", "vertex non-200 error carries status and body")
init_clean()

-- ---- stats pollers -------------------------------------------------------

print("council_stats / watch_stats: poller mechanics")
reset_transport{ res = { status = 200 }, decode_result = { active_locks = 1 } }
r, e = sidecar.council_stats()
eq(e, nil, "council_stats success")
eq(conns[1].timeout, 1000, "council_stats uses the 1000ms poller timeout")
eq(conns[1].connect.pool, pool("stats"), "council_stats uses the dedicated stats pool")
eq(conns[1].request.method, "GET", "council_stats is a GET")
eq(conns[1].request.headers["Connection"], "close", "council_stats sends Connection: close")
eq(conns[1].request.headers["Host"], "sidecar.local", "Host header present")
check(conns[1].keepalive == nil, "council_stats does not pool")
check(conns[1].closed, "council_stats closes the connection")

reset_transport{ res = { status = 503, body = "busy" } }
r, e = sidecar.council_stats()
eq(e, "sidecar returned 503: busy", "council_stats non-200 error text")

reset_transport{ res = { status = 200 }, decode_result = { audit_infra_errors_total = 0 } }
sidecar.watch_stats()
eq(conns[1].connect.pool, pool("watch_stats"), "watch_stats uses its own pool")
eq(conns[1].timeout, 1000, "watch_stats uses the poller timeout")
check(conns[1].closed, "watch_stats closes the connection")

-- ---- admin_proxy ---------------------------------------------------------

print("admin_proxy: forwarding and failure shapes")
set_request{
    method = "GET", uri = "/ledger/export", args = "limit=10&offset=4",
    headers = { ["X-Admin-Key"] = "edge-key" },
}
reset_transport{ res = { status = 200, headers = { ["Content-Type"] = "application/json" }, body = "[]" } }
sidecar.admin_proxy()
eq(conns[1].timeout, 5000, "admin_proxy timeout")
eq(conns[1].connect.pool, pool("admin"), "admin_proxy dedicated pool")
eq(conns[1].request.method, "GET", "admin_proxy forwards the method")
eq(conns[1].request.path, "/ledger/export?limit=10&offset=4",
    "admin_proxy preserves the query string")
eq(conns[1].request.headers["X-Admin-Key"], "edge-key",
    "admin_proxy forwards X-Admin-Key on /ledger/* routes")
eq(conns[1].request.headers["X-Request-ID"], "req-test", "admin_proxy forwards request identity")
eq(conns[1].request.headers["Content-Type"], "application/json",
    "admin_proxy declares JSON content type")
eq(conns[1].request.headers["Authorization"], nil, "admin_proxy strips Authorization")
check(conns[1].keepalive ~= nil and conns[1].keepalive[1] == 10000
    and conns[1].keepalive[2] == 4, "admin_proxy keepalive is (10s, 4)")
eq(ngx.status, 200, "admin_proxy forwards the upstream status")
eq(ngx.header["Content-Type"], "application/json", "admin_proxy forwards content type")
eq(ngx_out.said[1], "[]", "admin_proxy says the upstream body")

set_request{ method = "POST", uri = "/admin/keys", headers = { ["X-Admin-Key"] = "edge-key" } }
reset_transport{ res = { status = 200 } }
sidecar.admin_proxy()
eq(conns[1].request.headers["X-Admin-Key"], nil,
    "admin_proxy does NOT forward X-Admin-Key outside /ledger/*")

set_request{ method = "GET", uri = "/ledger/export" }
reset_transport{ connect_ok = false, connect_err = "no socket" }
reset_ngx()
sidecar.admin_proxy()
eq(ngx.status, 502, "connect failure answers 502")
eq(ngx.header["Content-Type"], "application/json", "502 is JSON")
contains(ngx_out.said[1], "error=sidecar unreachable", "502 names sidecar unreachable")
contains(ngx_out.said[1], "detail=no socket", "502 carries the connect detail")

reset_transport{ request_ok = false, request_err = "upstream broke" }
reset_ngx()
sidecar.admin_proxy()
eq(ngx.status, 502, "request failure answers 502")
contains(ngx_out.said[1], "error=sidecar request failed", "502 names the request failure")
contains(ngx_out.said[1], "detail=upstream broke", "502 carries the request detail")

reset_transport{ res = { status = 503 }, body_ok = false }
reset_ngx()
sidecar.admin_proxy()
eq(ngx.status, 503, "body-read failure forwards the upstream status, not a 502")
eq(ngx_out.said[1], "", "body-read failure says an empty body")

-- ---- watch_outbox_proxy --------------------------------------------------

print("watch_outbox_proxy: CORS + header allow-list")
set_request{
    method = "OPTIONS", uri = "/watch/outbox/list",
    headers = { ["Origin"] = "http://localhost:3000" },
}
reset_transport{}
reset_ngx()
sidecar.watch_outbox_proxy()
eq(ngx.status, 204, "loopback OPTIONS preflight answers 204 locally")
eq(ngx.header["Access-Control-Allow-Origin"], "http://localhost:3000",
    "loopback origin is echoed back")
eq(ngx.header["Vary"], "Origin", "CORS response varies on Origin")
eq(#conns, 0, "preflight never reaches the sidecar")

set_request{
    method = "GET", uri = "/watch/outbox/list",
    headers = {
        ["Origin"] = "https://evil.example",
        ["Authorization"] = "Bearer t",
        ["X-Tenant-Scope"] = "canary",
        ["Content-Type"] = "application/json",
        ["X-Admin-Key"] = "must-not-forward",
    },
}
reset_transport{ res = { status = 200, body = '{"rows":[]}' } }
reset_ngx()
sidecar.watch_outbox_proxy()
eq(ngx.header["Access-Control-Allow-Origin"], nil, "non-loopback origin gets no CORS header")
eq(conns[1].connect.pool, pool("watch_outbox"), "outbox proxy dedicated pool")
eq(conns[1].timeout, 5000, "outbox proxy timeout")
eq(conns[1].request.headers["Authorization"], "Bearer t", "Authorization forwarded")
eq(conns[1].request.headers["X-Tenant-Scope"], "canary", "tenant scope forwarded")
eq(conns[1].request.headers["Content-Type"], "application/json", "content type forwarded")
eq(conns[1].request.headers["X-Admin-Key"], nil, "X-Admin-Key never forwarded here")
eq(ngx.status, 200, "outbox proxy forwards the upstream status")
contains(ngx_out.printed[1], "rows", "outbox proxy prints the upstream body")

set_request{
    method = "POST", uri = "/watch/outbox/replay", body = '{"a":1}',
    headers = { ["Content-Type"] = "application/json" },
}
reset_transport{ res = { status = 200 } }
sidecar.watch_outbox_proxy()
eq(conns[1].request.headers["Content-Length"], "7", "body presence sets Content-Length")
eq(conns[1].request.body, '{"a":1}', "outbox proxy forwards the raw body")

set_request{ method = "GET", uri = "/watch/outbox/list" }
reset_transport{ res = { status = 500 }, body_ok = false }
reset_ngx()
sidecar.watch_outbox_proxy()
eq(ngx.status, 500, "outbox body-read failure forwards the upstream status")
eq(#ngx_out.printed, 0, "outbox body-read failure prints nothing")

-- ---- watch_ui_snapshot_proxy ---------------------------------------------

print("watch_ui_snapshot_proxy: exact GET projection")
set_request{ method = "POST", uri = "/watch/ui-snapshot/canary" }
reset_transport{}
reset_ngx()
sidecar.watch_ui_snapshot_proxy()
eq(ngx.status, 405, "non-GET is rejected")
contains(ngx_out.said[1], "method_not_allowed", "non-GET rejection body")
eq(#conns, 0, "non-GET never reaches the sidecar")

set_request{ method = "GET", uri = "/watch/ui-snapshot/canary/extra" }
reset_ngx()
sidecar.watch_ui_snapshot_proxy()
eq(ngx.status, 405, "multi-segment path is rejected")
eq(#conns, 0, "bad path never reaches the sidecar")

set_request{
    method = "GET", uri = "/watch/ui-snapshot/canary",
    headers = { ["Authorization"] = "Bearer p", ["X-Admin-Key"] = "no" },
}
reset_transport{ res = { status = 200, body = '{"snap":1}' } }
reset_ngx()
sidecar.watch_ui_snapshot_proxy()
eq(conns[1].connect.pool, pool("watch_ui_snapshot"), "ui snapshot dedicated pool")
eq(conns[1].timeout, 5000, "ui snapshot timeout")
eq(conns[1].request.method, "GET", "ui snapshot forwards GET only")
eq(conns[1].request.headers["Authorization"], "Bearer p", "ui snapshot forwards Authorization")
eq(conns[1].request.headers["X-Admin-Key"], nil, "ui snapshot forwards nothing else sensitive")
eq(ngx.status, 200, "ui snapshot forwards the upstream status")
contains(ngx_out.printed[1], "snap", "ui snapshot prints the body")

-- ---- watch_arm_proxy -----------------------------------------------------

print("watch_arm_proxy: bridge gates and forwarding")
set_request{ method = "POST", uri = "/watch/admin/producer/arm/stage" }
reset_transport{}
reset_ngx()
with_arm_marker(false, function() sidecar.watch_arm_proxy() end)
eq(ngx.status, 404, "arm bridge disabled without the marker file")
contains(ngx_out.said[1], "not_found", "disabled bridge body")
eq(#conns, 0, "disabled bridge never reaches the sidecar")

set_request{ method = "POST", uri = "/watch/admin/producer/arm/bogus" }
reset_transport{}
reset_ngx()
with_arm_marker(true, function() sidecar.watch_arm_proxy() end)
eq(ngx.status, 405, "routes outside the allow-list are rejected")
eq(#conns, 0, "rejected route never reaches the sidecar")

set_request{
    method = "POST", uri = "/watch/admin/producer/arm/stage", body = "",
    headers = { ["Authorization"] = "Bearer p" },
}
reset_transport{ res = { status = 200, body = '{"stage":"ok"}' } }
reset_ngx()
with_arm_marker(true, function() sidecar.watch_arm_proxy() end)
eq(conns[1].timeout, 10000, "arm bridge timeout")
eq(conns[1].connect.pool, pool("watch_arm"), "arm bridge dedicated pool")
eq(conns[1].request.method, "POST", "arm stage forwards POST")
eq(conns[1].request.path, "/watch/admin/producer/arm/stage", "arm path is exact (no query)")
eq(conns[1].request.body, "", "arm POST sends an explicit empty body")
eq(conns[1].request.headers["Authorization"], "Bearer p", "arm bridge forwards Authorization")
eq(conns[1].request.headers["Content-Type"], nil,
    "empty arm body sends no Content-Type")
check(conns[1].keepalive ~= nil and conns[1].keepalive[1] == 10000
    and conns[1].keepalive[2] == 4, "arm bridge keepalive is (10s, 4)")
eq(ngx.status, 200, "arm bridge forwards the upstream status")
contains(ngx_out.printed[1], "stage", "arm bridge prints the body")

set_request{ method = "GET", uri = "/watch/admin/producer/arm/status" }
reset_transport{ res = { status = 200, body = '{"armed":false}' } }
with_arm_marker(true, function() sidecar.watch_arm_proxy() end)
eq(conns[1].request.method, "GET", "arm status is a GET")
eq(conns[1].request.headers["Content-Type"], nil, "GET carries no content type")

-- ---- librarian_context ---------------------------------------------------

print("librarian_context")
reset_transport{ res = { status = 404, body = "missing" } }
r, e = sidecar.librarian_context("t1")
eq(e, "sidecar returned 404: missing", "librarian non-200 error text")
eq(conns[1].connect.pool, pool("librarian"), "librarian dedicated pool")
eq(conns[1].timeout, 50, "librarian uses the hot-path timeout")
check(conns[1].keepalive ~= nil and conns[1].keepalive[1] == 60000,
    "librarian pools the connection")

reset_transport{ body_ok = false, body_err = "truncated" }
r, e = sidecar.librarian_context("t1")
eq(e, "sidecar body read failed: truncated",
    "librarian body-read failure returns the standard error")

-- ---- TCP address form ----------------------------------------------------

print("TCP sidecar address")
with_env({ SIDECAR_ADDR = "127.0.0.1:8081" }, function() sidecar.init() end)
reset_transport{ res = { status = 200 }, decode_result = {} }
sidecar.council_stats()
eq(conns[1].connect.host, "127.0.0.1", "TCP address connects by host")
eq(conns[1].connect.port, 8081, "TCP address connects by port")
eq(conns[1].connect.pool, "sidecar:stats:127.0.0.1:8081", "pool name follows the TCP address")
eq(conns[1].request.headers["Host"], "127.0.0.1", "TCP Host header is the host")
init_clean()

if failures > 0 then
    print(failures .. " failure(s)")
    os.exit(1)
end
print("sidecar_transport_test: PASS")
