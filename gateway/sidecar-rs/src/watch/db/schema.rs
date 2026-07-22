//! Schema DDL and migrations for watch.db.

use std::path::Path;

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use tokio_rusqlite::Connection;

use crate::watch::outbox::DirectiveAuthority;

use super::{arm_audit_distinct_genesis, compute_arm_audit_preimage, WatchDb};

fn schema_v1() -> String {
    format!(
        r#"
CREATE TABLE IF NOT EXISTS watch_schema (
    version INTEGER NOT NULL PRIMARY KEY
);
INSERT OR IGNORE INTO watch_schema (version) VALUES (1);

CREATE TABLE IF NOT EXISTS watch_sentinels (
    name             TEXT NOT NULL,
    tenant           TEXT NOT NULL,
    tier             TEXT NOT NULL CHECK(tier IN ('fast','polling','deep')),
    cooldown_ms      INTEGER NOT NULL,
    config_json      TEXT NOT NULL,
    enabled          INTEGER NOT NULL DEFAULT 1,
    hard_killed_at   INTEGER,
    hard_kill_reason TEXT,
    probation_until  INTEGER,
    PRIMARY KEY (tenant, name)
);

-- -----------------------------------------------------------------------------
-- Audit-chain integrity guard.
-- The watch_fires table is the cryptographic audit chain for the watch plane.
-- It is STRICTLY APPEND-ONLY. You MUST NOT author any query, function, or
-- automated process that executes DELETE or UPDATE against this table.
-- Compaction, TTL-based pruning, or "cleanup" of this table is forbidden.
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS watch_fires (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant                  TEXT NOT NULL,
    sentinel                TEXT NOT NULL,
    fired_at                INTEGER NOT NULL,
    state_json              TEXT NOT NULL,
    reason                  TEXT NOT NULL,
    prev_hash               TEXT NOT NULL,
    hash                    TEXT NOT NULL UNIQUE,
    envelope_json           TEXT NOT NULL,
    envelope_schema_version INTEGER NOT NULL DEFAULT 1,
    -- W3 item 3: hash-preimage scheme selector (NOT the envelope_schema_version,
    -- which versions the envelope payload, not the hash field set). DEFAULT 3 is
    -- the LEGACY BACKFILL ONLY — every new INSERT must EXPLICITLY bind 4 (see the
    -- insert paths). A forgotten bind would silently default a new row to 3 →
    -- envelope_json unhashed → the exact bug this closes, invisibly.
    preimage_version        INTEGER NOT NULL DEFAULT 3
);
CREATE INDEX IF NOT EXISTS idx_watch_fires_tenant_fired
    ON watch_fires(tenant, fired_at DESC);
CREATE INDEX IF NOT EXISTS idx_watch_fires_sentinel
    ON watch_fires(sentinel);

-- W3 item 2: engine-enforced append-only on the watch-plane audit chain.
-- Mirrors arm_audit's trg_arm_audit_no_update / _no_delete (db.rs ~736) VERBATIM
-- — the "STRICTLY APPEND-ONLY" comment above was the ONLY guard before this;
-- siblings (arm_audit, directive_outbox) already carry engine triggers.
-- DROP-ON-REBUILD HAZARD: if watch_fires ever needs a CHECK change or table
-- rebuild, these triggers MUST be explicitly DROPped and re-CREATEd around it
-- (as arm_audit does at its migration sites ~db.rs:1284 / ~1351) — a rebuild
-- silently drops triggers, and append-only enforcement would be lost without a
-- compile error to catch it.
CREATE TRIGGER IF NOT EXISTS trg_watch_fires_no_update
BEFORE UPDATE ON watch_fires
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'watch_fires_append_only');
END;

CREATE TRIGGER IF NOT EXISTS trg_watch_fires_no_delete
BEFORE DELETE ON watch_fires
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'watch_fires_append_only');
END;

CREATE TABLE IF NOT EXISTS watch_quarantine (
    tenant                TEXT NOT NULL,
    sentinel              TEXT NOT NULL,
    quarantined_until     INTEGER NOT NULL,
    monotonic_at_set      INTEGER NOT NULL,
    duration_ms           INTEGER NOT NULL,
    consecutive_fails     INTEGER NOT NULL,
    consecutive_successes INTEGER NOT NULL DEFAULT 0,
    cycle_count           INTEGER NOT NULL DEFAULT 0,
    cycles_window_start   INTEGER,
    last_error            TEXT,
    last_quarantine_end   INTEGER,
    PRIMARY KEY (tenant, sentinel)
);

-- Phase 3a closed-signal-loop storage: dispatcher state machine.
-- C11 keeps raw escalation ids tenant-scoped instead of globally unique.
CREATE TABLE IF NOT EXISTS pending_escalations (
    id                    TEXT NOT NULL,
    tenant                TEXT NOT NULL,
    sentinel_name         TEXT NOT NULL,
    envelope_json         TEXT NOT NULL,
    status                TEXT NOT NULL,
    attempts              INTEGER NOT NULL DEFAULT 0,
    last_error            TEXT,
    council_response_json TEXT,
    directive_id          TEXT,
    created_at_ms         INTEGER NOT NULL,
    claimed_at_ms         INTEGER,
    claimed_until_ms      INTEGER,
    next_retry_at_ms      INTEGER,
    causal_fire_id        TEXT,
    replay_epoch          INTEGER NOT NULL DEFAULT 0,
    claim_token           TEXT,
    realized_cost_usd     REAL,
    reserved_estimate_usd REAL,
    reserved_day_bucket   TEXT,
    FOREIGN KEY (directive_id) REFERENCES directive_outbox(id),
    CHECK (status IN (
        'queued','claimed','council_response_staged','outbox_written',
        'dismissed','failed','expired','dead_lettered'
    )),
    PRIMARY KEY (tenant, id)
);
CREATE INDEX IF NOT EXISTS idx_pe_status_retry
    ON pending_escalations(status, next_retry_at_ms);
CREATE INDEX IF NOT EXISTS idx_pe_tenant_created
    ON pending_escalations(tenant, created_at_ms DESC);
CREATE INDEX IF NOT EXISTS idx_pe_claimed_age
    ON pending_escalations(status, claimed_at_ms)
    WHERE status = 'claimed';

-- Phase 1 weld: consumer-side dedup on stable causal identity (P0-4 / plan §5 Step 2 / phase1-producer-seam §3).
-- Additive for upgrade compat (existing rows get NULL; new rows set non-NULL causal_fire_id).
-- UNIQUE allows multiple NULLs (legacy); new rows with value collapse duplicates via ON CONFLICT in producer.
CREATE UNIQUE INDEX IF NOT EXISTS idx_pe_causal_dedup
    ON pending_escalations(tenant, sentinel_name, causal_fire_id);

-- Phase 3a durable signed directive outbox.
-- Recovery idempotency is tenant-scoped on (tenant, in_response_to).
CREATE TABLE IF NOT EXISTS directive_outbox (
    id                      TEXT PRIMARY KEY,
    in_response_to          TEXT NOT NULL,
    tenant                  TEXT NOT NULL,
    status                  TEXT NOT NULL,
    verdict                 TEXT NOT NULL,
    authority               TEXT NOT NULL,
    envelope_json           TEXT NOT NULL,
    envelope_json_canonical TEXT NOT NULL,
    signature_b64           TEXT NOT NULL,
    signing_kid             TEXT NOT NULL,
    council_session_id      TEXT,
    council_cost_usd        REAL,
    created_at_ms           INTEGER NOT NULL,
    expires_at_ms           INTEGER NOT NULL,
    acked_at_ms             INTEGER,
    claimed_until_ms        INTEGER,
    claim_count             INTEGER NOT NULL DEFAULT 0,
    last_error              TEXT,
    claim_handle            TEXT,
    worker_provenance       TEXT, -- JCS WorkerProvenanceGuard (VerifiedExact post-ack); claim_handle retained for leases; legacy via from_row col18 fallback
    CHECK (status IN ('staged','dismissed','expired','acked')),
    CHECK (verdict IN ('Act','Dismiss')),
    CHECK (authority IN ({})),
    UNIQUE (tenant, in_response_to),
    FOREIGN KEY (tenant, in_response_to)
        REFERENCES pending_escalations(tenant, id)
        DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX IF NOT EXISTS idx_do_tenant_status_created
    ON directive_outbox(tenant, status, created_at_ms DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_do_expires
    ON directive_outbox(status, expires_at_ms);
CREATE INDEX IF NOT EXISTS idx_do_claimed_until
    ON directive_outbox(claimed_until_ms);


CREATE TABLE IF NOT EXISTS tenant_policies (
    tenant TEXT PRIMARY KEY,
    allowed_models TEXT,
    max_cost_usd REAL,
    max_latency_ms INTEGER,
    allowed_sentinels TEXT,
    allowed_workers TEXT,
    retention_days INTEGER
    -- NOTE: legacy `redaction_enabled BOOLEAN NOT NULL DEFAULT 1` was a no-op
    -- (loaded, never enforced) and is absent from the current schema. Existing
    -- databases keep the dead column harmlessly:
    -- CREATE TABLE IF NOT EXISTS is a no-op on them, INSERTs omit it (its
    -- DEFAULT 1 satisfies NOT NULL), and SELECTs name columns explicitly. No
    -- destructive migration is performed.
);

CREATE TABLE IF NOT EXISTS tenant_policy_tokens (
    tenant      TEXT NOT NULL,
    token       TEXT NOT NULL,
    authority   TEXT NOT NULL CHECK(authority IN ('prepare','execute')),
    PRIMARY KEY (tenant, token)
);

CREATE TRIGGER IF NOT EXISTS trg_do_immutable_signed_fields
BEFORE UPDATE OF
    envelope_json_canonical, signature_b64, signing_kid,
    in_response_to, created_at_ms, tenant, verdict, authority,
    envelope_json, council_session_id, council_cost_usd
ON directive_outbox
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'directive_outbox_immutable_field');
END;

CREATE TRIGGER IF NOT EXISTS trg_do_monotonic_created_at
BEFORE INSERT ON directive_outbox
FOR EACH ROW
WHEN NEW.created_at_ms < COALESCE(
    (SELECT MAX(created_at_ms) FROM directive_outbox WHERE tenant = NEW.tenant), 0
)
BEGIN
    SELECT RAISE(ABORT, 'directive_outbox_created_at_regression');
END;

-- atomic spend ledger (Invariant + :5763): serialized reserve/settle
-- spend ledger keyed on the UTC calendar day. Replaces the BEFORE-INSERT SUM
-- check that was barred from precedent. Cap window is the calendar UTC day
-- (day_bucket = 'YYYY-MM-DD' derived in Rust from epoch-millis integer math,
-- never SQLite localtime/'now') -- this retires the rolling now-24h window that
-- was the Q5 double-spend hazard. reserved_usd accrues on claim and is backed
-- out on settle/release; settled_usd accrues the realized truth.
CREATE TABLE IF NOT EXISTS spend_ledger (
    day_bucket   TEXT NOT NULL,
    reserved_usd REAL NOT NULL DEFAULT 0.0,
    settled_usd  REAL NOT NULL DEFAULT 0.0,
    PRIMARY KEY (day_bucket)
);

-- watch telemetry (telemetry invariant): out-of-band reconciliation alarms. One
-- row per recon tick whose |local settled - external billing| divergence
-- exceeded RECON_DIVERGENCE_THRESHOLD_USD. The EXTERNAL number comes from an
-- out-of-band source (operator-dropped provider export, or a provider usage
-- API) — never from this database — so the row preserves both sides of the
-- cross-check even when the local ledger is the buggy party. source is the
-- ReconSource name ('file_import' / 'provider_usage').
CREATE TABLE IF NOT EXISTS recon_alarm (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms          INTEGER NOT NULL,
    day_bucket     TEXT NOT NULL,
    local_usd      REAL NOT NULL,
    external_usd   REAL NOT NULL,
    divergence_usd REAL NOT NULL,
    source         TEXT NOT NULL
);

-- single-writer (single-writer invariant): the SQLite advisory-lock equivalent. The
-- CHECK(singleton = 1) + PRIMARY KEY make a second claim row physically
-- impossible — exactly one writer_claim row can ever exist. Acquisition is
-- an atomic UPDATE...RETURNING inside BEGIN IMMEDIATE
-- (try_acquire_writer_claim); liveness is heartbeat_at_ms
-- (heartbeat_writer_claim, refuses-to-arm/self-disarms on 0 rows); crash
-- recovery is the stale-takeover predicate
-- (heartbeat_at_ms < now - WRITER_CLAIM_STALE_MS). Single-writer assumes a
-- single SHARED watch.db (the declared topology): two sidecars pointed at
-- DIFFERENT db files would both believe they are sole writer. See
-- docs/runbooks/arming-authorization.md.
CREATE TABLE IF NOT EXISTS writer_claim (
    singleton       INTEGER PRIMARY KEY CHECK (singleton = 1),
    instance_uuid   TEXT NOT NULL,
    boot_at_ms      INTEGER NOT NULL,
    heartbeat_at_ms INTEGER NOT NULL
);

-- -----------------------------------------------------------------------------
-- STRICT GOVERNANCE GUARDRAIL (p0a-four-eyes arming audit — the dual-custody invariant)
-- arm_audit is the append-only, hash-chained record of every arming-ceremony
-- action (stage / confirm / disarm) AND every authn/authz rejection. It is
-- STRICTLY APPEND-ONLY. You MUST NOT author any query, function, or automated
-- process that executes DELETE or UPDATE against this table. Unlike the
-- comment-only guardrail on watch_fires, the two triggers below enforce
-- append-only at the engine level (precedent: trg_do_immutable_signed_fields).
-- Chain shape mirrors watch_fires: prev_hash links each row to its
-- predecessor; the first row links to the frozen distinct arm_audit genesis.
-- principal stores the
-- principal NAME, never the token. stage_rehearsal/confirm_rehearsal are the
-- dual-custody-local-attest B5 rehearsal-harness actions (spec §8): the full
-- ceremony minus arm_producer_start, recorded on the same chain. boot_env_arm
-- detail records the EXECUTION_MODE value seen by the boot env-arm path.
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS arm_audit (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms     INTEGER NOT NULL,
    action    TEXT NOT NULL CHECK(action IN ('stage','confirm','disarm','stage_rejected','confirm_rejected','boot_env_arm','stage_rehearsal','confirm_rehearsal')),
    principal TEXT NOT NULL,
    detail    TEXT,
    prev_hash TEXT NOT NULL,
    hash      TEXT NOT NULL UNIQUE
);

CREATE TRIGGER IF NOT EXISTS trg_arm_audit_no_update
BEFORE UPDATE ON arm_audit
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'arm_audit_append_only');
END;

CREATE TRIGGER IF NOT EXISTS trg_arm_audit_no_delete
BEFORE DELETE ON arm_audit
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'arm_audit_append_only');
END;

-- -----------------------------------------------------------------------------
-- arm_otc — RETIRED (dual-custody-local-attest §9, review).
-- Was the RIDER A one-time confirm-code store (the invariant). The OTC
-- mechanism is retired; no code path reads or writes this table any more.
-- The table is ARCHIVED IN PLACE (archive-never-delete): existing rows are
-- ceremony history, inspectable alongside arm_audit. The CREATE stays so the
-- schema is stable across fresh and upgraded DBs.
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS arm_otc (
    code_hash       TEXT PRIMARY KEY,
    principal       TEXT NOT NULL,
    installed_at_ms INTEGER NOT NULL,
    used_at_ms      INTEGER,
    used_detail     TEXT
);

-- -----------------------------------------------------------------------------
-- arm_pending (dual-custody-local-attest B1 — spec §4.3, review)
-- The persisted pending arm stage (mandate 7: "persisted pending-state").
-- Single row by ceremony discipline: a new stage REPLACES any prior pending
-- row inside the same tx that writes its 'stage' arm_audit row, so the durable
-- stage and its audit record cannot diverge. Expiry is WALL-CLOCK (exp_at_ms)
-- so it survives restart — the in-memory StagedArm monotonic-Instant TTL is a
-- cache of this row, rehydrated at boot (expired rows are never rehydrated).
-- challenge_bytes holds the verbatim JCS challenge produced ONCE at stage time
-- (B2); it is served base64 over GET /arm/pending and verified against these
-- exact stored bytes — never a re-derivation. Cleared on confirm, expiry, or
-- disarm. Armed state itself still does NOT persist (env-gate unchanged).
-- -----------------------------------------------------------------------------
-- T1 MF-1 (content-binding, review): the 4 content-binding
-- fields are persisted next to the verbatim challenge so confirm re-derives and
-- STRICT-EQUALITY-compares against the staged truth (build_id is the EMBEDDED
-- constant), and GET /arm/pending renders the PERSISTED intent (B3 — never live
-- ambient). effective_daily_cap_cents is INTEGER CENTS (B5, no float on the
-- signed path). challenge_format_version carries a DB-layer downgrade floor
-- (B2): CHECK(>= 2) rejects any v1 row at the data layer, not just the binary.
CREATE TABLE IF NOT EXISTS arm_pending (
    stage_id        TEXT PRIMARY KEY,
    staged_by       TEXT NOT NULL,
    challenge_bytes BLOB NOT NULL,
    exp_at_ms       INTEGER NOT NULL,
    -- B7 (spec §8): 1 = rehearsal ceremony — same stage/confirm paths, same
    -- crypto, *_rehearsal audit actions, and the producer NEVER starts. The
    -- ROW decides (stored at stage time); a confirm request cannot upgrade a
    -- rehearsal stage into a real arm.
    rehearsal       INTEGER NOT NULL DEFAULT 0,
    build_id                  TEXT    NOT NULL DEFAULT '',
    enabled_surface           TEXT    NOT NULL DEFAULT '',
    effective_daily_cap_cents INTEGER NOT NULL DEFAULT 0,
    tenant                    TEXT    NOT NULL DEFAULT '',
    challenge_format_version  INTEGER NOT NULL DEFAULT 2
        CHECK (challenge_format_version >= 2)
);

-- -----------------------------------------------------------------------------
-- active_arm (Attested-arm B1 — the attested spend ceiling; review)
-- The SINGLE source of "may spend now, and up to how much". Written in the SAME
-- confirm tx that consumes arm_pending on a REAL (non-rehearsal, non-DARK)
-- confirm; DELETEd in the same tx that disarms; read by the reserve atomic
-- (claim_next_queued_or_failed_with_lease_and_epoch) as an ABSOLUTE ceiling —
-- ambient daily_spend_cap() may only NARROW it, never raise it. No row → the
-- reserve refuses real funds (fail-closed; DARK/never-armed producers do not
-- spend). STRUCTURAL singleton: CHECK(id = 0) + PRIMARY KEY make a second row
-- physically impossible (precedent: writer_claim's singleton). STRICT typing so
-- a TEXT cap can never sit where an i64 is read. armed_epoch is MONOTONIC: the
-- confirm upsert only overwrites a LOWER epoch (re-arm bumps it), and the
-- reserve cross-checks the producer's INDEPENDENTLY-captured epoch against it
-- (P0-2 teeth — a producer armed under epoch N cannot spend after re-arm to
-- N+1). effective_daily_cap_cents is INTEGER CENTS (no float on the ceiling
-- decision); CHECK(>= 1) rejects a sub-cent/zero cap at the data layer.
-- -----------------------------------------------------------------------------
-- Signed-material invariant: the reserve MUST
-- re-verify the ES256 signature at spend time — `active_arm` is a plain table a
-- watch.db-write attacker can forge, so the cap/epoch/build columns are NOT a
-- security anchor on their own. The signed material (the verbatim challenge
-- bytes signed by the attestation format, the DER signature, and the credential identity) is
-- persisted here, NOT deleted with arm_pending, so the reserve can re-run the
-- SAME hardware-attestation verify against the boot keyset. Forging a higher cap
-- now requires a valid signature = a real hardware tap. challenge_bytes is also
-- the source the reserve deserializes to assert signed-content == columns.
-- authenticator_data / client_data_json are NULL for se-p256, populated for the
-- fido2-es256 (native / browser) legs so the exact confirm-time verify replays.
CREATE TABLE IF NOT EXISTS active_arm (
    id                        INTEGER PRIMARY KEY CHECK(id = 0),
    build_id                  TEXT    NOT NULL,
    enabled_surface           TEXT    NOT NULL,
    effective_daily_cap_cents INTEGER NOT NULL CHECK(effective_daily_cap_cents >= 1),
    tenant                    TEXT    NOT NULL,
    armed_epoch               INTEGER NOT NULL,
    exp_at_ms                 INTEGER NOT NULL,
    challenge_sha256          TEXT    NOT NULL,
    audit_id                  INTEGER NOT NULL,
    challenge_bytes           BLOB    NOT NULL,
    signature_der             BLOB    NOT NULL,
    credential_id             TEXT    NOT NULL,
    credential_type           TEXT    NOT NULL,
    authenticator_data        BLOB,
    client_data_json          BLOB
) STRICT;

CREATE TABLE IF NOT EXISTS precedent_integrity_state (
    path          TEXT PRIMARY KEY,
    line_count    INTEGER NOT NULL,
    prefix_sha256 TEXT NOT NULL,
    byte_len      INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

-- -----------------------------------------------------------------------------
-- arm_attest_counters (dual-custody-local-attest B4 — spec §6)
-- FIDO2 monotonic signature counters, strictly increasing, enforced INSIDE the
-- one-transaction confirm. Keyed on credential_id; only `fido2-es256`
-- credentials write here — Secure Enclave has no counter and se-p256 NEVER
-- touches this table (counter logic keys on credential_type only, council
-- 1aba8e1d-445 action 6). Rollback-replay via host write access is a
-- documented accepted residual (spec §14, coupled to the invariant).
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS arm_attest_counters (
    credential_id TEXT PRIMARY KEY,
    last_counter  INTEGER NOT NULL
);
"#,
        DirectiveAuthority::sql_check_literals()
    )
}

impl WatchDb {
    /// Open the watch.db at `path` with the same PRAGMA bundle as
    /// council_idem.db: WAL + synchronous=NORMAL + busy_timeout=50ms +
    /// foreign_keys=ON.
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).await?;
        conn.call(|conn| {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "busy_timeout", 50i64)?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            Ok::<(), rusqlite::Error>(())
        })
        .await?;
        Ok(Self { conn })
    }

    pub async fn run_migrations(&self) -> anyhow::Result<()> {
        self.conn
            .call(|conn| {
                let schema = schema_v1();
                conn.execute_batch(&schema)?;
                // Phase 1 weld (additive, idempotent): ensure causal_fire_id column + dedup index
                // for upgraded DBs (fresh DBs get it from SCHEMA_V1 CREATE + INDEX above).
                // Uses table_info guard (no "ADD COLUMN IF NOT EXISTS" in this SQLite).
                // Matches design: nullable for legacy row compat; producer always sets for new.
                let has_causal: i64 = conn
                    .prepare("PRAGMA table_info(pending_escalations)")?
                    .query_map([], |r| {
                        let name: String = r.get(1)?;
                        Ok(if name == "causal_fire_id" { 1 } else { 0 })
                    })?
                    .filter_map(Result::ok)
                    .sum();
                if has_causal == 0 {
                    conn.execute(
                        "ALTER TABLE pending_escalations ADD COLUMN causal_fire_id TEXT",
                        [],
                    )?;
                }
                let pe_cols: Vec<String> = conn
                    .prepare("PRAGMA table_info(pending_escalations)")?
                    .query_map([], |r| r.get(1))?
                    .filter_map(Result::ok)
                    .collect();
                if !pe_cols.iter().any(|c| c == "replay_epoch") {
                    conn.execute(
                        "ALTER TABLE pending_escalations ADD COLUMN replay_epoch INTEGER NOT NULL DEFAULT 0",
                        [],
                    )?;
                }
                if !pe_cols.iter().any(|c| c == "claimed_until_ms") {
                    conn.execute("ALTER TABLE pending_escalations ADD COLUMN claimed_until_ms INTEGER", [])?;
                }
                if !pe_cols.iter().any(|c| c == "claim_token") {
                    conn.execute("ALTER TABLE pending_escalations ADD COLUMN claim_token TEXT", [])?;
                }
                if !pe_cols.iter().any(|c| c == "realized_cost_usd") {
                    conn.execute("ALTER TABLE pending_escalations ADD COLUMN realized_cost_usd REAL", [])?;
                }
                // atomic spend ledger: stamp the reservation on the claim row so settle/release
                // know what to back out (additive, nullable; follows the has_col guard pattern).
                if !pe_cols.iter().any(|c| c == "reserved_estimate_usd") {
                    conn.execute("ALTER TABLE pending_escalations ADD COLUMN reserved_estimate_usd REAL", [])?;
                }
                if !pe_cols.iter().any(|c| c == "reserved_day_bucket") {
                    conn.execute("ALTER TABLE pending_escalations ADD COLUMN reserved_day_bucket TEXT", [])?;
                }

                // Phase 3c: Outbox hardening (leases, backoff).
                let do_cols: Vec<String> = conn
                    .prepare("PRAGMA table_info(directive_outbox)")?
                    .query_map([], |r| r.get(1))?
                    .filter_map(Result::ok)
                    .collect();

                if !do_cols.iter().any(|c| c == "claimed_until_ms") {
                    conn.execute("ALTER TABLE directive_outbox ADD COLUMN claimed_until_ms INTEGER", [])?;
                }
                if !do_cols.iter().any(|c| c == "claim_count") {
                    conn.execute("ALTER TABLE directive_outbox ADD COLUMN claim_count INTEGER NOT NULL DEFAULT 0", [])?;
                }
                if !do_cols.iter().any(|c| c == "last_error") {
                    conn.execute("ALTER TABLE directive_outbox ADD COLUMN last_error TEXT", [])?;
                }
                if !do_cols.iter().any(|c| c == "claim_handle") {
                    conn.execute("ALTER TABLE directive_outbox ADD COLUMN claim_handle TEXT", [])?;
                }
                if !do_cols.iter().any(|c| c == "worker_provenance") {
                    conn.execute("ALTER TABLE directive_outbox ADD COLUMN worker_provenance TEXT", [])?;
                }

                // W3 item 3: preimage_version selector on watch_fires (additive,
                // has-col guard). DEFAULT 3 BACKFILLS every pre-existing row to
                // v3 — those rows were hashed under the 6-field preimage, so they
                // must verify as v3 (no envelope_json in the hash). New rows bind
                // 4 explicitly at the INSERT sites. No table rebuild; no script.
                let wf_cols: Vec<String> = conn
                    .prepare("PRAGMA table_info(watch_fires)")?
                    .query_map([], |r| r.get(1))?
                    .filter_map(Result::ok)
                    .collect();
                if !wf_cols.iter().any(|c| c == "preimage_version") {
                    conn.execute(
                        "ALTER TABLE watch_fires ADD COLUMN preimage_version INTEGER NOT NULL DEFAULT 3",
                        [],
                    )?;
                }

                conn.execute(
                    "CREATE INDEX IF NOT EXISTS idx_do_claimed_until ON directive_outbox(claimed_until_ms)",
                    [],
                )?;
                // Index is IF NOT EXISTS in SCHEMA; ensure here too for safety on upgrade path.
                conn.execute(
                    "CREATE UNIQUE INDEX IF NOT EXISTS idx_pe_causal_dedup
                     ON pending_escalations(tenant, sentinel_name, causal_fire_id)",
                    [],
                )?;

                // B7 (spec §8): rehearsal flag on the pending stage (additive,
                // has_col guard pattern — fresh DBs get it from the CREATE).
                let ap_cols: Vec<String> = conn
                    .prepare("PRAGMA table_info(arm_pending)")?
                    .query_map([], |r| r.get(1))?
                    .filter_map(Result::ok)
                    .collect();
                if !ap_cols.iter().any(|c| c == "rehearsal") {
                    conn.execute(
                        "ALTER TABLE arm_pending ADD COLUMN rehearsal INTEGER NOT NULL DEFAULT 0",
                        [],
                    )?;
                }

                // T1 MF-1 (B2/B5): the content-binding columns + the
                // challenge_format_version downgrade floor. SQLite cannot ALTER
                // a column-level CHECK onto an existing table, so the floor
                // (`CHECK(challenge_format_version >= 2)`) only takes effect via
                // the CREATE. An upgraded DB whose arm_pending predates these
                // columns therefore REBUILDS the table: a pending row is
                // ephemeral (cleared on confirm/expiry/disarm and never carries
                // armed state), and any pre-upgrade v1 challenge is unconfirmable
                // under v2 anyway — so dropping it is the fail-closed action, and
                // the rebuilt table carries the DB-layer CHECK that rejects any
                // future v1 row at the data layer (B2), not just in the binary.
                let ap_cols_v2: Vec<String> = conn
                    .prepare("PRAGMA table_info(arm_pending)")?
                    .query_map([], |r| r.get(1))?
                    .filter_map(Result::ok)
                    .collect();
                let ap_sql: Option<String> = conn
                    .query_row(
                        "SELECT sql FROM sqlite_master WHERE type='table' AND name='arm_pending'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                // hardening MED: verify the actual `>= 2` CHECK
                // clause is present, not merely the column name — a table with
                // the column but a missing/weakened CHECK must still REBUILD so
                // the data-layer downgrade floor (B2) is real, not assumed.
                let ap_has_check = ap_sql
                    .as_deref()
                    .is_some_and(|sql| sql.contains("challenge_format_version >= 2"));
                if !ap_cols_v2.iter().any(|c| c == "challenge_format_version") || !ap_has_check {
                    let tx = conn
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    tx.execute("DROP TABLE IF EXISTS arm_pending", [])?;
                    tx.execute(
                        "CREATE TABLE arm_pending (
                            stage_id        TEXT PRIMARY KEY,
                            staged_by       TEXT NOT NULL,
                            challenge_bytes BLOB NOT NULL,
                            exp_at_ms       INTEGER NOT NULL,
                            rehearsal       INTEGER NOT NULL DEFAULT 0,
                            build_id                  TEXT    NOT NULL DEFAULT '',
                            enabled_surface           TEXT    NOT NULL DEFAULT '',
                            effective_daily_cap_cents INTEGER NOT NULL DEFAULT 0,
                            tenant                    TEXT    NOT NULL DEFAULT '',
                            challenge_format_version  INTEGER NOT NULL DEFAULT 2
                                CHECK (challenge_format_version >= 2)
                        )",
                        [],
                    )?;
                    tx.commit()?;
                }

                // Signed-material invariant: ensure active_arm carries the
                // signed-material columns. active_arm is NEW in attested-arm change (never
                // shipped), so the only "upgrade" is an intra-dev DB created from
                // an earlier attested-arm change build without these columns. SQLite STRICT can't
                // ALTER ADD a NOT NULL BLOB without a default, and a live
                // active_arm row is ephemeral spend-authorization — dropping it
                // is the fail-closed action (forces a fresh hardware confirm). So
                // rebuild the table when the signature columns are absent.
                let aa_cols: Vec<String> = conn
                    .prepare("PRAGMA table_info(active_arm)")?
                    .query_map([], |r| r.get(1))?
                    .filter_map(Result::ok)
                    .collect();
                let aa_exists = conn
                    .query_row(
                        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='active_arm'",
                        [],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);
                if aa_exists && !aa_cols.iter().any(|c| c == "signature_der") {
                    let tx = conn
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    tx.execute("DROP TABLE IF EXISTS active_arm", [])?;
                    tx.execute(
                        "CREATE TABLE active_arm (
                            id                        INTEGER PRIMARY KEY CHECK(id = 0),
                            build_id                  TEXT    NOT NULL,
                            enabled_surface           TEXT    NOT NULL,
                            effective_daily_cap_cents INTEGER NOT NULL CHECK(effective_daily_cap_cents >= 1),
                            tenant                    TEXT    NOT NULL,
                            armed_epoch               INTEGER NOT NULL,
                            exp_at_ms                 INTEGER NOT NULL,
                            challenge_sha256          TEXT    NOT NULL,
                            audit_id                  INTEGER NOT NULL,
                            challenge_bytes           BLOB    NOT NULL,
                            signature_der             BLOB    NOT NULL,
                            credential_id             TEXT    NOT NULL,
                            credential_type           TEXT    NOT NULL,
                            authenticator_data        BLOB,
                            client_data_json          BLOB
                        ) STRICT",
                        [],
                    )?;
                    tx.commit()?;
                }

                // arm_audit CHECK rebuild (p0a P1 'boot_env_arm' + dual-custody-local-
                // attest B5 'stage_rehearsal'/'confirm_rehearsal'). SQLite cannot ALTER
                // a CHECK, so upgraded DBs rebuild the table with rows copied VERBATIM
                // (original ids, hashes, chain links — preimages are copied, never
                // recomputed, so the chain hashes are unchanged by construction).
                //
                // fail-closed migration invariant (fail-closed migration): inside the
                // SAME transaction, the copy must (a) row-count-match the original and
                // (b) pass a FULL hash-chain verification against the real history
                // BEFORE the migration commits. Any failure rolls the whole rebuild
                // back to the intact original table and boot aborts (run_migrations is
                // `?`-propagated at both call sites) — the gate stays closed rather
                // than running on a suspect audit chain.
                //
                // Crash-safety : one explicit transaction —
                // a crash mid-rebuild rolls back to the original table and the rebuild
                // retries on the next boot.
                //
                // Archive-never-delete: the original survives as arm_audit_pre_attest
                // (spec §13 rollback: kept until canary stability, then archived),
                // frozen by its own append-only triggers. A leftover backup from an
                // earlier rebuild is an operator artifact — refuse to clobber it.
                let arm_sql: Option<String> = conn
                    .query_row(
                        "SELECT sql FROM sqlite_master WHERE type='table' AND name='arm_audit'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                let needs_rebuild = arm_sql
                    .as_ref()
                    .is_some_and(|sql| !sql.contains("boot_env_arm") || !sql.contains("stage_rehearsal"));
                if needs_rebuild {
                    let constraint_err = |msg: String| {
                        rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                            Some(msg),
                        )
                    };
                    let tx = conn
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    // Hardening B4–B6 #4 (hardening): RE-CHECK both
                    // preconditions INSIDE the write lock. The pre-tx reads
                    // above could be stale if another process migrated this
                    // DB between our read and our BEGIN IMMEDIATE (excluded
                    // by deployment — single sidecar — but cheap to close).
                    let arm_sql_locked: Option<String> = tx
                        .query_row(
                            "SELECT sql FROM sqlite_master WHERE type='table' AND name='arm_audit'",
                            [],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let still_needs_rebuild = arm_sql_locked
                        .as_ref()
                        .is_some_and(|sql| !sql.contains("boot_env_arm") || !sql.contains("stage_rehearsal"));
                    // `still_needs_rebuild == false` means someone else
                    // completed the rebuild while we waited for the lock —
                    // drop the tx (rollback of nothing) and fall through, so
                    // any migrations appended after this block still run.
                    if still_needs_rebuild {
                        let backup_exists: bool = tx
                            .query_row(
                                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='arm_audit_pre_attest'",
                                [],
                                |_| Ok(true),
                            )
                            .optional()?
                            .unwrap_or(false);
                        if backup_exists {
                            return Err(constraint_err(
                                "arm_audit rebuild: arm_audit_pre_attest already exists — refusing to clobber the archived backup (fail-closed; operator must archive it out of watch.db first)".to_string(),
                            ));
                        }
                        let n_old: i64 =
                            tx.query_row("SELECT COUNT(*) FROM arm_audit", [], |r| r.get(0))?;
                        tx.execute_batch(
                            "DROP TRIGGER IF EXISTS trg_arm_audit_no_update;
                             DROP TRIGGER IF EXISTS trg_arm_audit_no_delete;
                             ALTER TABLE arm_audit RENAME TO arm_audit_pre_attest;
                             CREATE TABLE arm_audit (
                                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                                 at_ms     INTEGER NOT NULL,
                                 action    TEXT NOT NULL CHECK(action IN ('stage','confirm','disarm','stage_rejected','confirm_rejected','boot_env_arm','stage_rehearsal','confirm_rehearsal')),
                                 principal TEXT NOT NULL,
                                 detail    TEXT,
                                 prev_hash TEXT NOT NULL,
                                 hash      TEXT NOT NULL UNIQUE
                             );
                             INSERT INTO arm_audit (id, at_ms, action, principal, detail, prev_hash, hash)
                                 SELECT id, at_ms, action, principal, detail, prev_hash, hash
                                 FROM arm_audit_pre_attest ORDER BY id ASC;",
                        )?;

                        // Condition 5(a): the copy carries every row.
                        let n_new: i64 =
                            tx.query_row("SELECT COUNT(*) FROM arm_audit", [], |r| r.get(0))?;
                        if n_new != n_old {
                            return Err(constraint_err(format!(
                                "arm_audit rebuild: copied row count {n_new} != original {n_old} (condition 5 fail-closed; rolled back)"
                            )));
                        }

                        // Condition 5(b): full chain-verify against the REAL copied
                        // history — every prev_hash links, every hash recomputes.
                        {
                            let mut stmt = tx.prepare(
                                "SELECT id, at_ms, action, principal, detail, prev_hash, hash
                                 FROM arm_audit ORDER BY id ASC",
                            )?;
                            let mut rows = stmt.query([])?;
                            let mut expected_prev = arm_audit_distinct_genesis();
                            while let Some(row) = rows.next()? {
                                let id: i64 = row.get(0)?;
                                let at_ms: i64 = row.get(1)?;
                                let action: String = row.get(2)?;
                                let principal: String = row.get(3)?;
                                let detail: Option<String> = row.get(4)?;
                                let prev_hash: String = row.get(5)?;
                                let hash: String = row.get(6)?;
                                if prev_hash != expected_prev {
                                    return Err(constraint_err(format!(
                                        "arm_audit rebuild: chain break at id {id} — prev_hash does not link (condition 5 fail-closed; rolled back)"
                                    )));
                                }
                                let preimage = compute_arm_audit_preimage(
                                    at_ms,
                                    &action,
                                    &principal,
                                    detail.as_deref().unwrap_or(""),
                                    &prev_hash,
                                );
                                let recomputed = hex::encode(Sha256::digest(preimage.as_bytes()));
                                if recomputed != hash {
                                    return Err(constraint_err(format!(
                                        "arm_audit rebuild: hash mismatch at id {id} (condition 5 fail-closed; rolled back)"
                                    )));
                                }
                                expected_prev = hash;
                            }
                        }

                        tx.execute_batch(
                            "CREATE TRIGGER trg_arm_audit_no_update
                             BEFORE UPDATE ON arm_audit
                             FOR EACH ROW
                             BEGIN
                                 SELECT RAISE(ABORT, 'arm_audit_append_only');
                             END;
                             CREATE TRIGGER trg_arm_audit_no_delete
                             BEFORE DELETE ON arm_audit
                             FOR EACH ROW
                             BEGIN
                                 SELECT RAISE(ABORT, 'arm_audit_append_only');
                             END;
                             CREATE TRIGGER trg_arm_audit_pre_attest_no_update
                             BEFORE UPDATE ON arm_audit_pre_attest
                             FOR EACH ROW
                             BEGIN
                                 SELECT RAISE(ABORT, 'arm_audit_pre_attest_frozen');
                             END;
                             CREATE TRIGGER trg_arm_audit_pre_attest_no_delete
                             BEFORE DELETE ON arm_audit_pre_attest
                             FOR EACH ROW
                             BEGIN
                                 SELECT RAISE(ABORT, 'arm_audit_pre_attest_frozen');
                             END;",
                        )?;
                        tx.commit()?;
                    }
                }
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }
}
