# Disarm the Watch Producer

Disarm is the first response to unexpected cost, duplicate work, a writer
claim problem, database uncertainty, or an unexplained credential change.
It does not require a second factor.

From the repository root:

```bash
gateway/bin/disarm
```

Then verify:

- producer state is off;
- no new claims are created;
- the disarm event is present in the arm audit;
- in-flight work settles without new dispatches;
- provider cost and the Gateway spend ledger reconcile.

If the management helper is unavailable, stop the canonical runtime, set
`WATCH_PRODUCER_ENABLED=false` in `~/.config/irin/gateway.env`, and leave it
off on restart. Do not use `docker compose down -v`; the volume contains the
evidence needed to diagnose and reconcile the event.

Resolve the cause, verify database and ledger integrity, restart, and run a
fresh rehearsal before any later arm.
