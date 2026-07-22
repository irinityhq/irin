-- =============================================================================
-- §6.4 — Council session aggregation
-- =============================================================================
--
-- Canonical SQL for rolling a council session's total spend out of the audit
-- ledger. Joins leaf rows (the per-seat calls) with the council_wrapper row
-- (the chair synthesis layer) by council_session_id, and confirms that
-- council_replay rows contribute zero.
--
-- Invariants (from COUNCIL_GATEWAY_CONTRACT.md §6.4):
--   1. For any session_id: total_usd = leaf_sum_usd + wrapper_cost_usd
--   2. council_replay rows contribute 0 to total_usd
--   3. The wrapper row's own cost_usd is the chair call's direct cost (the
--      chair LLM invocation). wrapper_cost_usd is what aggregates with the
--      leaf rows — the two are NOT the same field; do not double-count.
--   4. Caller-facing X-Total-Cost-Usd egress header must equal total_usd.
--
-- Usage from the sidecar / a verifier:
--   sqlite3 ledger.db -cmd ".parameter set :session_id 'sess_abc'" \
--     < test/sql/council_644_aggregation.sql
-- =============================================================================

SELECT
    json_extract(payload, '$.council_session_id') AS session_id,

    -- Leaf row sum: per-seat direct cost. cost_usd lives on the payload.
    SUM(CASE WHEN json_extract(payload, '$.kind') = 'leaf'
             THEN COALESCE(json_extract(payload, '$.cost_usd'), 0)
             ELSE 0 END) AS leaf_sum_usd,

    -- Wrapper row: exactly one per session. wrapper_cost_usd is the chair
    -- synthesis layer's billable spend, separate from per-seat costs.
    SUM(CASE WHEN json_extract(payload, '$.kind') = 'council_wrapper'
             THEN COALESCE(json_extract(payload, '$.wrapper_cost_usd'), 0)
             ELSE 0 END) AS wrapper_cost_usd,

    -- Total = leaf_sum + wrapper. council_replay rows are excluded by both
    -- branches above — they kind-match neither 'leaf' nor 'council_wrapper'.
    (SUM(CASE WHEN json_extract(payload, '$.kind') = 'leaf'
              THEN COALESCE(json_extract(payload, '$.cost_usd'), 0) ELSE 0 END)
   + SUM(CASE WHEN json_extract(payload, '$.kind') = 'council_wrapper'
              THEN COALESCE(json_extract(payload, '$.wrapper_cost_usd'), 0) ELSE 0 END)
    ) AS total_usd,

    -- Diagnostics — counts of each row kind under this session.
    SUM(CASE WHEN json_extract(payload, '$.kind') = 'leaf' THEN 1 ELSE 0 END)
        AS leaf_count,
    SUM(CASE WHEN json_extract(payload, '$.kind') = 'council_wrapper' THEN 1 ELSE 0 END)
        AS wrapper_count,
    SUM(CASE WHEN json_extract(payload, '$.kind') = 'council_replay' THEN 1 ELSE 0 END)
        AS replay_count
FROM audit_events
WHERE json_extract(payload, '$.council_session_id') IS NOT NULL
  AND json_extract(payload, '$.council_session_id') = COALESCE(:session_id,
        json_extract(payload, '$.council_session_id'))
GROUP BY session_id;
