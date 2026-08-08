# Seam: worker authority and quarantine execute

**Change class:** in-process worker tick, fail-closed nacks, signed-row identity,
claim/lease through drain, late-ack reconciliation, and the single
`quarantine_producer` execute effect.

Token **shape and authorize policy** live on
[`structured-execute-authority.md`](structured-execute-authority.md). This page
is the worker/effect boundary.

## When this is your PR

- You change `run_worker_tick` / `run_worker_tick_with_quarantine`.
- You change quarantine execute, drain ack, claim lease, or row-identity checks.
- You change typed nacks for recommend/prepare/unimplemented execute.

## Where the change lands

| If you change… | Land here first |
| --- | --- |
| Worker loop / pre-act / nacks | `gateway/sidecar-rs/src/watch/worker.rs` — `run_worker_tick`, `run_worker_tick_with_quarantine` |
| Quarantine effect / arming drain | Worker + `gateway/sidecar-rs/src/watch/api/arming.rs` (producer-disarm path used by quarantine) |
| Token recheck at effect | Same worker path; authorize semantics still owned by `dispatcher.rs` / structured-execute page |
| Durable outbox row / claim state | `gateway/sidecar-rs/src/watch/db/` as touched by worker tests |
| Integration proof | `gateway/sidecar-rs/tests/watch_worker.rs` |

Product naming: action is `quarantine_producer`, subject `watch-producer`. Do not
reintroduce alternate sentinel-quarantine names.

## Authoritative proof (copy-paste order)

From repository root:

1. **Worker suite (default when this seam is in scope):**

   ```bash
   cargo test -p gateway-sidecar --test watch_worker
   ```

2. **Focused gates (when narrowing):**

   ```bash
   cargo test -p gateway-sidecar --test watch_worker test_worker_blocks_invalid_token
   cargo test -p gateway-sidecar --test watch_worker test_worker_blocks_missing_token_fail_closed
   cargo test -p gateway-sidecar --test watch_worker test_worker_recommend_without_token_is_nacked
   cargo test -p gateway-sidecar --test watch_worker test_worker_execute_without_executor_is_nacked
   cargo test -p gateway-sidecar --test watch_worker test_worker_executes_quarantine_producer_and_acks
   cargo test -p gateway-sidecar --test watch_worker test_worker_rejects_mismatched_signed_row_identity
   cargo test -p gateway-sidecar --test watch_worker test_worker_does_not_ack_dropped_drain_ack
   cargo test -p gateway-sidecar --test watch_worker test_worker_claim_survives_bounded_drain
   cargo test -p gateway-sidecar --test watch_worker test_worker_reconciles_late_drain_ack_on_retry
   ```

3. **If structured token policy also changes:** run the structured-execute
   authorize filters on that page, then return here for worker proof.

4. **Path-selected:** `make check` while iterating; `make ship-check` once before
   PR readiness.

5. **`make verify`:** when the change couples to arming/outbox signing path that
   the isolated stack exercises; not required for pure nack-message edits.

## Must not break (test names)

- No fake success for recommend / missing executor:
  `test_worker_recommend_without_token_is_nacked`,
  `test_worker_execute_without_executor_is_nacked`
- Invalid or missing token fails closed:
  `test_worker_blocks_invalid_token`,
  `test_worker_blocks_missing_token_fail_closed`
- Quarantine happy path acks after real effect path:
  `test_worker_executes_quarantine_producer_and_acks`
- Signed row identity:
  `test_worker_rejects_mismatched_signed_row_identity`
- Drain / claim / late ack:
  `test_worker_does_not_ack_dropped_drain_ack`,
  `test_worker_claim_survives_bounded_drain`,
  `test_worker_reconciles_late_drain_ack_on_retry`
- Signature / authority refuse (no silent downgrade):
  `test_worker_rejects_tampered_envelope`,
  `test_worker_rejects_forged_signature_wrong_key`,
  `test_worker_rejects_unpinned_kid`,
  `test_worker_no_verifier_fails_closed`,
  `test_worker_rejects_unknown_authority_no_silent_downgrade`

## Coverage gaps (do not over-claim)

- **`prepare` authority nack** shares the same non-executing branch as
  `recommend` in `worker.rs`, but the suite has a dedicated nack test only for
  recommend (`test_worker_recommend_without_token_is_nacked`). There is no
  focused `prepare` nack regression. If you change prepare handling, add and
  cite a dedicated test in the same PR.

## Do not

- Reintroduce fake effect acks for recommend / stub execute (see
  `test_worker_recommend_without_token_is_nacked` and
  `test_worker_execute_without_executor_is_nacked`).
- Split token authorize policy into a parallel worker-only ruleset — policy
  authority stays on the structured-execute seam; worker rechecks, does not
  redefine wire policy.
- Rename `quarantine_producer` / `watch-producer` without updating goldens and
  this page’s proof list in the same PR.
- Claim worker-off product posture changed unless you intentionally productize
  the loop (default-off remains operator doctrine elsewhere).

## Related

- Token mint/authorize: [`structured-execute-authority.md`](structured-execute-authority.md)
- Receipt projection after execute: [`redacted-execute-receipt.md`](redacted-execute-receipt.md)
