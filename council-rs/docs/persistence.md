# Persistence

Council persistence is local-first. Session transcripts, flight records,
Librarian chats, research jobs, and derived indexes are runtime state, not
portable source docs.

## Session Files

Default path:

```text
sessions/council_<timestamp>_<session_id>.json
```

Override with:

```bash
export COUNCIL_SESSIONS_DIR=/path/to/sessions
```

The main session schema is `CouncilSession` in `src/types.rs` and
`schemas/session.v2.schema.json`.

Important fields:

- `session_id`
- `topic`
- `cabinet_name`
- `rounds`
- `synthesis`
- `synthesis_model`
- `total_tokens`
- `total_latency_ms`
- `total_cost_usd`
- `mode`
- `precedent_ids`
- `timestamp`
- `schema_version`
- `tier`
- `budget`
- `context_sources`

New fields in Rust types should use serde defaults when historical sessions
need to remain readable.

## Round and Seat Records

`RoundResult` carries:

- `round_num`
- `responses`
- `convergence_score`
- `converged`
- `judge_provider`
- `judge_assessment`
- `judge_gateway_attempts` (Gateway-owned request IDs for every governed
  convergence-judge cascade candidate attempted in that round)
- `flip_flop_hash`
- `validation_report`

`SeatResponse` carries provider/model identity, text, token counts, cached
tokens, cost, latency, and optional error.

Touching either type requires validating old sessions.

## Precedent Index

Default path:

```text
sessions/index.jsonl
```

Each line is a `PrecedentEntry` derived from a completed `CouncilSession`.

Rebuild:

```bash
./target/release/council --reindex
```

Validate:

```bash
python3 tests/validate_index.py --strict
```

The keyword precedent path in `src/precedent/mod.rs` is separate from the
semantic War Room embedding features. Do not assume every search path uses the
same scoring.

## Flight Records

Default path:

```text
runs/<timestamp>_<session_id>_status.md
```

Override with:

```bash
export COUNCIL_RUNS_DIR=/path/to/runs
```

Flight records are operator summaries. They are useful for review but should
not be treated as the canonical session schema. They include the complete
chair synthesis; use the session JSON for complete per-seat response text and
schema-stable transcript fields.

## War Room Local State

Additional War Room state can include:

- `sessions/lineage.jsonl`
- `sessions/intervention_log.jsonl`
- drift reports under `runs/`
- weekly drift JSON under `runs/`
- meta-review reports under `runs/`
- research jobs persisted by `src/warroom/research_store.rs`
- local Librarian chat JSON under `librarian_chats/`

These are local operational assets. Back them up deliberately if they matter.

## Librarian Chat Storage

Council stores local chat wrapper state for the War Room Librarian tab. The
Librarian service remains the retrieval/generation authority.

Current constraints from `src/librarian/routes.rs` and storage modules include:

- 2 MiB hard cap per chat file;
- 64 KiB assistant cap;
- 1 KiB source snippet cap;
- 20 sources max per turn;
- user content max 8192 bytes;
- title max 120 bytes;
- cabinet name max 64 bytes;
- `client_msg_id` max 64 bytes;
- optimistic concurrency with `If-Match` on chat patch.

## Migration Awareness

Historical sessions span Python and Rust eras. The Rust types intentionally
deserialize older sessions leniently in places like `mode`, token/cost fields,
and schema version defaults.

Before changing persistence:

```bash
cargo test --all-targets
python3 tests/validate_session.py --strict
python3 tests/validate_index.py --strict
```

Do not delete or rewrite historical sessions without a separate migration plan.
