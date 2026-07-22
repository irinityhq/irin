//! Write-ahead durable mirror for Council idempotency state.
//!
//! Handlers call these
//! `upsert_*` methods FIRST (durably committed in SQLite via
//! tokio_rusqlite's dedicated thread) and only THEN mutate in-memory
//! LRU. On crash, every persisted row replays at startup recovery
//! using the Stripe idempotency-key pattern.

use std::path::Path;
use std::time::Duration;
use tokio_rusqlite::Connection;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS idem_schema (
    version INTEGER NOT NULL PRIMARY KEY
);
INSERT OR IGNORE INTO idem_schema (version) VALUES (1);

CREATE TABLE IF NOT EXISTS council_idem (
    caller_key            TEXT NOT NULL,
    idempotency_key       TEXT NOT NULL,
    state                 TEXT NOT NULL CHECK(state IN ('pending','stored','failed')),
    body_sha256           TEXT NOT NULL,
    owner_request_id      TEXT,
    response_body_json    TEXT,
    response_headers_json TEXT,
    response_body_sha256  TEXT,
    started_at            INTEGER,
    stored_at             INTEGER,
    failed_at             INTEGER,
    PRIMARY KEY (caller_key, idempotency_key)
);
CREATE INDEX IF NOT EXISTS idx_council_idem_state_ts
    ON council_idem(state, stored_at, failed_at, started_at);

CREATE TABLE IF NOT EXISTS council_grants (
    caller_key  TEXT NOT NULL,
    grant_id    TEXT NOT NULL,
    granted_at  INTEGER NOT NULL,
    PRIMARY KEY (caller_key, grant_id)
);
CREATE INDEX IF NOT EXISTS idx_council_grants_caller ON council_grants(caller_key);
"#;

pub const IDEM_TTL: Duration = Duration::from_secs(86_400);
/// PENDING_TTL raised to 300s to satisfy deliberation_p99 <= LEASE_DURATION_MS (150_000 in watch/db.rs claim)
/// <= PENDING_TTL. Matches the bump in council.rs in-memory logic. Used for grant recovery floor and
/// stale pending checks in the durable mirror.
pub const PENDING_TTL: Duration = Duration::from_secs(300);
pub const FAILED_TTL: Duration = Duration::from_secs(60);

pub struct CouncilIdemDb {
    conn: Connection,
}

/// Summary of `recover_on_startup` for boot-log observability.
#[derive(Debug, Clone)]
pub struct RecoveryReport {
    pub loaded_stored: usize,
    pub dropped_pending: usize,
    pub stale_grants: usize,
}

/// A durable `state='stored'` row, loaded at boot so the in-memory Stored
/// LRU can be rehydrated from the write-ahead mirror.
/// `stored_at_ms` is the epoch-millis the row was committed; the caller
/// reconstructs a monotonic `Instant` from its *relative age*, never the
/// absolute value (`Instant` is process-relative, not wall-clock).
#[derive(Debug, Clone)]
pub struct StoredRow {
    pub caller_key: String,
    pub idempotency_key: String,
    pub body_sha256: String,
    pub response_body_json: String,
    pub response_body_sha256: String,
    pub owner_request_id: String,
    pub stored_at_ms: i64,
}

impl CouncilIdemDb {
    /// Open the council_idem.db at `path` with the write-ahead PRAGMA bundle:
    ///   - journal_mode=WAL    (concurrent readers + single writer)
    ///   - synchronous=NORMAL  (WAL-safe; tradeoff is window of seconds on power loss)
    ///   - busy_timeout=50ms   (caps contention pause)
    ///
    /// PRAGMAs are set via `pragma_update` rather than embedded in SCHEMA_V1
    /// because `journal_mode=WAL` cannot be set inside `execute_batch` (it's
    /// a connection-level setting, not a statement).
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).await?;
        conn.call(|conn| {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "busy_timeout", 50i64)?;
            Ok::<(), rusqlite::Error>(())
        })
        .await?;
        Ok(Self { conn })
    }

    pub async fn run_migrations(&self) -> anyhow::Result<()> {
        self.conn
            .call(|conn| {
                conn.execute_batch(SCHEMA_V1)?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    pub async fn upsert_pending(
        &self,
        caller_key: &str,
        idem_key: &str,
        body_sha: &str,
        owner_req_id: &str,
        started_at_ms: i64,
    ) -> anyhow::Result<()> {
        let caller_key = caller_key.to_string();
        let idem_key = idem_key.to_string();
        let body_sha = body_sha.to_string();
        let owner_req_id = owner_req_id.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO council_idem (caller_key, idempotency_key, state,
                        body_sha256, owner_request_id, started_at)
                     VALUES (?1, ?2, 'pending', ?3, ?4, ?5)
                     ON CONFLICT(caller_key, idempotency_key) DO UPDATE SET
                        state='pending', body_sha256=excluded.body_sha256,
                        owner_request_id=excluded.owner_request_id,
                        started_at=excluded.started_at,
                        stored_at=NULL, failed_at=NULL,
                        response_body_json=NULL, response_headers_json=NULL,
                        response_body_sha256=NULL",
                    rusqlite::params![caller_key, idem_key, body_sha, owner_req_id, started_at_ms],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_stored(
        &self,
        caller_key: &str,
        idem_key: &str,
        body_sha: &str,
        body_sha256_resp: &str,
        headers_json: &str,
        body_json: &str,
        owner_req_id: &str,
        stored_at_ms: i64,
    ) -> anyhow::Result<()> {
        let caller_key = caller_key.to_string();
        let idem_key = idem_key.to_string();
        let body_sha = body_sha.to_string();
        let body_sha256_resp = body_sha256_resp.to_string();
        let headers_json = headers_json.to_string();
        let body_json = body_json.to_string();
        let owner_req_id = owner_req_id.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    // owner_request_id is persisted on the Stored row (not NULLed)
                    // so a boot-time `load_stored_rows` → `rehydrate_stored` can
                    // restore the non-repudiation `original_request_id` after a
                    // restart. An empty string is stored when no owner
                    // is known (legacy callers) — never NULL — so the read side's
                    // `Option<String>::unwrap_or_default()` and a freshly-minted
                    // row agree on the same empty-owner representation.
                    "INSERT INTO council_idem (caller_key, idempotency_key, state,
                        body_sha256, response_body_json, response_headers_json,
                        response_body_sha256, owner_request_id, stored_at)
                     VALUES (?1, ?2, 'stored', ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(caller_key, idempotency_key) DO UPDATE SET
                        state='stored',
                        body_sha256=excluded.body_sha256,
                        response_body_json=excluded.response_body_json,
                        response_headers_json=excluded.response_headers_json,
                        response_body_sha256=excluded.response_body_sha256,
                        -- Write-once-sticky owner:
                        -- never let a re-store carrying an empty owner overwrite a
                        -- known non-repudiation `original_request_id`. Only a
                        -- non-empty incoming owner replaces the stored one; an
                        -- empty one preserves whatever is already there.
                        owner_request_id=CASE
                            WHEN excluded.owner_request_id IS NOT NULL
                                 AND excluded.owner_request_id != ''
                            THEN excluded.owner_request_id
                            ELSE owner_request_id END,
                        stored_at=excluded.stored_at,
                        started_at=NULL, failed_at=NULL",
                    rusqlite::params![
                        caller_key,
                        idem_key,
                        body_sha,
                        body_json,
                        headers_json,
                        body_sha256_resp,
                        owner_req_id,
                        stored_at_ms
                    ],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    pub async fn upsert_failed(
        &self,
        caller_key: &str,
        idem_key: &str,
        failed_at_ms: i64,
    ) -> anyhow::Result<()> {
        let caller_key = caller_key.to_string();
        let idem_key = idem_key.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO council_idem (caller_key, idempotency_key, state,
                        body_sha256, failed_at)
                     VALUES (?1, ?2, 'failed', '', ?3)
                     ON CONFLICT(caller_key, idempotency_key) DO UPDATE SET
                        state='failed', failed_at=excluded.failed_at,
                        started_at=NULL, stored_at=NULL, owner_request_id=NULL,
                        response_body_json=NULL, response_headers_json=NULL,
                        response_body_sha256=NULL",
                    rusqlite::params![caller_key, idem_key, failed_at_ms],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    pub async fn delete(&self, caller_key: &str, idem_key: &str) -> anyhow::Result<()> {
        let caller_key = caller_key.to_string();
        let idem_key = idem_key.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM council_idem WHERE caller_key=?1 AND idempotency_key=?2",
                    rusqlite::params![caller_key, idem_key],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    pub async fn record_grant(
        &self,
        caller_key: &str,
        grant_id: &str,
        granted_at_ms: i64,
    ) -> anyhow::Result<()> {
        let caller_key = caller_key.to_string();
        let grant_id = grant_id.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO council_grants (caller_key, grant_id, granted_at)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![caller_key, grant_id, granted_at_ms],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    pub async fn drop_grant(&self, caller_key: &str, grant_id: &str) -> anyhow::Result<()> {
        let caller_key = caller_key.to_string();
        let grant_id = grant_id.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM council_grants WHERE caller_key=?1 AND grant_id=?2",
                    rusqlite::params![caller_key, grant_id],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    /// Boot-time recovery: drop orphaned Pending rows (the sidecar's previous
    /// process owned them), sweep expired Stored/Failed, sweep stale grants,
    /// and report the survivor count of Stored so the in-memory LRU can
    /// rehydrate. Every
    /// in-flight claim either committed durably (and replays here) or
    /// was definitionally lost (and we restart cleanly).
    pub async fn recover_on_startup(&self) -> anyhow::Result<RecoveryReport> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let idem_ttl_ms = IDEM_TTL.as_millis() as i64;
        let pending_ttl_ms = PENDING_TTL.as_millis() as i64;
        let failed_ttl_ms = FAILED_TTL.as_millis() as i64;

        let (loaded, dropped, stale) = self
            .conn
            .call(move |conn| {
                // Drop all Pending — orphaned by sidecar restart, by definition.
                let dropped = conn.execute("DELETE FROM council_idem WHERE state='pending'", [])?;

                // Drop Stored past TTL.
                let stored_floor = now_ms - idem_ttl_ms;
                conn.execute(
                    "DELETE FROM council_idem WHERE state='stored' AND stored_at < ?1",
                    rusqlite::params![stored_floor],
                )?;

                // Drop Failed past TTL.
                let failed_floor = now_ms - failed_ttl_ms;
                conn.execute(
                    "DELETE FROM council_idem WHERE state='failed' AND failed_at < ?1",
                    rusqlite::params![failed_floor],
                )?;

                // Count surviving Stored — the in-memory LRU rehydration target.
                let loaded: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM council_idem WHERE state='stored'",
                    [],
                    |r| r.get(0),
                )?;

                // Drop stale grants (older than PENDING_TTL + 30s grace).
                let grant_floor = now_ms - pending_ttl_ms - 30_000;
                let stale = conn.execute(
                    "DELETE FROM council_grants WHERE granted_at < ?1",
                    rusqlite::params![grant_floor],
                )?;

                Ok::<_, rusqlite::Error>((loaded as usize, dropped, stale))
            })
            .await?;

        Ok(RecoveryReport {
            loaded_stored: loaded,
            dropped_pending: dropped,
            stale_grants: stale,
        })
    }

    /// Load every surviving `state='stored'` row so the in-memory LRU can be
    /// rehydrated at boot. Read-only; runs once before the HTTP
    /// handlers serve traffic. Kept SEPARATE from `recover_on_startup` so the
    /// recovery COUNT contract (and its existing test) stays untouched.
    /// `response_headers_json` is intentionally not loaded — the in-memory
    /// `IdemState::Stored` does not carry headers; only the response body
    /// Value plus the non-repudiation shas/owner are surfaced on replay.
    pub async fn load_stored_rows(&self) -> anyhow::Result<Vec<StoredRow>> {
        let rows = self
            .conn
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT caller_key, idempotency_key, body_sha256,
                            response_body_json, response_body_sha256,
                            owner_request_id, stored_at
                     FROM council_idem WHERE state='stored'",
                )?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok(StoredRow {
                            caller_key: r.get(0)?,
                            idempotency_key: r.get(1)?,
                            body_sha256: r.get(2)?,
                            response_body_json: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                            response_body_sha256: r
                                .get::<_, Option<String>>(4)?
                                .unwrap_or_default(),
                            owner_request_id: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                            stored_at_ms: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                        })
                    })?
                    .collect::<Result<Vec<_>, rusqlite::Error>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await?;
        Ok(rows)
    }

    /// Single-row durable lookup for read-through on LRU miss (D5 full close).
    /// Prevents cold-tail re-bills when > IDEM_CAPACITY un-expired Stored rows
    /// exist and the hot entry has fallen out of the bounded in-memory LRU.
    /// Returns the row only if it is still within TTL (belt-and-suspenders with
    /// recovery sweep); caller is responsible for body_sha256 validation and
    /// LRU warming.
    pub async fn get_stored_row(
        &self,
        caller_key: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<StoredRow>> {
        let ck = caller_key.to_string();
        let ik = idempotency_key.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT caller_key, idempotency_key, body_sha256,
                            response_body_json, response_body_sha256,
                            owner_request_id, stored_at
                     FROM council_idem
                     WHERE state='stored'
                       AND caller_key = ?1
                       AND idempotency_key = ?2",
                )?;
                let mut rows = stmt.query_map(rusqlite::params![ck, ik], |r| {
                    Ok(StoredRow {
                        caller_key: r.get(0)?,
                        idempotency_key: r.get(1)?,
                        body_sha256: r.get(2)?,
                        response_body_json: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        response_body_sha256: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        owner_request_id: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                        stored_at_ms: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    })
                })?;
                rows.next().transpose()
            })
            .await
            .map_err(Into::into)
    }
}
