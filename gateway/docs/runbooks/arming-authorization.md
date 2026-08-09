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
| Pending | `GET /watch/admin/producer/arm/pending` | Crash-resume; returns **stored** challenge bytes (never re-derived) |
| Confirm | `POST /watch/admin/producer/arm/confirm` | Single-operator principal token + local hardware attestation: SE-P256 or FIDO2-ES256 signs the **stored** challenge bytes; content-bound cap/window. se-p256: no UP assertion in the envelope, product SE path biometry-gated (`gateway/bin/arm-attest.swift`); fido2-es256: `authenticatorData` required and rejected without the UP flag |
| Status | `GET /watch/admin/producer/arm/status` | Principal-authenticated projection: armed/staged flags, lease deadline, registry-loaded, keyset digest. **No** challenge, signature, or principal |
| Disarm | `POST /watch/admin/producer/disarm` | Admin token **or** any arm principal |

Confirm runs in one DB transaction (pending, signature, counter, content-binding) and verifies the ES256 signature over the stored stage challenge bytes. Failures are fail-closed. This is not dual custody or four-eyes by two human tokens.

## Desktop Touch ID bridge (installed IRIN.app)

The installed app runs the same ceremony from Settings, beside the Gateway
control — no terminal command. What changes:

- The Gateway Pack admits exactly two arm-surface env keys on the sidecar:
  `GW_ARM_PRINCIPALS` (Keychain-held, per-spawn only, scrubbed from the ambient
  environment) and `GW_ARM_ATTEST_KEYS_PATH` (a fixed in-container path).
  `WATCH_ADMIN_TOKEN` is armed the same way (Keychain-held, per-spawn only,
  scrubbed from the ambient environment) for the read-only Watch/Outbox
  surface, and producer/dispatcher stay `false` at boot.
- The enrollment registry is an app-owned file bind-mounted read-only at
  `/run/secrets/arm_attest_keys.json` only in the validating sidecar. It holds
  PUBLIC credential records but remains root-owned mode `0600`. The edge never
  receives or parses it. Instead, the desktop pack mounts an empty non-secret
  `arm-bridge-enabled` marker read-only for its unprivileged OpenResty workers.
  Default registry contents `[]` load as UNLOADED, so enabling Gateway arms
  nothing.
- The registry is boot-loaded. The installed app refreshes its owned Gateway
  Pack as part of the explicit setup/re-enrollment action, so the new registry
  and Keychain principal are live before the control reports ready.
- Gateway exposes the five routes above at loopback ONLY when the desktop
  pack's non-secret feature marker is mounted in the edge container
  (`lua/sidecar.lua watch_arm_proxy`, exact method+path allow-list). Every other
  deployment answers 404 and the ceremony stays UDS-only.
- The app pins the bundled helper's SHA-256 and the registry keyset digest at
  enrollment. A helper swap, a missing Secure Enclave blob, or a sidecar that
  loaded a different registry forces explicit re-enrollment. App-owned prior
  enrollment records and opaque wrapped-key blobs are atomically moved to dated
  archives, never read, copied, deleted, or reused across identities.

## Spend ceilings (enforced)

| Limit | Value | Env / notes |
| --- | --- | --- |
| Day-cap code ceiling | **$50 USD / UTC day** | Hard-coded ceiling; `DAILY_SPEND_CAP_USD` may only **lower**; raise/garbage refuses boot |
| Canonical pack day cap | **$25 / UTC day** | Compose default `DAILY_SPEND_CAP_USD=25` |
| Per-directive reservation (code default) | **$5** | `WATCH_MAX_FANOUT_COST_USD` default |
| Canonical pack fanout reserve | **$2.50** | Compose default `WATCH_MAX_FANOUT_COST_USD=2.50` |
| Signed spend window | boot-locked (default 24h) | `GW_ARM_WINDOW_MS`; signed into challenge; not env-extendable after tap when signed-window enforcement is on |

These are **ex-ante reservations**, not a post-hoc hard clamp: claim reserves the
ceiling up front; settle records realized cost and **flags overshoot** when
realized exceeds the reservation. Already accepted provider work cannot be
un-spent by the cap. Reserve re-verifies the arm signature over the stored
challenge bytes and the content binding before spend.

## Boot env triple-gate (automation path)

Producer may start at boot only if **all** of:

1. `WATCH_PRODUCER_ENABLED` is `1` or `true`
2. `EXECUTION_MODE` is exactly `LIVE`
3. `WATCH_DISPATCHER_GATEWAY_KEY` is set

Any other `EXECUTION_MODE` keeps the gate closed. This path acquires the writer claim, appends a boot arm audit entry, and runs the sweep loops — but it never writes a signed `active_arm`. Spend reserves fail closed without one, so boot-env arming cannot authorize spend on its own; only a completed hardware ceremony can.

## Startup cabinet probe (inference cost)

Before live dispatcher claim or boot hydration trusts Council output, the
sidecar runs the council-triage **startup probe**: an authenticated Gateway
`/v1/chat/completions` call against the local `council-triage` cabinet,
retried up to `WATCH_DISPATCHER_PROBE_MAX_ATTEMPTS` (default **30**) at
`WATCH_DISPATCHER_PROBE_RETRY_MS` intervals (default 1s). Each attempt that
reaches a provider is real inference and **incurs cost** under the same
metering path as other governed completions — worst case, one boot can meter
up to the full attempt budget, and there is no separate probe metering
counter. A failing probe degrades or aborts dispatcher activation per boot
policy; it does not create `active_arm` or authorize spend by itself.

## Writer claim

Only one live producer writer per `watch.db`. Heartbeat default **30s**, stale **90s**; lost claim self-disarms. UI `action_production_armed` reflects a live kill channel, not merely the env flag.

## Worker / dispatcher (default off)

- `WATCH_DISPATCHER_ENABLED` defaults **false** (claim → council-triage → stage).
- `WATCH_WORKER_ENABLED` defaults **false** and controls the built-in worker
  loop. Authenticated claim, heartbeat, ack, worker-ack, and nack routes remain
  mounted independently. The built-in loop is not an operator-ready autonomous
  execution feature.
- The only effect the worker loop can execute is `quarantine_producer`, which
  disarms the producer. It requires a signed `execute` directive whose verified
  envelope scopes `tenant`, `in_response_to`, `subject=watch-producer`, and
  `allowed_actions: ["quarantine_producer"]`, plus a capability token that is
  still valid at effect time. `prepare` and `execute` without a valid token,
  and every other action, are nacked fail-closed.

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
relaunch IRIN.app or restart the Gateway Pack from Settings
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

Ceilings (see above): **$50/day** code day-cap (canonical pack **$25/day**),
per-directive reservation code default **$5** (canonical pack **$2.50**), plus
any in-flight Council work already accepted. Treat these as ex-ante
reservation bounds with flagged overshoot, not as a guarantee that realized
cost never exceeds the reserve.

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
