# Watch Plane Retention

How long the watch-plane store (`sidecar-rs` SQLite `watch.db`) keeps rows, and
which rows it never deletes. This is operator/orientation material — source of
truth is `sidecar-rs/src/watch/db.rs` and `sidecar-rs/src/watch/runner.rs`.

## At a glance

| Table | What it holds | Retention |
| --- | --- | --- |
| `watch_fires` | Append-only, hash-chained fire and lifecycle log | **Unbounded by design** — never pruned |
| `pending_escalations` | Escalation lifecycle state machine | Terminal rows pruned at **7d** (hardcoded) |
| `directive_outbox` | Signed directive outbox | Terminal rows pruned at **7d** (hardcoded) |

## Fire log is unbounded BY DESIGN

`watch_fires` is an append-only, per-tenant hash-chained ledger (each row's
`hash` covers the prior row's `prev_hash`). Deleting any row would break
hash-chain contiguity and make `GET /watch/verify-chain/{tenant}` fail. The
pruner therefore **never touches `watch_fires`** (contiguity mandate,
`db.rs:1152`). Growth is bounded operationally (backup + archive), not by
in-process deletion. The `test_prune_never_touches_fire_log` test in
`sidecar-rs/tests/watch_prune.rs` pins this invariant.

## Terminal-row pruning (7d, hardcoded)

`WatchDb::prune_terminal_rows(older_than_ms)` (`db.rs:2284`) deletes rows whose
`created_at_ms < older_than_ms` **and** whose `status` is terminal. It is called
once per hour by `pruning_loop` (`runner.rs:1114`, spawned at `runner.rs:289`)
with `older_than_ms = now - 7 days`. The 7-day window is hardcoded in the loop.

Terminal status sets (only these are pruned):

- `pending_escalations`: `outbox_written`, `dismissed`, `expired`,
  `dead_lettered`.
  - Non-terminal states (`queued`, `claimed`, `council_response_staged`) are
    **never** pruned regardless of age.
  - `failed` is terminal in the lifecycle but is **not** in the prune SQL today,
    so `failed` rows are not deleted by the pruner (tracked as an open item — see
    below).
- `directive_outbox`: SQL targets `acked`, `nacked`, `expired`. The table CHECK
  constraint only admits `staged`, `dismissed`, `expired`, `acked`, so `nacked`
  is unreachable and the **effective** prune set is `acked` + `expired`.
  `staged` and `dismissed` are kept.

Boundary semantics: the predicate is `created_at_ms < older_than_ms` (strict).
A row exactly at the boundary (`created_at_ms == older_than_ms`) is **kept**; a
row 1ms older is pruned; a row 1ms newer is kept.

Foreign key: `directive_outbox` references `pending_escalations`
`(tenant, in_response_to)` with `foreign_keys=ON`. A parent escalation cannot be
pruned while a child directive row still exists.

## Open items

- **`tenant_policies.retention_days` is currently ignored.** The column exists
  (`db.rs:246`, read at `db.rs:405`) but the pruner uses the hardcoded 7-day
  window and does not consult per-tenant policy. Wiring per-tenant retention into
  `pruning_loop`/`prune_terminal_rows` is tracked separately; do not assume the
  policy value is enforced today.
- **`failed` escalations are not pruned.** They accumulate until handled by other
  means. Decide whether `failed` should join the terminal prune set.

## Tests

`sidecar-rs/tests/watch_prune.rs` covers: terminal-only deletion, the
non-terminal safety property, the retention-window boundary (both tables), and
the fire-log untouched invariant. Run:

```
cd sidecar-rs && cargo test --test watch_prune
```
