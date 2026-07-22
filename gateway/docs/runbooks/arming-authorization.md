# Arming the Watch Producer

The watch producer can cause paid deliberation and must remain off until an
operator intentionally completes the hardware-backed ceremony. Rehearsal
uses the same stage, challenge, signature, and audit path without starting the
producer.

## Safety invariants

- The producer is off by default. `WATCH_PRODUCER_ENABLED` alone does not arm; starting the producer at boot also needs `EXECUTION_MODE=LIVE` and `WATCH_DISPATCHER_GATEWAY_KEY`. Starting is not spending: every spend reserve fails closed unless a hardware ceremony has written a signed `active_arm` row — the env path alone cannot cause paid deliberation.
- A real arm requires an enrolled hardware credential and an authorized local
  principal.
- Rehearsal is strongly recommended before a real arm (process). Code does not require a prior rehearsal-ok record; dirty builds force rehearsal mode.
- One shared `watch.db` has one live writer claim.
- Spend ceilings are enforced below the producer.
- Any authorized principal may disarm immediately; disarm never requires a
  second factor.
- Database or writer-claim uncertainty fails closed.


## HTTP ceremony state machine

Host CLIs call these sidecar routes over the management UDS:

| Step | Route | Role |
| --- | --- | --- |
| Legacy arm | `POST /watch/admin/producer/arm` | **410 Gone** — do not use |
| Stage | `POST /watch/admin/producer/arm/stage` | Principal stages challenge v3 (JCS); default TTL **120s** (`ARM_STAGE_TTL_MS`) |
| Pending | `GET /watch/admin/producer/arm/pending` | Crash-resume; returns **stored** challenge bytes |
| Confirm | `POST /watch/admin/producer/arm/confirm` | SE-P256 or FIDO2-ES256 local attest; content-bound cap/window |
| Disarm | `POST /watch/admin/producer/disarm` | Admin token **or** any arm principal |

Confirm runs in one DB transaction (pending, signature, counter, content-binding). Failures are fail-closed.

## Spend ceilings (enforced)

| Limit | Default | Env |
| --- | --- | --- |
| Daily Watch spend ceiling | **$50 USD / UTC day** | `DAILY_SPEND_CAP_USD` may only **lower**; raise/garbage refuses boot |
| Fanout reserve unit | **$5** | `WATCH_MAX_FANOUT_COST_USD` |
| Signed spend window | boot-locked (default 24h) | `GW_ARM_WINDOW_MS`; signed into challenge; not env-extendable after tap when signed-window enforcement is on |

Reserve re-verifies the arm signature and content binding before spend.

## Boot env triple-gate (automation path)

Producer may start at boot only if **all** of:

1. `WATCH_PRODUCER_ENABLED` is `1` or `true`
2. `EXECUTION_MODE` is exactly `LIVE`
3. `WATCH_DISPATCHER_GATEWAY_KEY` is set

Any other `EXECUTION_MODE` keeps the gate closed. This path acquires the writer claim, appends a boot arm audit entry, and runs the sweep loops — but it never writes a signed `active_arm`. Spend reserves fail closed without one, so boot-env arming cannot authorize spend on its own; only a completed hardware ceremony can.

## Writer claim

Only one live producer writer per `watch.db`. Heartbeat default **30s**, stale **90s**; lost claim self-disarms. UI `action_production_armed` reflects a live kill channel, not merely the env flag.

## Worker / dispatcher (default off)

- `WATCH_DISPATCHER_ENABLED` defaults **false** (claim → council-triage → stage).
- `WATCH_WORKER_ENABLED` defaults **false** and controls the built-in worker
  loop. Authenticated claim, heartbeat, ack, worker-ack, and nack routes remain
  mounted independently. The built-in loop is not an operator-ready autonomous
  execution feature.

## Related docs

- [`../watch-api.md`](../watch-api.md) — full `/watch/*` surface
- [`../../../docs/surface-map.md`](../../../docs/surface-map.md) — compact surface map
- [`../../../docs/security-claims-vs-reality.md`](../../../docs/security-claims-vs-reality.md)

## Configure the local principal

Edit `~/.config/irin/gateway.env` and set a generated local token:

```text
GW_ARM_PRINCIPALS=sovereign-op:<random-token>
```

Keep the file mode `0600`, then restart the runtime so the sidecar loads the
registry. The helper reads this same file. Do not pass the token as a command
argument.

The producer also requires a configured dispatcher key, a live Council, and
the intended spend limits. Keep the producer off while those prerequisites
are checked.

## Enroll a hardware credential

For Touch ID on macOS:

```bash
gateway/bin/arm-enroll
make runtime-restart
```

Enrollment writes only the public credential record to the durable sidecar
volume. Verify the reported keyset hash against the sidecar boot log:

```bash
gateway/bin/verify-attest-keyset-hash
docker compose -p gateway logs sidecar 2>&1 | grep keyset_hash
```

A FIDO2 backup credential can be enrolled with
`gateway/bin/arm-enroll-fido2`.

## Rehearse

```bash
gateway/bin/arm --rehearse
```

Expected result: `rehearsal-ok`. The producer does not start. Treat an
unexpected biometric prompt, changed credential hash, expired stage, or
writer-claim conflict as a stop condition.

## Arm

Before a real arm, confirm:

- Council, Gateway, and the Watch surface are healthy.
- The configured tenant and Sentinel profile are the intended ones.
- Daily and per-directive spend ceilings are explicit.
- The dispatcher credential is present.
- The last rehearsal passed against the current build and keyset.
- There is no other writer using the same `watch.db`.

Then run:

```bash
gateway/bin/arm
```

Expected result: `armed`. Verify the producer state, writer claim, arm audit,
and spend metrics before creating a test fire.

## Max-loss bound

Hard ceilings (see above): **$50/day** Watch spend and **$5** default fanout
reserve unit, plus any in-flight Council work already accepted.

Operational upper bound:

```text
max_loss = charge_unit * claims_per_tick + in_flight_at_disarm
bounded_loss = min(daily_ceiling, max_loss)
```

Set claim batch and dispatcher settings so this bound is acceptable before
arming. The database spend ledger remains the enforcement layer even if
producer ownership changes.

## Abort and disarm

| Trigger | Action | Who |
|---|---|---|
| Unexpected provider charge or duplicate work | Disarm immediately | Any authorized principal |
| Writer claim lost or heartbeat fails | Confirm fail-closed self-disarm | Operator |
| Watch database unavailable | Keep producer off | Operator |
| Credential or keyset hash changed unexpectedly | Disarm and investigate | Operator |
| Spend cap or reconciliation alarm | Disarm and preserve evidence | Operator |

Use the management socket so the kill switch is not exposed on the public
listener:

```bash
gateway/bin/disarm
```

If the helper cannot run, stop the canonical runtime and set
`WATCH_PRODUCER_ENABLED=false` in the local Gateway environment before the
next start. Do not delete the state volume during incident handling.

## DB-unavailable = fail-closed

The producer must not arm or remain armed when it cannot prove ownership of
the singleton writer claim or append the required audit state. Recovery is:

1. Leave the producer disabled.
2. Restore database availability without replacing the volume.
3. Verify ledger and watch-chain integrity.
4. Restart, rehearse, and inspect the new writer claim.
5. Arm only after the failure cause is understood.

## Signature expiry and revocation

Stages expire and cannot be reused. Remove a compromised public credential
from the registry while the runtime is stopped, restart, and verify the new
keyset hash before rehearsal. Keep the private hardware key outside the
repository and sidecar volume.

## Partial-deliberation cost

Disarm prevents new claims but cannot cancel provider work already accepted.
Expect in-flight cost to settle after disarm, and reconcile the provider,
Gateway spend ledger, and outbox before considering the event closed.

## Optional real-charge reconciliation

The isolated harness is `test/p0e_real_charge.sh`. Its default mode uses a
no-spend stub. Live mode is separately gated and must prove `billed == M` for
the intended request count while remaining below the configured test cap.
Never run the live mode as a routine setup check.
