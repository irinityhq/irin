// ==========================================================================
// ledger.rs — Cryptographic Audit Ledger
//
// Implements a tamper-evident event log utilizing SQLite (WAL mode),
// SHA-256 hash-chaining, and Ed25519 signing. Every stage of the pipeline
// emits an event here, creating a mathematically provable trail.
// ==========================================================================

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_rusqlite::Connection;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
// v3: length-prefixed hash preimage. v2's pipe-delimited format was vulnerable
// to delimiter-collision: a field containing `|` could collide with a different
// field arrangement. v3 encodes each field as `{len}:{value}` joined by `|`,
// which is unambiguous regardless of field content. The gateway ledger volume
// was ephemeral when v3 shipped, so there are no v2 events in production —
// verify_chain still supports v1 (legacy) and v3 (current); v2 has no callers.
pub const LEDGER_SCHEMA_VERSION: u32 = 3;

pub const EVENT_KEY_INTRODUCE: &str = "key_introduce";
#[allow(dead_code)]
pub const EVENT_KEY_REVOKE: &str = "key_revoke";

/// Build the v3 hash preimage: each field encoded as `{length}:{value}` and
/// joined by `|`. Unambiguous regardless of field content (no delimiter
/// collision possible — the length prefix forces the parser to take exactly
/// `len` bytes from `:` onward before looking for the next field).
#[allow(clippy::too_many_arguments)] // stable hash-preimage builder; refactoring the signature is a wire-compat change.
pub fn build_hash_preimage_v3(
    timestamp: u64,
    source: &str,
    target: &str,
    payload: &str,
    metadata: &str,
    schema_version: u32,
    caller_key: &str,
    prev_hash: &str,
) -> String {
    let ts_str = timestamp.to_string();
    let sv_str = schema_version.to_string();
    format!(
        "{}:{}|{}:{}|{}:{}|{}:{}|{}:{}|{}:{}|{}:{}|{}:{}",
        ts_str.len(),
        ts_str,
        source.len(),
        source,
        target.len(),
        target,
        payload.len(),
        payload,
        metadata.len(),
        metadata,
        sv_str.len(),
        sv_str,
        caller_key.len(),
        caller_key,
        prev_hash.len(),
        prev_hash,
    )
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEvent {
    pub id: Option<i64>,
    pub timestamp: u64,
    pub source: String,
    pub target: String,
    pub payload: String,  // JSON payload stringified
    pub metadata: String, // JSON metadata stringified
    pub caller_key: Option<String>,
    pub signing_key_pubkey: Option<String>,
    pub schema_version: u32,
    pub prev_hash: String,
    pub hash: String,
    pub signature: String, // Hex string of Ed25519 signature
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventInput {
    pub source: String,
    pub target: String,
    pub payload: serde_json::Value,
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub caller_key: Option<String>,
}

// ---------------------------------------------------------------------------
// Audit Ledger Implementation
// ---------------------------------------------------------------------------

pub struct AuditLedger {
    conn: Arc<Mutex<Connection>>,
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
    old_verifying_key: Option<VerifyingKey>,
}

impl AuditLedger {
    /// Initializes the schema, configures WAL and auto_vacuum, and supports key rotation.
    pub async fn new(
        db_path: &str,
        signing_key_bytes: Option<&[u8]>,
        old_verifying_key_bytes: Option<&[u8]>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let conn = Connection::open(db_path).await?;

        // Configure connection for performance and concurrency
        conn.call(|conn| {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "busy_timeout", "5000")?;
            conn.pragma_update(None, "auto_vacuum", "FULL")?;

            // Initialize schema
            conn.execute(
                "CREATE TABLE IF NOT EXISTS audit_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp INTEGER NOT NULL,
                    source TEXT NOT NULL,
                    target TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    metadata TEXT NOT NULL,
                    schema_version INTEGER NOT NULL DEFAULT 1,
                    prev_hash TEXT NOT NULL,
                    hash TEXT NOT NULL UNIQUE,
                    signature TEXT NOT NULL
                )",
                [],
            )?;

            // Create index on hash for fast lookups
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_audit_events_hash ON audit_events (hash)",
                [],
            )?;

            // Schema version check — fail fast if old DB lacks the column
            let has_schema_version = {
                let mut pragma_stmt = conn.prepare("PRAGMA table_info(audit_events)")?;
                let columns: Vec<String> = pragma_stmt
                    .query_map([], |row| row.get::<_, String>(1))?
                    .filter_map(|r| r.ok())
                    .collect();
                columns.contains(&"schema_version".to_string())
            };
            if !has_schema_version {
                panic!(
                    "Ledger schema mismatch: audit_events missing schema_version column. \
                     Delete ledger.db and restart."
                );
            }

            // v2 migration: add caller_key column if missing.
            // Nullable: NULL => legacy/v1 event, "" => authenticated but unknown,
            // non-empty => the resolved key_id from /auth/check.
            let has_caller_key = {
                let mut pragma_stmt = conn.prepare("PRAGMA table_info(audit_events)")?;
                let columns: Vec<String> = pragma_stmt
                    .query_map([], |row| row.get::<_, String>(1))?
                    .filter_map(|r| r.ok())
                    .collect();
                columns.contains(&"caller_key".to_string())
            };
            if !has_caller_key {
                conn.execute("ALTER TABLE audit_events ADD COLUMN caller_key TEXT", [])?;
            }

            // Partial index on (caller_key, timestamp) for per-key audit lookups.
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_audit_caller_key_ts \
                 ON audit_events (caller_key, timestamp) \
                 WHERE caller_key IS NOT NULL",
                [],
            )?;

            // 4a migration: add signing_key_pubkey column if missing.
            // Stores the hex pubkey that signed each event — verifier index,
            // not part of the hash preimage (the signature itself binds the signer).
            let has_signing_key_pubkey = {
                let mut pragma_stmt = conn.prepare("PRAGMA table_info(audit_events)")?;
                let columns: Vec<String> = pragma_stmt
                    .query_map([], |row| row.get::<_, String>(1))?
                    .filter_map(|r| r.ok())
                    .collect();
                columns.contains(&"signing_key_pubkey".to_string())
            };
            if !has_signing_key_pubkey {
                conn.execute(
                    "ALTER TABLE audit_events ADD COLUMN signing_key_pubkey TEXT",
                    [],
                )?;
            }

            Ok::<(), rusqlite::Error>(())
        })
        .await?;

        // Initialize keys
        let signing_key = match signing_key_bytes {
            Some(bytes) => {
                let mut key_bytes = [0u8; 32];
                key_bytes.copy_from_slice(&bytes[..32]);
                SigningKey::from_bytes(&key_bytes)
            }
            None => {
                warn!("No Ed25519 key provided, generating an ephemeral key for the ledger");
                let mut csprng = OsRng;
                SigningKey::generate(&mut csprng)
            }
        };

        let verifying_key = signing_key.verifying_key();

        let old_verifying_key = match old_verifying_key_bytes {
            Some(bytes) if bytes.len() == 32 => {
                let mut b = [0u8; 32];
                b.copy_from_slice(&bytes[..32]);
                VerifyingKey::from_bytes(&b).ok()
            }
            _ => None,
        };

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            signing_key,
            verifying_key,
            old_verifying_key,
        })
    }

    /// Check the database size and run a manual VACUUM if it exceeds the threshold (e.g. 50MB)
    pub async fn run_vacuum_if_needed(&self, threshold_mb: f64) -> Result<bool, String> {
        let conn_lock = self.conn.lock().await;

        let (page_count, page_size) = conn_lock
            .call(|conn| {
                let count: i64 = conn.pragma_query_value(None, "page_count", |row| row.get(0))?;
                let size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
                Ok::<_, rusqlite::Error>((count, size))
            })
            .await
            .map_err(|e| format!("Database error checking size: {}", e))?;

        let size_mb = (page_count * page_size) as f64 / 1_048_576.0;

        if size_mb > threshold_mb {
            info!(
                "ledger database size is {:.2}MB (> {}MB threshold), running VACUUM",
                size_mb, threshold_mb
            );
            conn_lock
                .call(|conn| {
                    conn.execute("VACUUM", [])?;
                    Ok::<_, rusqlite::Error>(())
                })
                .await
                .map_err(|e| format!("Database error running VACUUM: {}", e))?;
            info!("ledger VACUUM complete");
            return Ok(true);
        }
        Ok(false)
    }

    /// Retrieve paginated events for export.
    pub async fn export_events(&self, limit: u32, offset: u32) -> Result<Vec<LedgerEvent>, String> {
        let conn_lock = self.conn.lock().await;
        conn_lock.call(move |conn| {
            let mut stmt = conn.prepare("SELECT id, timestamp, source, target, payload, metadata, caller_key, signing_key_pubkey, schema_version, prev_hash, hash, signature FROM audit_events ORDER BY id ASC LIMIT ?1 OFFSET ?2")?;
            let event_iter = stmt.query_map(rusqlite::params![limit, offset], |row| {
                Ok(LedgerEvent {
                    id: Some(row.get(0)?),
                    timestamp: row.get(1)?,
                    source: row.get(2)?,
                    target: row.get(3)?,
                    payload: row.get(4)?,
                    metadata: row.get(5)?,
                    caller_key: row.get::<_, Option<String>>(6)?,
                    signing_key_pubkey: row.get::<_, Option<String>>(7)?,
                    schema_version: row.get(8)?,
                    prev_hash: row.get(9)?,
                    hash: row.get(10)?,
                    signature: row.get(11)?,
                })
            })?;
            let mut events = Vec::new();
            for e in event_iter {
                events.push(e?);
            }
            Ok::<Vec<LedgerEvent>, rusqlite::Error>(events)
        }).await.map_err(|e| format!("Database error: {}", e))
    }

    /// Record a new event into the ledger.
    /// Computes the hash chain and signs the entry.
    pub async fn record_event(&self, input: EventInput) -> Result<LedgerEvent, String> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let payload_str = serde_json::to_string(&input.payload).unwrap_or_default();
        let metadata_str = serde_json::to_string(&input.metadata).unwrap_or_default();
        // For the hash preimage, use the empty string when caller_key is absent.
        // For the DB column, store NULL when absent (so legacy queries / partial
        // index continue to work).
        let caller_key_opt: Option<String> = input.caller_key.clone().filter(|s| !s.is_empty());
        let caller_key_str = caller_key_opt.clone().unwrap_or_default();

        let conn_lock = self.conn.lock().await;
        let signer_pubkey_hex = hex::encode(self.verifying_key.as_bytes());

        // Perform the DB read and write in a single call to ensure atomicity
        let (
            source_clone,
            target_clone,
            payload_clone,
            metadata_clone,
            caller_key_db,
            caller_key_hash,
            key_clone,
            pubkey_clone,
        ) = (
            input.source.clone(),
            input.target.clone(),
            payload_str.clone(),
            metadata_str.clone(),
            caller_key_opt.clone(),
            caller_key_str.clone(),
            self.signing_key.clone(),
            signer_pubkey_hex.clone(),
        );

        let event_result = conn_lock.call(move |conn| {
            // Get previous hash
            let mut stmt = conn.prepare("SELECT hash FROM audit_events ORDER BY id DESC LIMIT 1")?;
            let prev_hash = match stmt.query_row([], |row| row.get::<_, String>(0)) {
                Ok(h) => h,
                Err(_) => GENESIS_HASH.to_string(), // No events yet
            };

            // Compute new hash (schema v3 preimage — length-prefixed encoding).
            // See build_hash_preimage_v3 for rationale. v1/v2 verify paths in
            // verify_chain() preserve historical compatibility.
            let data_to_hash = build_hash_preimage_v3(
                timestamp, &source_clone, &target_clone, &payload_clone, &metadata_clone,
                LEDGER_SCHEMA_VERSION, &caller_key_hash, &prev_hash,
            );

            let mut hasher = Sha256::new();
            hasher.update(data_to_hash.as_bytes());
            let hash_bytes = hasher.finalize();
            let hash_hex = hex::encode(hash_bytes);

            // Sign the hash
            let signature: Signature = key_clone.sign(&hash_bytes);
            let sig_hex = hex::encode(signature.to_bytes());

            // Insert into DB. caller_key is NULL when absent, the string otherwise.
            conn.execute(
                "INSERT INTO audit_events (timestamp, source, target, payload, metadata, caller_key, signing_key_pubkey, schema_version, prev_hash, hash, signature)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    timestamp,
                    source_clone,
                    target_clone,
                    payload_clone,
                    metadata_clone,
                    caller_key_db,
                    pubkey_clone,
                    LEDGER_SCHEMA_VERSION,
                    prev_hash,
                    hash_hex,
                    sig_hex
                ],
            )?;

            let id = conn.last_insert_rowid();

            Ok::<LedgerEvent, rusqlite::Error>(LedgerEvent {
                id: Some(id),
                timestamp,
                source: source_clone,
                target: target_clone,
                payload: payload_clone,
                metadata: metadata_clone,
                caller_key: caller_key_db,
                signing_key_pubkey: Some(pubkey_clone),
                schema_version: LEDGER_SCHEMA_VERSION,
                prev_hash,
                hash: hash_hex,
                signature: sig_hex,
            })
        }).await.map_err(|e| format!("Database error: {}", e))?;

        debug!(
            event_id = event_result.id,
            hash = %event_result.hash,
            "ledger: event recorded"
        );

        Ok(event_result)
    }

    /// Retrieve the latest event.
    #[allow(dead_code)]
    pub async fn get_latest_event(&self) -> Result<Option<LedgerEvent>, String> {
        let conn_lock = self.conn.lock().await;

        let event = conn_lock.call(|conn| {
            let mut stmt = conn.prepare("SELECT id, timestamp, source, target, payload, metadata, caller_key, signing_key_pubkey, schema_version, prev_hash, hash, signature FROM audit_events ORDER BY id DESC LIMIT 1")?;

            let result = stmt.query_row([], |row| {
                Ok(LedgerEvent {
                    id: Some(row.get(0)?),
                    timestamp: row.get(1)?,
                    source: row.get(2)?,
                    target: row.get(3)?,
                    payload: row.get(4)?,
                    metadata: row.get(5)?,
                    caller_key: row.get::<_, Option<String>>(6)?,
                    signing_key_pubkey: row.get::<_, Option<String>>(7)?,
                    schema_version: row.get(8)?,
                    prev_hash: row.get(9)?,
                    hash: row.get(10)?,
                    signature: row.get(11)?,
                })
            });

            match result {
                Ok(e) => Ok::<Option<LedgerEvent>, rusqlite::Error>(Some(e)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e),
            }
        }).await.map_err(|e| format!("Database error: {}", e))?;

        Ok(event)
    }

    /// Full cryptographic verification of the entire chain.
    /// Recomputes all hashes and verifies all signatures.
    pub async fn verify_chain(&self) -> Result<bool, String> {
        let conn_lock = self.conn.lock().await;
        let verifying_key = self.verifying_key;
        let old_verifying_key = self.old_verifying_key;

        let is_valid = conn_lock.call(move |conn| {
            let mut stmt = conn.prepare("SELECT id, timestamp, source, target, payload, metadata, caller_key, signing_key_pubkey, schema_version, prev_hash, hash, signature FROM audit_events ORDER BY id ASC")?;

            let event_iter = stmt.query_map([], |row| {
                Ok(LedgerEvent {
                    id: Some(row.get(0)?),
                    timestamp: row.get(1)?,
                    source: row.get(2)?,
                    target: row.get(3)?,
                    payload: row.get(4)?,
                    metadata: row.get(5)?,
                    caller_key: row.get::<_, Option<String>>(6)?,
                    signing_key_pubkey: row.get::<_, Option<String>>(7)?,
                    schema_version: row.get(8)?,
                    prev_hash: row.get(9)?,
                    hash: row.get(10)?,
                    signature: row.get(11)?,
                })
            })?;

            let mut expected_prev_hash = GENESIS_HASH.to_string();

            for event_result in event_iter {
                let event = event_result?;

                // 1. Verify chain link
                if event.prev_hash != expected_prev_hash {
                    error!(
                        id = event.id,
                        expected = %expected_prev_hash,
                        actual = %event.prev_hash,
                        "ledger verification failed: chain broken"
                    );
                    return Ok::<bool, rusqlite::Error>(false);
                }

                // 2. Recompute hash — preimage is schema-version-aware:
                //    v1: 7-field pipe-delimited, no caller_key
                //    v2: 8-field pipe-delimited, with caller_key (legacy — no
                //        v2 events ever shipped to production, but the path is
                //        retained so any leftover dev DBs still verify)
                //    v3+: length-prefixed (build_hash_preimage_v3)
                let data_to_hash = match event.schema_version {
                    1 => format!(
                        "{}|{}|{}|{}|{}|{}|{}",
                        event.timestamp, event.source, event.target, event.payload, event.metadata,
                        event.schema_version, event.prev_hash
                    ),
                    2 => {
                        let caller_key_for_hash = event.caller_key.clone().unwrap_or_default();
                        format!(
                            "{}|{}|{}|{}|{}|{}|{}|{}",
                            event.timestamp, event.source, event.target, event.payload, event.metadata,
                            event.schema_version, caller_key_for_hash, event.prev_hash
                        )
                    }
                    _ => build_hash_preimage_v3(
                        event.timestamp, &event.source, &event.target, &event.payload, &event.metadata,
                        event.schema_version, event.caller_key.as_deref().unwrap_or(""), &event.prev_hash,
                    ),
                };

                let mut hasher = Sha256::new();
                hasher.update(data_to_hash.as_bytes());
                let hash_bytes = hasher.finalize();
                let hash_hex = hex::encode(hash_bytes);

                if event.hash != hash_hex {
                    error!(
                        id = event.id,
                        expected_hash = %hash_hex,
                        actual_hash = %event.hash,
                        "ledger verification failed: hash mismatch"
                    );
                    return Ok(false);
                }

                // 3. Verify signature
                let sig_bytes = match hex::decode(&event.signature) {
                    Ok(b) => b,
                    Err(_) => {
                        error!(id = event.id, "ledger verification failed: invalid signature hex");
                        return Ok(false);
                    }
                };

                let sig_array: [u8; 64] = match sig_bytes.try_into() {
                    Ok(a) => a,
                    Err(_) => {
                        error!(id = event.id, "ledger verification failed: signature wrong length");
                        return Ok(false);
                    }
                };

                let signature = Signature::from_bytes(&sig_array);

                // If the event carries signing_key_pubkey, use it to select the
                // verifying key directly. Otherwise fall back to trial against
                // the active and old keys (legacy rows without the field).
                let sig_valid = if let Some(ref pubkey_hex) = event.signing_key_pubkey {
                    if let Ok(pk_bytes) = hex::decode(pubkey_hex) {
                        if let Ok(pk_arr) = <[u8; 32]>::try_from(pk_bytes.as_slice()) {
                            if let Ok(event_vk) = VerifyingKey::from_bytes(&pk_arr) {
                                event_vk.verify_strict(&hash_bytes, &signature).is_ok()
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    // Legacy event without signing_key_pubkey — trial verify
                    if verifying_key.verify_strict(&hash_bytes, &signature).is_ok() {
                        true
                    } else if let Some(old_key) = &old_verifying_key {
                        old_key.verify_strict(&hash_bytes, &signature).is_ok()
                    } else {
                        false
                    }
                };

                if !sig_valid {
                    error!(id = event.id, "ledger verification failed: invalid cryptographic signature");
                    return Ok(false);
                }

                expected_prev_hash = event.hash;
            }

            Ok(true)
        }).await.map_err(|e| format!("Database error: {}", e))?;

        if is_valid {
            info!("ledger: full cryptographic chain verified successfully");
        }

        Ok(is_valid)
    }

    /// Semantic fsck — runs verify_chain, then walks events a second time for
    /// policy checks: signing_key_pubkey presence, schema monotonicity, key
    /// lifecycle event scanning, introduce-before-use, no-use-after-revoke.
    #[allow(dead_code)]
    pub async fn fsck(&self) -> Result<FsckReport, String> {
        let chain_valid = self.verify_chain().await?;

        let conn_lock = self.conn.lock().await;
        let report = conn_lock
            .call(move |conn| {
                let total: usize = conn
                    .query_row("SELECT COUNT(*) FROM audit_events", [], |row| row.get(0))
                    .unwrap_or(0);

                let mut stmt = conn.prepare(
                    "SELECT id, timestamp, source, target, payload, metadata, caller_key, \
                 signing_key_pubkey, schema_version, prev_hash, hash, signature \
                 FROM audit_events ORDER BY id ASC",
                )?;

                let event_iter = stmt.query_map([], |row| {
                    Ok(LedgerEvent {
                        id: Some(row.get(0)?),
                        timestamp: row.get(1)?,
                        source: row.get(2)?,
                        target: row.get(3)?,
                        payload: row.get(4)?,
                        metadata: row.get(5)?,
                        caller_key: row.get::<_, Option<String>>(6)?,
                        signing_key_pubkey: row.get::<_, Option<String>>(7)?,
                        schema_version: row.get(8)?,
                        prev_hash: row.get(9)?,
                        hash: row.get(10)?,
                        signature: row.get(11)?,
                    })
                })?;

                let mut pubkey_missing_on_v3plus: Vec<i64> = Vec::new();
                let mut schema_monotonic = true;
                let mut max_schema_version: u32 = 0;
                let mut signers_seen: HashSet<String> = HashSet::new();
                let mut introduces: Vec<(i64, String)> = Vec::new(); // (event_id, new_pubkey_hex)
                let mut revokes: Vec<(i64, String)> = Vec::new(); // (event_id, revoked_pubkey_hex)
                let mut introduced_keys: HashSet<String> = HashSet::new();
                let mut revoked_keys: HashSet<String> = HashSet::new();
                let mut duplicate_introduces: Vec<String> = Vec::new();
                let mut revoked_key_uses: Vec<i64> = Vec::new();
                let mut warnings: Vec<String> = Vec::new();

                for event_result in event_iter {
                    let event = event_result?;
                    let eid = event.id.unwrap_or(0);

                    // Schema monotonicity
                    if event.schema_version < max_schema_version {
                        schema_monotonic = false;
                    }
                    max_schema_version = max_schema_version.max(event.schema_version);

                    // signing_key_pubkey presence on v3+
                    if event.schema_version >= 3 && event.signing_key_pubkey.is_none() {
                        pubkey_missing_on_v3plus.push(eid);
                    }

                    // Track signers
                    if let Some(ref pk) = event.signing_key_pubkey {
                        signers_seen.insert(pk.clone());

                        // No-use-after-revoke: if this key has been revoked, flag it
                        if revoked_keys.contains(pk) {
                            revoked_key_uses.push(eid);
                        }
                    }

                    // Key lifecycle event detection
                    if event.target == EVENT_KEY_INTRODUCE {
                        if let Ok(payload) =
                            serde_json::from_str::<serde_json::Value>(&event.payload)
                        {
                            if let Some(new_pk) =
                                payload.get("new_pubkey_hex").and_then(|v| v.as_str())
                            {
                                if introduced_keys.contains(new_pk) {
                                    duplicate_introduces.push(new_pk.to_string());
                                }
                                introduced_keys.insert(new_pk.to_string());
                                introduces.push((eid, new_pk.to_string()));
                            }
                        }
                    } else if event.target == EVENT_KEY_REVOKE {
                        if let Ok(payload) =
                            serde_json::from_str::<serde_json::Value>(&event.payload)
                        {
                            if let Some(revoked_pk) =
                                payload.get("revoked_pubkey_hex").and_then(|v| v.as_str())
                            {
                                revoked_keys.insert(revoked_pk.to_string());
                                revokes.push((eid, revoked_pk.to_string()));
                            }
                        }
                    }
                }

                // Warnings for missing pubkeys (not failures — legacy dev DBs)
                if !pubkey_missing_on_v3plus.is_empty() {
                    warnings.push(format!(
                        "{} v3+ event(s) missing signing_key_pubkey: {:?}",
                        pubkey_missing_on_v3plus.len(),
                        &pubkey_missing_on_v3plus[..pubkey_missing_on_v3plus.len().min(5)]
                    ));
                }

                Ok::<FsckReport, rusqlite::Error>(FsckReport {
                    chain_valid,
                    schema_monotonic,
                    total_events: total,
                    pubkey_missing_on_v3plus,
                    introduces,
                    revokes,
                    signers_seen,
                    introduced_keys,
                    revoked_keys,
                    revoked_key_uses,
                    duplicate_introduces,
                    warnings,
                })
            })
            .await
            .map_err(|e| format!("Database error: {}", e))?;

        Ok(report)
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
pub struct FsckReport {
    pub chain_valid: bool,
    pub schema_monotonic: bool,
    pub total_events: usize,
    pub pubkey_missing_on_v3plus: Vec<i64>,
    pub introduces: Vec<(i64, String)>,
    pub revokes: Vec<(i64, String)>,
    pub signers_seen: HashSet<String>,
    pub introduced_keys: HashSet<String>,
    pub revoked_keys: HashSet<String>,
    pub revoked_key_uses: Vec<i64>,
    pub duplicate_introduces: Vec<String>,
    pub warnings: Vec<String>,
}

impl FsckReport {
    #[allow(dead_code)]
    pub fn is_healthy(&self) -> bool {
        self.chain_valid
            && self.schema_monotonic
            && self.revoked_key_uses.is_empty()
            && self.duplicate_introduces.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    fn temp_db_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "gateway_ledger_test_{}_{}.db",
            name,
            std::process::id()
        ));
        path
    }

    async fn test_ledger(name: &str) -> (AuditLedger, PathBuf) {
        let path = temp_db_path(name);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
        let ledger = AuditLedger::new(path.to_str().unwrap(), None, None)
            .await
            .unwrap();
        (ledger, path)
    }

    fn cleanup(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    fn test_input(action: &str) -> EventInput {
        EventInput {
            source: "test".into(),
            target: "test".into(),
            payload: serde_json::json!({"action": action}),
            metadata: serde_json::json!({"test": true}),
            caller_key: None,
        }
    }

    #[tokio::test]
    async fn test_schema_version_in_hash() {
        let (ledger, path) = test_ledger("schema_in_hash").await;
        let event = ledger.record_event(test_input("check_hash")).await.unwrap();

        let payload_str =
            serde_json::to_string(&serde_json::json!({"action":"check_hash"})).unwrap();
        let metadata_str = serde_json::to_string(&serde_json::json!({"test":true})).unwrap();
        // v3 preimage format: length-prefixed encoding via build_hash_preimage_v3.
        // (caller_key is the empty string when None.)
        let with_version = build_hash_preimage_v3(
            event.timestamp,
            &event.source,
            &event.target,
            &payload_str,
            &metadata_str,
            LEDGER_SCHEMA_VERSION,
            "",
            &event.prev_hash,
        );
        let mut hasher = Sha256::new();
        hasher.update(with_version.as_bytes());
        let hash_with = hex::encode(hasher.finalize());
        assert_eq!(
            event.hash, hash_with,
            "hash must use v3 length-prefixed preimage"
        );

        // Sanity: a preimage without schema_version must produce a different hash.
        let without_version = format!(
            "{}|{}|{}|{}|{}|{}",
            event.timestamp, event.source, event.target, payload_str, metadata_str, event.prev_hash
        );
        let mut hasher2 = Sha256::new();
        hasher2.update(without_version.as_bytes());
        let hash_without = hex::encode(hasher2.finalize());
        assert_ne!(event.hash, hash_without, "hash without version must differ");

        assert_eq!(event.schema_version, LEDGER_SCHEMA_VERSION);
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_corrupted_schema_version() {
        let (ledger, path) = test_ledger("corrupted_version").await;
        ledger.record_event(test_input("event1")).await.unwrap();
        ledger.record_event(test_input("event2")).await.unwrap();

        assert!(ledger.verify_chain().await.unwrap());

        let conn = ledger.conn.lock().await;
        conn.call(|conn| {
            conn.execute(
                "UPDATE audit_events SET schema_version = 99 WHERE id = 1",
                [],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .unwrap();
        drop(conn);

        assert!(!ledger.verify_chain().await.unwrap());
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_verify_chain_with_versioned_events() {
        let (ledger, path) = test_ledger("verify_chain").await;

        for i in 0..5 {
            ledger
                .record_event(test_input(&format!("event_{}", i)))
                .await
                .unwrap();
        }

        assert!(ledger.verify_chain().await.unwrap());

        let latest = ledger.get_latest_event().await.unwrap().unwrap();
        assert_eq!(latest.id, Some(5));
        assert_eq!(latest.schema_version, LEDGER_SCHEMA_VERSION);
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_schema_v2_caller_key_in_hash() {
        // Despite the historical name, this test now exercises the v3 preimage —
        // record_event always emits LEDGER_SCHEMA_VERSION (currently 3). The
        // intent the test guards is unchanged: caller_key must contribute to
        // the hash. v2 remains a verify-only legacy code path.
        let (ledger, path) = test_ledger("v3_caller_key").await;

        let mut input = test_input("with_caller");
        input.caller_key = Some("k_abc123".to_string());
        let event = ledger.record_event(input).await.unwrap();

        // Stored caller_key roundtrips
        assert_eq!(event.caller_key.as_deref(), Some("k_abc123"));
        assert_eq!(event.schema_version, LEDGER_SCHEMA_VERSION);

        // Hash preimage uses v3 length-prefixed encoding with caller_key slot
        let payload_str =
            serde_json::to_string(&serde_json::json!({"action":"with_caller"})).unwrap();
        let metadata_str = serde_json::to_string(&serde_json::json!({"test":true})).unwrap();
        let preimage_with_key = build_hash_preimage_v3(
            event.timestamp,
            &event.source,
            &event.target,
            &payload_str,
            &metadata_str,
            event.schema_version,
            "k_abc123",
            &event.prev_hash,
        );
        let mut h1 = Sha256::new();
        h1.update(preimage_with_key.as_bytes());
        assert_eq!(event.hash, hex::encode(h1.finalize()));

        // Same preimage with empty caller_key slot must NOT match
        let preimage_empty = build_hash_preimage_v3(
            event.timestamp,
            &event.source,
            &event.target,
            &payload_str,
            &metadata_str,
            event.schema_version,
            "",
            &event.prev_hash,
        );
        let mut h2 = Sha256::new();
        h2.update(preimage_empty.as_bytes());
        assert_ne!(
            event.hash,
            hex::encode(h2.finalize()),
            "caller_key must contribute to hash"
        );

        // Chain still verifies
        assert!(ledger.verify_chain().await.unwrap());
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_verify_chain_mixed_schema_versions() {
        // Simulate a mixed chain: a v1-era event (no caller_key, v1 preimage)
        // followed by a current-schema event (with caller_key, length-prefixed
        // preimage). verify_chain must use the per-event schema_version to pick
        // the right preimage.
        let (ledger, path) = test_ledger("mixed_schema").await;

        // Hand-craft a schema_version=1 event using the v1 preimage format.
        let timestamp_v1: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let payload_v1 = serde_json::to_string(&serde_json::json!({"action":"v1_event"})).unwrap();
        let metadata_v1 = serde_json::to_string(&serde_json::json!({"test":true})).unwrap();
        let prev_hash_v1 = GENESIS_HASH.to_string();

        let preimage_v1 = format!(
            "{}|{}|{}|{}|{}|{}|{}",
            timestamp_v1, "test", "test", payload_v1, metadata_v1, 1u32, prev_hash_v1
        );
        let mut hv1 = Sha256::new();
        hv1.update(preimage_v1.as_bytes());
        let hash_v1_bytes = hv1.finalize();
        let hash_v1_hex = hex::encode(hash_v1_bytes);
        let sig_v1 = ledger.signing_key.sign(&hash_v1_bytes);
        let sig_v1_hex = hex::encode(sig_v1.to_bytes());

        // Insert the v1 event directly (caller_key is NULL, schema_version = 1)
        let conn = ledger.conn.lock().await;
        let prev_v1 = prev_hash_v1.clone();
        let payload_clone = payload_v1.clone();
        let metadata_clone = metadata_v1.clone();
        let hash_clone = hash_v1_hex.clone();
        let sig_clone = sig_v1_hex.clone();
        conn.call(move |c| {
            c.execute(
                "INSERT INTO audit_events (timestamp, source, target, payload, metadata, caller_key, signing_key_pubkey, schema_version, prev_hash, hash, signature)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    timestamp_v1, "test", "test", payload_clone, metadata_clone,
                    1u32, prev_v1, hash_clone, sig_clone
                ],
            )?;
            Ok::<(), rusqlite::Error>(())
        }).await.unwrap();
        drop(conn);

        // Now record a current-schema event on top, with a caller_key
        let mut v_input = test_input("current_event");
        v_input.caller_key = Some("k_v2".to_string());
        let v_event = ledger.record_event(v_input).await.unwrap();
        assert_eq!(v_event.schema_version, LEDGER_SCHEMA_VERSION);
        assert_eq!(
            v_event.prev_hash, hash_v1_hex,
            "current event must chain off the v1 event's hash"
        );

        // Mixed-version chain must verify
        assert!(
            ledger.verify_chain().await.unwrap(),
            "mixed v1+current chain must validate"
        );
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_signing_key_pubkey_populated() {
        let (ledger, path) = test_ledger("pubkey_populated").await;
        let event = ledger
            .record_event(test_input("pubkey_check"))
            .await
            .unwrap();

        let expected_pubkey = hex::encode(ledger.verifying_key.as_bytes());
        assert_eq!(event.signing_key_pubkey, Some(expected_pubkey.clone()));

        let latest = ledger.get_latest_event().await.unwrap().unwrap();
        assert_eq!(latest.signing_key_pubkey, Some(expected_pubkey));

        assert!(ledger.verify_chain().await.unwrap());
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_signing_key_pubkey_mismatch_fails_verify() {
        let (ledger, path) = test_ledger("pubkey_mismatch").await;
        ledger.record_event(test_input("good_event")).await.unwrap();

        // Corrupt signing_key_pubkey to a different valid pubkey
        let wrong_key = SigningKey::generate(&mut OsRng);
        let wrong_pubkey = hex::encode(wrong_key.verifying_key().as_bytes());

        let conn = ledger.conn.lock().await;
        let wp = wrong_pubkey.clone();
        conn.call(move |c| {
            c.execute(
                "UPDATE audit_events SET signing_key_pubkey = ?1 WHERE id = 1",
                rusqlite::params![wp],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .unwrap();
        drop(conn);

        assert!(
            !ledger.verify_chain().await.unwrap(),
            "mismatched signing_key_pubkey must fail verification"
        );
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_startup_schema_check() {
        let path = temp_db_path("startup_check");
        let _ = std::fs::remove_file(&path);

        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE audit_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                source TEXT NOT NULL,
                target TEXT NOT NULL,
                payload TEXT NOT NULL,
                metadata TEXT NOT NULL,
                prev_hash TEXT NOT NULL,
                hash TEXT NOT NULL UNIQUE,
                signature TEXT NOT NULL
            )",
            [],
        )
        .unwrap();
        drop(conn);

        let result = std::panic::catch_unwind(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async { AuditLedger::new(path.to_str().unwrap(), None, None).await })
        });
        assert!(result.is_err(), "should panic on old schema");
        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // fsck tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fsck_clean_chain_no_ceremony_events() {
        let (ledger, path) = test_ledger("fsck_clean").await;
        for i in 0..5 {
            ledger
                .record_event(test_input(&format!("event_{}", i)))
                .await
                .unwrap();
        }

        let report = ledger.fsck().await.unwrap();
        assert!(report.is_healthy());
        assert!(report.chain_valid);
        assert!(report.schema_monotonic);
        assert_eq!(report.total_events, 5);
        assert!(report.pubkey_missing_on_v3plus.is_empty());
        assert!(report.introduces.is_empty());
        assert!(report.revokes.is_empty());
        assert_eq!(report.signers_seen.len(), 1);
        assert!(report.revoked_key_uses.is_empty());
        assert!(report.duplicate_introduces.is_empty());
        assert!(report.warnings.is_empty());
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_fsck_schema_monotonicity_violation() {
        let (ledger, path) = test_ledger("fsck_sv_violation").await;

        // Record two v3 events normally
        ledger.record_event(test_input("first")).await.unwrap();
        let _second = ledger.record_event(test_input("second")).await.unwrap();

        // Corrupt the second event to schema_version=1 (regression)
        let conn = ledger.conn.lock().await;
        conn.call(move |c| {
            c.execute(
                "UPDATE audit_events SET schema_version = 1 WHERE id = 2",
                [],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .unwrap();
        drop(conn);

        let report = ledger.fsck().await.unwrap();
        // chain_valid is false because the hash preimage changes with schema_version
        assert!(!report.chain_valid);
        // schema_monotonic is false because v3 → v1
        assert!(!report.schema_monotonic);
        assert!(!report.is_healthy());
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_fsck_null_pubkey_on_v3_warns() {
        let (ledger, path) = test_ledger("fsck_null_pubkey").await;
        ledger.record_event(test_input("event")).await.unwrap();

        // NULL out the signing_key_pubkey (simulating a pre-4a v3 event)
        let conn = ledger.conn.lock().await;
        conn.call(|c| {
            c.execute(
                "UPDATE audit_events SET signing_key_pubkey = NULL WHERE id = 1",
                [],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .unwrap();
        drop(conn);

        let report = ledger.fsck().await.unwrap();
        // chain_valid stays true: verify_chain falls back to trial against
        // self.verifying_key for NULL pubkey rows, which succeeds here.
        assert!(report.chain_valid);
        assert_eq!(report.pubkey_missing_on_v3plus, vec![1i64]);
        assert!(!report.warnings.is_empty());
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_fsck_empty_chain() {
        let (ledger, path) = test_ledger("fsck_empty").await;
        let report = ledger.fsck().await.unwrap();
        assert!(report.is_healthy());
        assert_eq!(report.total_events, 0);
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_fsck_detects_introduce_event() {
        use crate::keymgmt;

        let (ledger, path) = test_ledger("fsck_introduce").await;

        let (_, new_bytes) = keymgmt::generate_keypair();
        let payload = keymgmt::sign_introduce(
            &ledger.signing_key,
            &new_bytes,
            keymgmt::CeremonyPurpose::LedgerSigning,
        );

        let input = EventInput {
            source: "keymgmt".into(),
            target: EVENT_KEY_INTRODUCE.into(),
            payload: serde_json::to_value(&payload).unwrap(),
            metadata: serde_json::json!({}),
            caller_key: None,
        };
        ledger.record_event(input).await.unwrap();

        let report = ledger.fsck().await.unwrap();
        assert!(report.is_healthy());
        assert_eq!(report.introduces.len(), 1);
        assert_eq!(report.introduces[0].1, hex::encode(new_bytes));
        assert_eq!(report.introduced_keys.len(), 1);
        cleanup(&path);
    }
}
