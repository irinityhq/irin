-- ==========================================================================
-- lib/ledger.lua — Ledger write helper with bounded retry.
--
-- Wraps `sidecar.ledger_record` with three-attempt retry (50/200/500ms
-- backoff). Designed for use inside `ngx.timer.at` closures; calls
-- ngx.sleep on retry which is only legal in a timer/light-thread context.
--
-- Why this exists as a shared lib (rather than inlined in cost.lua):
-- previously, only outbound_response writes were retried —
-- inbound_request, guard_input/blocked, cache_check/hit, route_decide/
-- rejected, budget_check/blocked, and policy_evaluate/blocked all fired
-- once and dropped silently on transient sidecar failure. Open-end
-- events on the audit chain are exactly as load-bearing as terminators
-- (a chain with one open end and no terminator is corrupt; same the
-- other way). The harden-mode the invariant specifically called this
-- out as a P1.
--
-- Module-load binding pattern (see lua/cost.lua header for full rationale):
-- the sidecar function reference is pinned at require time so timer
-- closures hold a function pointer, not a table reference. CI lint at
-- test/lint-timer-closures.sh enforces this.
--
-- Timer-pool capacity math (per the audit-chain schema in
-- COUNCIL_GATEWAY_CONTRACT.md):
--   * Every accepted request emits exactly ONE open-end event
--     (request_received) + exactly ONE terminating event (one of:
--     guard_input/blocked, cache_check/hit, route_decide/rejected,
--     budget_check/blocked, policy_evaluate/blocked, outbound_response).
--   * The 6 terminators are mutually exclusive — pick exactly one.
--   * Therefore a single request schedules AT MOST 2 ledger timers, not 6.
--   * Each timer runs once and exits (retries happen inside the closure
--     via ngx.sleep, not via re-scheduling).
--   * At RPS = 100, peak in-flight ledger timers ≈ 2 × 100 × MAX_BACKOFF_SUM
--     = 2 × 100 × 0.75 = 150, well under the OpenResty default
--     `lua_max_running_timers` of 256.
--   * If peak RPS or sidecar latency rises materially, the
--     `gw_ledger_timer_rejected` Prometheus counter (incremented by
--     `_M.schedule` below) will surface the saturation — refactor to a
--     bounded mpsc only when the counter shows non-zero.
-- ==========================================================================

local cjson    = require "cjson.safe"
local sidecar  = require "sidecar"

local sidecar_ledger_record = sidecar.ledger_record

local _M = {}

local MAX_ATTEMPTS    = 3
local BACKOFF_SECONDS = { 0.05, 0.2, 0.5 }
-- Jitter range applied to each backoff: 75% to 125% of the base value.
-- Defeats lockstep retry storms when many requests fail simultaneously
-- (e.g., a sidecar restart): without jitter they'd all retry at exactly
-- 50ms, then exactly 200ms, etc.
local JITTER_LO = 0.75
local JITTER_HI = 1.25

--- record_with_retry — fire a ledger event with bounded retry.
--
-- Returns true on success, false on exhausted retries (in which case
-- the failure has been logged with ngx.ERR + structured JSON).
--
-- Must be called from a timer/light-thread context — ngx.sleep on
-- retry will fail in any phase where cosockets are not available.
function _M.record_with_retry(source, target, payload, metadata, caller_key)
    -- request_id may live in either the payload or the metadata depending on
    -- the call site (router.lua's open-end + close events put it in payload;
    -- cost.lua's outbound_response puts it in both). Falling back lets the
    -- failure log be useful from every site, which is the entire point of
    -- having the helper — the retry-exhausted log is the observability
    -- backstop and emitting `request_id=?` defeats it.
    local request_id = (metadata and metadata.request_id)
                    or (payload and payload.request_id)
                    or "?"
    local action     = (metadata and metadata.action) or "?"

    -- caller_key threads through to the sidecar's /ledger/record body where
    -- it becomes the v2 schema's per-key audit identity. Empty string or nil
    -- both mean "no key" — the sidecar collapses both to NULL in the column.
    for attempt = 1, MAX_ATTEMPTS do
        local result, err = sidecar_ledger_record(source, target, payload, metadata, caller_key)
        if result and not err then
            return true
        end
        if attempt < MAX_ATTEMPTS then
            local base    = BACKOFF_SECONDS[attempt] or 0.5
            local jittery = base * (JITTER_LO + math.random() * (JITTER_HI - JITTER_LO))
            ngx.sleep(jittery)
        else
            ngx.log(ngx.ERR, cjson.encode({
                event       = "ledger_commit_failed",
                request_id  = request_id,
                source      = source,
                target      = target,
                action      = action,
                attempts    = MAX_ATTEMPTS,
                last_error  = err or "unknown",
            }))
        end
    end
    return false
end

--- schedule — `ngx.timer.at(0, fn)` wrapped with rejection capture.
--
-- The default OpenResty `lua_max_running_timers` is 256; under sustained
-- timer-pool saturation (sidecar slowness or peak RPS), `ngx.timer.at`
-- returns nil + an error and the closure never runs. Without this wrapper
-- we'd drop the audit-chain link silently — exactly the failure mode the
-- bounded retry was designed to surface.
--
-- The wrapper:
--   * captures the schedule result;
--   * increments the `ledger_timer_rejected` shared-dict counter
--     (consumed by `cost.prometheus()` and emitted as
--     `gw_ledger_timer_rejected`);
--   * emits a structured ERR log with the same shape as
--     `ledger_commit_failed` so a single jq pattern catches both.
--
-- `action` and `request_id` are caller-supplied so the rejection log is
-- attributable without parsing the closure.
function _M.schedule(action, request_id, fn)
    local ok, err = ngx.timer.at(0, fn)
    if not ok then
        local metrics = ngx.shared.gw_metrics
        if metrics then
            metrics:incr("ledger_timer_rejected:" .. (action or "unknown"), 1, 0)
        end
        ngx.log(ngx.ERR, cjson.encode({
            event       = "ledger_timer_rejected",
            request_id  = request_id or "?",
            action      = action or "?",
            error       = err or "unknown",
        }))
    end
    return ok, err
end

return _M
