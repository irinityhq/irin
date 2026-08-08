# Seam: redacted execute receipt

**Change class:** tenant-scoped execute receipt projection on the UI snapshot
and the War Room client whitelist parser. Server and client must stay in lockstep.

Token wire/JCS authority is
[`structured-execute-authority.md`](structured-execute-authority.md)
(`t22k_capability_token_golden`). This page is **not** the JCS preimage.

## When this is your PR

- You add, remove, or rename fields on execute receipts in the snapshot.
- You change redaction rules (what must never appear).
- You change War Room parsing of `recent_execute_receipts`.

## Where the change lands

| If you change… | Land here first |
| --- | --- |
| Server projection / list | `gateway/sidecar-rs/src/watch/db/outbox_store.rs` — `list_recent_execute_receipts` |
| Snapshot assembly | `gateway/sidecar-rs/src/watch/api/stats.rs` — `ui_snapshot_json` path that attaches recent receipts |
| Client whitelist / parse | `council-rs/warroom/web/lib/watch-gateway.ts` — `EXECUTE_RECEIPT_KEYS`, `assertRedactedExecuteReceipt` |
| Client proof | `council-rs/warroom/web/lib/watch-gateway.test.ts` |
| Pack / Tauri only if shipping surface changes | `make -C council-rs warroom-check` |

## Authoritative proof (copy-paste order)

1. **Client unit tests (always when parser or field set changes):**

   ```bash
   cd council-rs/warroom/web && npm run test:unit
   ```

   Note: plain `npm test` is Playwright and does **not** run these unit tests.

2. **Full War Room web lane (when web surface is in the PR):**

   ```bash
   make -C council-rs warroom-check
   ```

   or lint + typecheck + `test:unit` in `council-rs/warroom/web/`.

3. **Server projection (when sidecar snapshot/list changes):**

   ```bash
   cargo test -p gateway-sidecar --test watch_api gate4_ui_snapshot
   cargo test -p gateway-sidecar --lib completed_ack_projects_clean_receipt
   cargo test -p gateway-sidecar --lib freeform_last_error_is_dropped_and_lifecycle_stays_out_of_result
   cargo test -p gateway-sidecar --lib lifecycle_decisions_do_not_duplicate_into_result
   ```

   Named integration gates include
   `gate4_ui_snapshot_projects_redacted_execute_receipts`,
   `gate4_ui_snapshot_pending_receipt_has_null_result`,
   `gate4_ui_snapshot_execute_receipt_tail_is_bounded`, and
   `gate4_ui_snapshot_has_exact_whitelist_and_no_raw_values` (matched by the
   `gate4_ui_snapshot` filter above).

4. **Path-selected:** `make check` while iterating; `make ship-check` once before
   PR readiness (required when both Rust and npm lanes are in the diff).

5. **`make verify`:** not required for receipt-redaction-only changes.

## Must not break (test names / contracts)

- Unknown fields and `raw_token` rejected on the client:
  `watch-gateway.test.ts` — "rejects unknown fields including raw_token"
- Decision/action sets finite; lifecycle strings not smuggled in `result`:
  same file — unrestricted decision/action and lifecycle-in-result cases
- Server projection never includes raw token or signature material:
  `gate4_ui_snapshot_has_exact_whitelist_and_no_raw_values`
- Both sides update together: adding a server field without client whitelist
  update fails closed on the UI; removing a required key breaks parse

## Do not

- Put raw capability token, signature, or secret material on the snapshot.
- Expand the client whitelist without a matching server projection change (or
  the reverse).
- Point implementers at COMMS_CONTRACT or JCS goldens for this UI contract —
  authority is the whitelist + server list helpers above.
- Use Playwright (`npm test`) as proof of the parser; use `npm run test:unit`.

## Related

- Token authorize path that produces execute decisions:
  [`structured-execute-authority.md`](structured-execute-authority.md)
- Worker effect path: [`worker-authority-quarantine.md`](worker-authority-quarantine.md)
