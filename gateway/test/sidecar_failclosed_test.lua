-- sidecar_failclosed_test.lua — S-30 / D-01 transport-failure regressions.
-- Run from gateway/: `make lua-unit`.
package.path = "./?.lua;./lua/?.lua;" .. package.path

package.preload["cjson.safe"] = function()
    return {
        encode = function() return "{}" end,
        decode = function() return nil, "unexpected decode" end,
    }
end

package.preload["resty.http"] = function()
    return {
        new = function()
            return {
                set_timeout = function() end,
                connect = function() return nil, "connection refused" end,
            }
        end,
    }
end

_G.ngx = {
    log = function() end,
    INFO = 1,
    WARN = 2,
    ERR = 3,
}

local sidecar = require("sidecar")
local failures = 0

local function check(condition, message)
    if condition then
        print("  ok   - " .. message)
    else
        failures = failures + 1
        print("  FAIL - " .. message)
    end
end

local policy, policy_err = sidecar.policy_evaluate("openai", "hello", "RED")
check(policy_err == nil, "policy transport failure is encoded as a decision")
check(policy ~= nil and policy.allowed == false, "policy transport failure denies")
check(policy ~= nil and policy.dry_run == false, "policy transport failure is enforced")
check(policy ~= nil and policy.level == "RED", "policy transport failure preserves level")

local routing, route_err = sidecar.route_decide("fast", {}, nil, "GREEN", false)
check(routing == nil, "route transport failure has no routing decision")
check(type(route_err) == "string" and route_err:find("sidecar unreachable", 1, true),
      "route transport failure returns an error")

if failures > 0 then
    print(failures .. " failure(s)")
    os.exit(1)
end
print("sidecar_failclosed_test: PASS")
