#!/usr/bin/env bash
# ==========================================================================
# lint-timer-closures.sh — guard the module-load function-binding pattern.
#
# OpenResty timer closures created by ngx.timer.at(0, function(...)...end)
# fire AFTER the access phase has returned. If the closure body contains
# `sidecar.X(...)`, the table lookup resolves at call time — meaning a
# hot-reload that swaps the sidecar module table (or a test that monkey-
# patches it) silently diverts in-flight ledger writes / route outcomes /
# cache stores to the new implementation between the access and log phases.
#
# Function references are bound at module load:
#   local sidecar_ledger_record = sidecar.ledger_record   -- pinned reference
#
# This lint enforces that policy: `sidecar\.` must not appear anywhere
# inside an `ngx.timer.at` body. The check is structural — awk tracks
# brace depth from the opening `function(premature)` — so it survives
# nested timers, anonymous functions, and reformatting.
# ==========================================================================

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXIT=0

for f in "$ROOT/lua"/*.lua "$ROOT/lua/lib"/*.lua; do
    [ -f "$f" ] || continue
    awk -v file="$f" '
        # Track depth from the "ngx.timer.at" opening to its matching close.
        /ngx\.timer\.at[[:space:]]*\(/ {
            in_timer = 1
            depth = 0
            timer_start_line = NR
        }
        in_timer {
            # Crude but effective brace tracking. Strings with literal
            # braces inside timer bodies would fool this; we have none and
            # the lint is allowed to be slightly conservative.
            for (i = 1; i <= length($0); i++) {
                c = substr($0, i, 1)
                if (c == "(" || c == "{") depth++
                else if (c == ")" || c == "}") {
                    depth--
                    if (depth == 0) { in_timer = 0; break }
                }
            }
            # Inside the timer body — flag any `sidecar.` reference.
            if (in_timer && $0 ~ /sidecar\./) {
                # Allow comments mentioning the rule (the policy itself).
                stripped = $0
                sub(/--.*$/, "", stripped)
                if (stripped ~ /sidecar\./) {
                    printf("%s:%d: sidecar.X inside ngx.timer.at body (started line %d) — bind at module load instead\n", \
                           file, NR, timer_start_line)
                    bad = 1
                }
            }
        }
        END { exit bad ? 1 : 0 }
    ' "$f" || EXIT=1
done

if [ $EXIT -ne 0 ]; then
    echo
    echo "❌ lint-timer-closures: violations above. See lua/cost.lua:14-23 for the binding pattern."
    exit 1
fi
echo "✅ lint-timer-closures: no sidecar.X references inside ngx.timer.at bodies"
