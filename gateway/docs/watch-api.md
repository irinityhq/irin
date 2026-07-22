# Watch HTTP API

---

## Base

- Served by **gateway-sidecar** over the management UDS (and whatever nginx proxies in compose).
- Errors use **RFC 9457 problem+json** on the guarded API paths documented
  below.
- **Canary:** when `WATCH_CANARY_TENANT` is set (compose often `canary`), tenant-scoped admin paths reject other tenants with `403 single_tenant_violation`. Unset → default tenant name `sovereign` for the tripwire config.

### Auth classes

| Class | Mechanism | Typical routes |
| --- | --- | --- |
| Public read | No admin header | list, temperature, audit, stats, verify-chain, outbox **pubkey** |
| Admin bearer | `WATCH_ADMIN_TOKEN` or `BOOTSTRAP_TOKEN` (empty → all admin 401) | ui-snapshot, force-wake, quarantine clear, outbox rows, claim/ack/… |
| Arm principal | `Bearer name:token` from `GW_ARM_PRINCIPALS` | arm stage/confirm; see the [arming runbook](runbooks/arming-authorization.md) |

Admin comparison fails closed for an empty expected secret, caps bearer length
at 128 bytes, hashes both sides with SHA-256, and uses a constant-time compare.
Tenant-scoped Outbox routes also enforce the configured canary tenant. Claim,
ack, heartbeat, worker-ack, and nack take that tenant from the required
`X-Tenant-Scope` header; list and get take it from the path. See each route
below.

---

## Read surfaces

### `GET /watch/list/{tenant}`

Registered sentinels for tenant: tier, cooldown, enabled, hard_killed, last_fire, fires_last_hour.
DB error → 500 problem+json.

### `GET /watch/temperature/{tenant}`

Score ≈ `clamp01(0.7 * fires_1h/5 + 0.3 * fires_24h/24)`; levels **cold** (&lt;0.15), **warm** (&lt;0.6), **hot**.
No admin auth on handler.

### `GET /watch/audit/{tenant}`

Descending fire log. Query: `limit` default **50**, cap **500**; optional `before_id` cursor.
Fields include id, sentinel, fired_at, state_json, reason, prev_hash, hash.

### `GET /watch/stats`

Process-wide WatchStats: infra failures, pending/lease/dup, spend_today + spend_cap_usd, arm_rejected_unauth, recon, etc. Spend gauge degrades to 0.0 on DB failure and bumps a failure counter.

### `GET /watch/verify-chain/{tenant}`

Walk per-tenant hash chain; **5s** budget → 504 on exceed. Returns **200** with `ok:false` when chain broken (not only 5xx).

### `GET /watch/ui-snapshot/{tenant}`

**Admin + canary.** Sanitized projection for UI: sentinel readiness, temperature, recent fires (no raw state/reason dump), budget, degradation, **`action_production_armed`** (live kill channel present — not merely env).

---

## Admin mutation

### `POST /watch/force-wake/{sentinel}`

Admin first (**401 before 404**). Skips observe/interesting; synthetic escalate + same audit write path.
409 if quarantined or hard-killed. Body tenant default historically `sovereign` (see known default-tenant caveat below).

### `DELETE /watch/quarantine/{sentinel}`

Admin. Clears quarantine/hard-kill; optional `reset_probation` / skip_probation.
404 if registry miss. Returns cleared labels plus `probation_until` (Unix ms or
null).

---

## Outbox

### `GET /watch/outbox/pubkey`

**Public.** Ed25519 verifying key + kid (`sidecar-v1-{first8 hex of sha256(pubkey)}`).

### `GET /watch/outbox/{tenant}` and `GET /watch/outbox/{tenant}/{id}`

**Admin + canary path tenant.** Authentication is checked before the canary
guard and store lookup, avoiding an unauthenticated existence oracle. Returns
`envelope_json_canonical` plus the signature for client verification. List
limit is clamped to 1..200; the cursor is base64 `created_at:id`.

### `POST /watch/outbox/claim`

Admin + tenant scope + canary.
`claim_limit` clamp 1..200; `lease_duration_ms` clamp **1000..300000**.
Returns claimed rows including handle in worker_provenance.

### `POST /watch/outbox/{id}/heartbeat`

Admin + required `X-Tenant-Scope` + canary. Extends the lease using
`opaque_handle`; `extension_ms` is clamped to 1s..300s.
Invalid handle 400; not actionable 409; success 204.

### `POST /watch/outbox/{id}/ack`

Admin + `X-Tenant-Scope` + canary. Acked → 204; dismissed/expired → 409 `not_actionable`.

### `POST /watch/outbox/{id}/worker_ack`

Admin + required `X-Tenant-Scope` + canary. Completes work with the full
`WorkerProvenanceGuard` persisted.
**Product note:** this authenticated completion surface is always mounted. The
built-in autonomous worker loop that calls it is separately default-off through
`WATCH_WORKER_ENABLED` and is not an operator-ready product path.

### `POST /watch/outbox/{id}/nack`

Admin + required `X-Tenant-Scope` + canary. Returns the row to staged/retry
using `error_reason` and the claim handle.

---

## Tenant policy

`GET /watch/tenant-policy/{tenant}` is canary-guarded.
`POST /watch/tenant-policy/{tenant}` requires admin authentication and the
canary guard. These routes manage the Watch database policy used by
capability-token checks.

---

## Security boundaries

1. Public watch reads exist; they are not a multi-tenant security boundary (single-operator + canary).
2. Outbox **artifacts** require admin; only **pubkey** is public among outbox routes.
3. Claim/lease ceilings exist to bound DoS and stuck workers.
4. Force-wake and quarantine clear are incident tools, not casual toggles.

---

## Quarantine, probation, and force-wake

---

## Why this exists

Sentinels can fail or spam. Quarantine **slows or hard-kills** noisy units without deleting the append-only fire chain. Operators need a clear SM for incident response (stuck sentinel, bad config, force diagnostic fire).

---

## Defaults (QuarantineConfig)

These are the defaults in `QuarantineConfig`:

| Parameter | Default | Meaning |
| --- | --- | --- |
| `fails_to_trigger` | **2** | Failures before quarantine engages |
| Backoff schedule | **60s, 300s, 1800s, 3600s** | Escalating cool-down |
| `hard_kill_after_cycles` | **5** within **1h** window | Escalate to hard-kill |
| Hysteresis | **3** successes and ≥ **2×** backoff | Exit quarantine |
| Probation after hard-kill clear | **10 min** log-only | Does not block fire; tags reason `[PROBATION]` |
| Pending hard-kill retry | periodic, **5s** await budget | Failed hard_kill DB upserts retried |

State is in-memory with DB rehydrate for arm_pending / probation / hard_kill on boot.

---

## Fire pipeline interaction

Each fire goes through observe → interesting → escalate → audit with phase
budgets: a roughly 200 ms total budget with per-phase caps, pre- and
post-interesting quarantine gates, and an optimistic-concurrency insert on the
hash chain.

`FireOutcome` includes Fired, Uninteresting,
Gated(Quarantined|HardKilled|ProbationLogOnly), observe/escalate/audit errors,
panic, timeout, and budget violation.

Polling and Deep Sentinels tick on their cooldown; Fast Sentinels are kicked
externally. The runner records success or failure in quarantine state after the
pipeline completes.

---

## Operator actions

### Force-wake — `POST /watch/force-wake/{sentinel}`

- **Admin bearer required first** (401 before 404 — no name oracle).
- Skips observe/interesting; synthetic escalate + normal audit write.
- **409** if quarantined or hard-killed (clear first).
- An omitted body tenant defaults to `sovereign`; setting the canary tenant does
  not rewrite that request-body default.

Use for: “I need a diagnostic fire now,” not as a substitute for fixing config.

### Clear quarantine — `DELETE /watch/quarantine/{sentinel}`

- Admin bearer.
- Registry miss → 404.
- Clears quarantine / hard-kill; optional **reset probation** (`skip_probation` mapping).
- Response includes cleared labels plus `probation_until` (Unix ms or null).

### Probation log-only

After hard-kill clear, probation **does not block** the fire pipeline; reasons
are prefixed `[PROBATION]`. The current CDC sweep does not filter that prefix:
it inserts every committed fire, including probation fires, into
`pending_escalations`. A live dispatcher may therefore claim one. Producer
startup and the signed `active_arm` spend check remain separate gates, but
operators should disarm or disable the dispatcher when probation traffic must
not reach paid Council work.

---

## Related integrity

- **watch_fires** remain append-only; quarantine never deletes history.
- **watch-health-watch** treats other units' quarantine or hard-kill state and
  chain breaks as interesting.
- Pending hard-kill retry failures increment metrics; admin clear can clear the
  pending state.

---

## Incident playbook (short)

1. Check `GET /watch/list/{tenant}` / ui-snapshot for hard_killed / readiness.
2. Inspect `GET /watch/audit/{tenant}` and verify-chain.
3. If stuck quarantined: fix root cause (config path, endpoint, credentials).
4. `DELETE /watch/quarantine/{sentinel}` when safe; consider leaving probation.
5. Optional force-wake **after** clear if you need a single diagnostic fire.
6. If the producer is armed and spend is climbing for the wrong reason, run
   `gateway/bin/disarm`; quarantine alone is not the spend kill switch.

## Implementation references

- Route mounting: [`../sidecar-rs/src/main.rs`](../sidecar-rs/src/main.rs)
- HTTP handlers and authentication guards: [`../sidecar-rs/src/watch/api.rs`](../sidecar-rs/src/watch/api.rs)
- Outbox and spend storage: [`../sidecar-rs/src/watch/db.rs`](../sidecar-rs/src/watch/db.rs)
- Quarantine state machine: [`../sidecar-rs/src/watch/quarantine.rs`](../sidecar-rs/src/watch/quarantine.rs)
- Producer startup: [`../sidecar-rs/src/watch/runner.rs`](../sidecar-rs/src/watch/runner.rs)
- Built-in worker loop: [`../sidecar-rs/src/watch/worker.rs`](../sidecar-rs/src/watch/worker.rs)

---
