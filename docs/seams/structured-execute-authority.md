# Seam: structured execute authority

**Change class:** capability-token wire shape, admin mint, signature verify,
authorize-for-execute, durable replay bind, and opaque-token refuse for execute.

Also called capability-token mint/verify/authorize.

## When this is your PR

- You touch `CapabilityToken`, token JCS preimage, mint, authorize, or execute
  token policy.
- You change who may mint or what fields mint pins server-side.
- You change durable consumption / same-directive retry semantics.

## Where the change lands

| If you change… | Land here first |
| --- | --- |
| Wire / type / field invariants | `sentinel/sovereign-protocol` — `CapabilityToken` in `src/types.rs`; JCS via `to_jcs_bytes` in `src/jcs.rs` |
| Signing preimage golden | `sentinel/sovereign-protocol/tests/wire_golden.rs` — `t22k_capability_token_golden` |
| Admin mint | `gateway/sidecar-rs/src/watch/api/capability_mint.rs` — `mint_capability_token_json`; handler `watch_mint_capability_token` in `src/routes/watch.rs`; route `POST /watch/capability-token/mint` mounted in `src/routes/mod.rs` |
| Sign / verify | `gateway/sidecar-rs/src/keymgmt.rs` — `sign_capability_token` / `verify_capability_token` |
| Authorize / policy / replay | `gateway/sidecar-rs/src/watch/dispatcher.rs` — `is_capability_token_valid`, `bind_capability_token_consumption` |
| DB wrapper / consumption store | `gateway/sidecar-rs/src/watch/db/outbox_store.rs` — `is_capability_token_valid`, consumption helpers; schema only if identity/replay storage shape changes (`src/watch/db/schema.rs`) |
| Worker pre-act recheck | `gateway/sidecar-rs/src/watch/worker.rs` — `run_worker_tick_with_quarantine` (token gate before effect) |
| Proposal fence (token key smuggled in proposal) | `gateway/sidecar-rs/src/watch/startup_probe.rs` + `sentinel/sovereign-protocol/src/vectors/directive_fence_cases.json` |
| Edge exposure of new watch paths | `gateway/nginx.conf` + `gateway/lua/sidecar.lua` — mint is **UDS-only** today; do not place mint under the `/watch/outbox/` prefix tunnel |
| Visible receipt fields | hand off to [`redacted-execute-receipt.md`](redacted-execute-receipt.md) |

## Authoritative proof (copy-paste order)

Run from repository root unless noted.

1. **Wire golden (type or preimage):**

   ```bash
   cargo test -p sovereign-protocol --test wire_golden t22k_capability_token_golden
   ```

2. **Mint contract:**

   ```bash
   cargo test -p gateway-sidecar capability_mint
   ```

3. **Authorize / policy / durable replay (focused):**

   ```bash
   cargo test -p gateway-sidecar --test watch_dispatch_live w2_3b_
   cargo test -p gateway-sidecar --test watch_dispatch_live pr1_structured_execute
   cargo test -p gateway-sidecar --test watch_dispatch_live live_token_store
   ```

   Full suite when the change is broad:

   ```bash
   cargo test -p gateway-sidecar --test watch_dispatch_live
   ```

4. **Worker token gate (if worker path in scope):**

   ```bash
   cargo test -p gateway-sidecar --test watch_worker test_worker_blocks_invalid_token
   cargo test -p gateway-sidecar --test watch_worker test_worker_blocks_missing_token_fail_closed
   cargo test -p gateway-sidecar --test watch_worker test_worker_recommend_without_token_is_nacked
   ```

5. **Proposal fence (if proposal wire in scope):**

   ```bash
   cargo test -p sovereign-protocol --test fence_vectors_golden
   cargo test -p gateway-sidecar --lib validate_rejects_capability_token_at_top_level
   cargo test -p gateway-sidecar --lib validate_rejects_unknown_scope_key_capability_token
   ```

   Council-side fence (when Council proposal path is in scope):

   ```bash
   cargo test -p council-rs rejects_capability_token
   ```

6. **Path-selected product proof:** `make check` while iterating; `make ship-check`
   once before marking the PR ready (repository rule for product PRs).

7. **`make verify`:** only if you change shared signing key lifecycle or JCS
   used by directive envelopes, not for token-policy-only edits.
   `gateway/test/demo.sh` does not exercise capability tokens.

## Must not break (test names)

- Structured denial is terminal (no legacy opaque fallback after structured parse):
  `w2_3b_malformed_policy_cannot_fall_back_to_legacy_token`
- Malformed / DB-error policy fails closed:
  `w2_3b_structured_token_allowed_workers_db_error_fails_closed`
- Empty allowlist remains intentionally open where designed:
  `w2_3b_structured_token_clean_empty_allowlist_still_allowed`
- Durable same-directive retry and foreign refuse (survives DB reopen):
  `pr1_structured_execute_same_directive_retry_and_foreign_refuse`
- Approval and cost guards in isolation:
  `pr1_structured_execute_rejects_false_approval`,
  `pr1_structured_execute_rejects_cost_variants`
- Production JCS preimage (not serde field order):
  `t22k_capability_token_golden`
- Mint refuses and successful mint under process-global key:
  `capability_mint` filter tests in `capability_mint.rs`

## Coverage gaps (do not over-claim)

Mint branches without dedicated tests in-tree as of this checklist: empty-tenant
refusal, signing-key-unavailable (503), and some expiry-cap edges. If you touch
those branches, add focused tests in the same PR.

## Do not

- Fall back from structured validation to legacy opaque matching for execute
  (`w2_3b_malformed_policy_cannot_fall_back_to_legacy_token`).
- Sign goldens with serde field-order bytes (`t22k_capability_token_golden`).
- Verify mint against a per-test signing key instead of `directive_signing_key()`.
- Expose mint under the `/watch/outbox/` prefix tunnel; use exact method+path
  allowlists (see arm bridge pattern in `gateway/lua/sidecar.lua`).
- Let callers set `subject`, `allowed_actions`, `approval_required`, or
  `max_cost_usd` at mint — server pins them.
- Log or project raw token or signature material (receipt seam owns UI shape).
- Restate arming/JCS/worker product doctrine here — link tests above.

## Related

- Worker effect path: [`worker-authority-quarantine.md`](worker-authority-quarantine.md)
- Receipt UI/server projection: [`redacted-execute-receipt.md`](redacted-execute-receipt.md)
