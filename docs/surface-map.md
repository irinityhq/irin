# Surface map

This page is a compact index of the operator-visible surfaces implemented in
the current tree. It is not an OpenAPI specification or an authorization to arm
the Watch producer.

## Mental model

**Council / War Room** runs multi-model deliberation. **Gateway** is an
optional governed path for authentication, metering, caching, routing, policy,
and audit records. The **Watch plane** lives in Gateway: deterministic
Sentinels observe and record evidence, while separate producer and dispatcher
controls govern escalation toward Council and the signed Outbox.

Producer startup and spend authorization are separate. A completed hardware
ceremony creates the signed `active_arm` required for spend. The narrow boot
triple-gate (`WATCH_PRODUCER_ENABLED`, `EXECUTION_MODE=LIVE`, and a dispatcher
key) can start the producer for recovery or testing, but does not create an
`active_arm` and cannot authorize paid deliberation by itself. Defaults keep
the producer, dispatcher, built-in worker loop, and spend paths off.

## Watch plane HTTP

Many read routes have no admin header at the handler. Admin routes require the
`WATCH_ADMIN_TOKEN` or `BOOTSTRAP_TOKEN` bearer. Outbox row reads require
admin authentication. The arm ceremony uses principals from
`GW_ARM_PRINCIPALS`, not the admin token alone.

| Surface | Behavior |
| --- | --- |
| `GET /watch/list/{tenant}` | Registered Sentinels: tier, cooldown, enabled, hard-killed state, last fire, and recent fire count. |
| `GET /watch/temperature/{tenant}` | Heat score from fire rates; levels are cold, warm, and hot. |
| `GET /watch/audit/{tenant}` | Descending fire log, capped at 500 rows, including hash-chain fields. |
| `GET /watch/stats` | Watch counters and gauges including spend, arm rejections, leases, duplicates, and kill-switch latency. |
| `GET /watch/ui-snapshot/{tenant}` | Admin- and canary-guarded sanitized UI projection, including `action_production_armed`. |
| `POST /watch/force-wake/{sentinel}` | Admin-only manual fire; authentication is checked before existence, and quarantined or hard-killed Sentinels return 409. |
| `DELETE /watch/quarantine/{sentinel}` | Admin clear of quarantine or hard kill, with optional probation reset. |
| `GET /watch/verify-chain/{tenant}` | Hash-chain walk with a five-second budget; a broken chain returns 200 with `ok:false`. |
| `GET /watch/outbox/pubkey` | Public Ed25519 verification key and key id. |
| `GET /watch/outbox/{tenant}` and `/{id}` | Admin- and canary-guarded canonical envelope and signature for client verification. |
| `POST /watch/outbox/claim` | Mounted admin API for claiming rows with bounded leases. |
| `POST /watch/outbox/{id}/heartbeat` | Mounted admin API for extending a lease using its opaque handle. |
| `POST /watch/outbox/{id}/ack` | Mounted admin API for acknowledgement; dismissed or expired rows return 409. |
| `POST /watch/outbox/{id}/worker_ack` | Mounted admin completion API accepting `WorkerProvenanceGuard`; it is distinct from the default-off built-in worker loop. |
| `POST /watch/outbox/{id}/nack` | Mounted admin API returning a row to staged/retry state. |
| Tenant policy get/set | Canary-guarded policy surface used by capability-token checks. |

Admin bearer comparison fails closed for an empty expected secret, caps bearer
length at 128 bytes, hashes both sides with SHA-256, and compares in constant
time. See the detailed [Watch HTTP API](../gateway/docs/watch-api.md).

### Arm and disarm

| Surface | Behavior |
| --- | --- |
| `POST .../arm` | Legacy endpoint; returns 410 Gone. Use stage and confirm. |
| `POST .../arm/stage` | An arm principal stages a JCS challenge with a default 120-second TTL. |
| `GET .../arm/pending` | Returns the stored challenge bytes for crash recovery; the challenge is not re-derived. |
| `POST .../arm/confirm` | Completes single-operator dual custody: the arm principal authorizes the request and a local Secure Enclave or FIDO2 key attests it. The signed material binds the spend cap and window. |
| Rehearsal or dirty build | Never starts the real producer. |
| `POST .../disarm` | Kill switch accepted from the admin token or any arm principal; drains the writer. |
| Writer claim | Enforces a single writer, with 30-second heartbeat and 90-second stale self-disarm defaults. |
| Boot triple-gate | Starts the producer only when all three boot variables are present; does not create the signed `active_arm` required for spend. |
| Spend ceiling | Hard maximum is $50/day; the environment may only lower it. The canonical runtime sets $25/day and a $2.50 fanout reserve. |
| Signed spend window | Locked by the hardware ceremony and not extendable by an environment change after confirmation. |

Host helpers include `gateway/bin/arm`, `disarm`, `arm-enroll`,
`arm-enroll-fido2`, `arm-attest`, and `verify-attest-keyset-hash`. The complete
operator procedure is [Arming and authorization](../gateway/docs/runbooks/arming-authorization.md).

### Stock Sentinels

| Name | Observation |
| --- | --- |
| `file-inbox-watch` | Polls a filesystem location and matches configured filename patterns. |
| `silence-watch` | Detects ledger silence or backlog age. |
| `gateway-active-watch` | Checks HTTP queue depth or a JSONPath threshold. |
| `watch-health-watch` | Detects quarantined or hard-killed Sentinels and chain breaks. |
| `ledger-delta-watch` | Compares daily spend with a baseline. |
| `anomaly-watch` | Detects failure-rate spikes against an EWMA. |
| `completion-verify-watch` | Detects acknowledged Outbox rows without `VerifiedExact` provenance. |
| `precedent-integrity-watch` | Detects truncation or mutation of the sessions index. |

The registry loads YAML from `SENTINELS_CONFIG_PATH` and fails fast on an
unknown Sentinel name.

### Dispatcher and worker

| Surface | Behavior |
| --- | --- |
| Live dispatcher | `WATCH_DISPATCHER_ENABLED` defaults to false and requires a Gateway key; it claims an escalation, invokes Council triage, and stages a directive. |
| Built-in worker loop | `WATCH_WORKER_ENABLED` defaults to false; when explicitly enabled, it verifies signed envelopes, and prepare/execute work requires capability tokens. |
| Worker HTTP APIs | Claim, heartbeat, ack, worker-ack, and nack routes are mounted independently of the built-in worker-loop flag. They require admin authentication, `X-Tenant-Scope`, and the canary guard. |
| Directive authorities | Limited to `recommend`, `prepare`, and `execute`. |
| Replay epoch | Optional fence on claims and producer work. |
| CDC producer | Converts fires into pending escalations and can wrap them in a comms envelope. |

See [Watch quarantine, probation, and force-wake](../gateway/docs/watch-api.md#quarantine-probation-and-force-wake)
for quarantine defaults and incident handling.

## Gateway core

| Surface | Behavior |
| --- | --- |
| Sidecar UDS HTTP | Axum over a Unix socket: health, guard, ledger, cache, route, budget, policy, auth, admin, Vertex, Council, Librarian, and Watch. |
| Global UDS rate limit | Defaults to 6,000 requests per minute through `SIDECAR_GLOBAL_RPM`; excess requests return 429 with `Retry-After`. |
| Socket permissions | Default `0660`; invalid mode or group configuration fails startup. |
| Virtual API keys | Uses `AUTH_PEPPER` and hashes stored in `auth_keys.json`; absent credentials fail closed. |
| Rate limits | Global, per-IP, and per-key buckets, with configured exempt CIDRs. |
| Admin key lifecycle | Bootstrap or admin-tier provision and revoke; an admin cannot revoke itself. |
| Budget | Defaults to $10 per 24 hours per key; optional SQLite persistence. |
| Cache | `gateway:cache:v5:` prefix with local, Redis, or SQLite backends. |
| Smart router | Scores quality, latency, cost, and risk, with per-family health. |
| Policy and guards | Sensitivity routing, decontamination, shape, sovereignty, and tool checks. |
| Ledger | Sign, verify, and export; verify and export require admin `X-Admin-Key`. |
| Council idempotency | Concurrency cap, durable claim/store, and TTL alignment. |
| Lua/nginx front door | Fails closed if the sidecar is unavailable, enforces shape limits, and proxies the supported routes. |
| Offline ceremony CLI | `gateway-ceremony` supports the air-gapped key path. |

See [Gateway core surfaces](../gateway/docs/gateway-core-surfaces.md) for the
route and implementation detail.

## Sentinel and sovereign-protocol

| Surface | Behavior |
| --- | --- |
| Crate modules | `comms`, `directive`, `escalation`, `jcs`, `fence_vectors`, and `types`. |
| Wire constants | Contract `irin.comms.v0.1`, CloudEvents 1.0, and envelope schema 1. |
| Envelope kinds | Escalation and Directive, using `irin.*.v0.1` type ids. |
| Outer wrapper | `{"v":1,"envelope":...}` around the CloudEvents-shaped envelope. |
| Envelope validation | Fails closed on invalid spec version, type, or content type. |
| JCS | RFC 8785 canonicalization with a non-finite guard, UTF-16 key ordering, stable number formatting, and strict duplicate-key rejection. |
| Provenance types | Gateway, worker, and provider provenance structures. |
| Capability and errors | `CapabilityToken` for sensitive paths and `ProblemDetails` for problem+json responses. |
| Directive fence corpus | Golden vectors consumed by both the Council emitter and Gateway receiver. |

See the [sovereign-protocol implementation notes](../sentinel/docs/protocol-implementation.md)
and frozen [`COMMS_CONTRACT.md`](../sentinel/COMMS_CONTRACT.md).

## Implementation references

- Watch route mounting and boot gates: [`gateway/sidecar-rs/src/main.rs`](../gateway/sidecar-rs/src/main.rs)
- Watch handlers and arm ceremony: [`gateway/sidecar-rs/src/watch/api.rs`](../gateway/sidecar-rs/src/watch/api.rs)
- Producer startup and writer lifecycle: [`gateway/sidecar-rs/src/watch/runner.rs`](../gateway/sidecar-rs/src/watch/runner.rs)
- Built-in worker loop: [`gateway/sidecar-rs/src/watch/worker.rs`](../gateway/sidecar-rs/src/watch/worker.rs)
- Spend authorization and Outbox storage: [`gateway/sidecar-rs/src/watch/db.rs`](../gateway/sidecar-rs/src/watch/db.rs)
- Quarantine state machine: [`gateway/sidecar-rs/src/watch/quarantine.rs`](../gateway/sidecar-rs/src/watch/quarantine.rs)
- Shared protocol source and conformance tests: [`sentinel/sovereign-protocol`](../sentinel/sovereign-protocol)

## Boundaries

- This is not a public-deployment or multi-tenant security claim.
- A configured Sentinel observes and records; it does not authorize escalation,
  provider spend, or execution.
- Starting the producer does not create spend authority. Only a successful
  hardware ceremony creates the signed `active_arm` used by spend checks.
- The built-in worker loop is a default-off development path, while its
  authenticated management APIs are mounted.
- The supported operator path ends at a signed directive artifact.
