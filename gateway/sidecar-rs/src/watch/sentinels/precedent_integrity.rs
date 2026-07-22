//! `precedent-integrity-watch` sentinel.
//!
//! Watches the Council precedent index (`sessions/index.jsonl`) as an
//! append-only byte stream. The persisted baseline stores the line count, byte
//! length, and SHA-256 of the prefix observed at the last healthy checkpoint.
//! Later observes verify that old prefix before accepting any appended bytes.
//!
//! Non-goals:
//! - Detection only, not prevention. A root attacker can rewrite both the
//!   precedent index and watch.db; this sentinel gives evidence when the stores
//!   diverge, at the same altitude as the watch/arm audit chains.
//! - Baseline forgery is possible for an attacker with watch.db write access.
//!   Manual resync after an acknowledged fire is deleting the state row for the
//!   watched path so the next observe bootstraps a fresh baseline.
//! - Known legitimate triggers: council `--reindex` and
//!   `POST /api/precedent/reindex` rewrite `index.jsonl` in place and WILL
//!   produce a MUTATED/TRUNCATED fire. That is by design — the sentinel cannot
//!   distinguish an authorized rewrite from tampering. After an operator-known
//!   reindex, resync the same way as after an acknowledged fire.
//! - No LLM calls. If it needs an LLM, it is a Worker, not a Sentinel.

use crate::watch::{
    EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
};
use async_trait::async_trait;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
const MANUAL_RESYNC: &str =
    "delete the precedent_integrity_state row for this path after operator acknowledgement";

#[derive(Debug, Clone)]
struct Baseline {
    line_count: i64,
    prefix_sha256: String,
    byte_len: i64,
}

#[derive(Debug, Clone)]
struct Observed {
    exists: bool,
    line_count: Option<i64>,
    byte_len: i64,
    hash: Option<String>,
    prefix_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct IntegrityObservation {
    verdict: &'static str,
    baseline: Option<Baseline>,
    observed: Observed,
    advanced: bool,
}

pub struct PrecedentIntegritySentinel {
    name: String,
    tenant: String,
    watch_db_path: PathBuf,
    index_path: PathBuf,
    cooldown: Duration,
}

impl PrecedentIntegritySentinel {
    pub fn new(name: &str, tenant: &str, watch_db_path: &Path, index_path: &Path) -> Self {
        Self {
            name: name.into(),
            tenant: tenant.into(),
            watch_db_path: watch_db_path.to_path_buf(),
            index_path: index_path.to_path_buf(),
            cooldown: Duration::from_secs(60),
        }
    }

    pub fn from_env_or_default(name: &str, tenant: &str, watch_db_path: &Path) -> Self {
        Self::new(
            name,
            tenant,
            watch_db_path,
            &Self::index_path_from_env_or_default(),
        )
    }

    pub fn with_cooldown(mut self, d: Duration) -> Self {
        self.cooldown = d;
        self
    }

    pub fn index_path_from_env_or_default() -> PathBuf {
        std::env::var("PRECEDENT_INDEX_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Self::default_index_path())
    }

    pub fn should_register_from_env_or_default() -> bool {
        std::env::var_os("PRECEDENT_INDEX_PATH").is_some() || Self::default_index_path().exists()
    }

    fn default_index_path() -> PathBuf {
        std::env::var("COUNCIL_SESSIONS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("sessions"))
            .join("index.jsonl")
    }

    fn observe_blocking(
        watch_db_path: &Path,
        index_path: &Path,
        now_ms: i64,
    ) -> Result<IntegrityObservation, String> {
        let conn =
            rusqlite::Connection::open(watch_db_path).map_err(|e| format!("open watch.db: {e}"))?;
        conn.busy_timeout(Duration::from_millis(50))
            .map_err(|e| format!("set busy_timeout: {e}"))?;

        let path_key = path_key(index_path);
        let baseline = read_baseline(&conn, &path_key)?;

        // Open O_NONBLOCK, then fstat the HANDLE — one inode for both the
        // type check and the read (no stat/open TOCTOU), and the open itself
        // can never hang: open(2) on a writer-less FIFO blocks without
        // O_NONBLOCK, which would pin one of the watch runtime's blocking
        // threads forever (the observe timeout only detaches the task).
        // O_NONBLOCK is a no-op for regular-file reads.
        let file = match open_nonblocking(index_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let observed = Observed {
                    exists: false,
                    line_count: baseline.as_ref().map(|_| 0),
                    byte_len: 0,
                    hash: baseline.as_ref().map(|_| sha256_hex(&[])),
                    prefix_hash: None,
                };
                let verdict = if baseline.is_some() {
                    "TRUNCATED"
                } else {
                    "BOOTSTRAP_PENDING"
                };
                return Ok(IntegrityObservation {
                    verdict,
                    baseline,
                    observed,
                    advanced: false,
                });
            }
            // Symlink where the append-only index should be: same tamper
            // class as a FIFO/device — refuse to follow, fire on baseline.
            Err(e) if is_symlink_refusal(&e) => {
                let verdict = if baseline.is_some() {
                    "NOT_REGULAR"
                } else {
                    "BOOTSTRAP_PENDING"
                };
                return Ok(IntegrityObservation {
                    verdict,
                    baseline,
                    observed: Observed {
                        exists: true,
                        line_count: None,
                        byte_len: 0,
                        hash: None,
                        prefix_hash: None,
                    },
                    advanced: false,
                });
            }
            Err(e) => return Err(format!("open precedent index: {e}")),
        };

        let metadata = file
            .metadata()
            .map_err(|e| format!("fstat precedent index: {e}"))?;

        // A FIFO / device / directory where the append-only index should be
        // is itself tamper evidence when a baseline exists — and reading it
        // could block or never terminate, so refuse the read outright.
        if !metadata.is_file() {
            let verdict = if baseline.is_some() {
                "NOT_REGULAR"
            } else {
                "BOOTSTRAP_PENDING"
            };
            return Ok(IntegrityObservation {
                verdict,
                baseline,
                observed: Observed {
                    exists: true,
                    line_count: None,
                    byte_len: 0,
                    hash: None,
                    prefix_hash: None,
                },
                advanced: false,
            });
        }

        // Bounded read: never materialize more than MAX + 1 bytes, no matter
        // what fstat claimed (kills the len/read race). A baseline is only
        // ever written from a read that fit under MAX, so when the file is
        // oversized the buffer still contains the full historical prefix —
        // verify it BEFORE refusing, otherwise bloating the file past the cap
        // would hide a prefix mutation behind the OVERSIZED verdict.
        use std::io::Read;
        let mut bytes = Vec::new();
        file.take(MAX_INDEX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("read precedent index: {e}"))?;

        if bytes.len() as u64 > MAX_INDEX_BYTES {
            if let Some(baseline) = baseline {
                // A baseline is only ever written from a read that fit under
                // MAX, so byte_len must be non-negative and fit inside the
                // bounded buffer. A row claiming otherwise is not a bigger
                // file — it is a forged or corrupted baseline, which is
                // itself tamper evidence, never a transient error. Negative
                // values collapse into the same guard via usize::MAX.
                let prefix_len = usize::try_from(baseline.byte_len).unwrap_or(usize::MAX);
                if prefix_len > bytes.len() {
                    return Ok(IntegrityObservation {
                        verdict: "MUTATED",
                        baseline: Some(baseline),
                        observed: Observed {
                            exists: true,
                            line_count: None,
                            byte_len: metadata.len() as i64,
                            hash: None,
                            prefix_hash: None,
                        },
                        advanced: false,
                    });
                }
                let observed_prefix_hash = sha256_hex(&bytes[..prefix_len]);
                if observed_prefix_hash != baseline.prefix_sha256 {
                    return Ok(IntegrityObservation {
                        verdict: "MUTATED",
                        baseline: Some(baseline),
                        observed: Observed {
                            exists: true,
                            line_count: None,
                            byte_len: metadata.len() as i64,
                            hash: None,
                            prefix_hash: Some(observed_prefix_hash),
                        },
                        advanced: false,
                    });
                }
                return Ok(IntegrityObservation {
                    verdict: "OVERSIZED",
                    baseline: Some(baseline),
                    observed: Observed {
                        exists: true,
                        line_count: None,
                        byte_len: metadata.len() as i64,
                        hash: None,
                        prefix_hash: Some(observed_prefix_hash),
                    },
                    advanced: false,
                });
            }
            return Ok(IntegrityObservation {
                verdict: "OVERSIZED",
                baseline: None,
                observed: Observed {
                    exists: true,
                    line_count: None,
                    byte_len: metadata.len() as i64,
                    hash: None,
                    prefix_hash: None,
                },
                advanced: false,
            });
        }

        let observed_hash = sha256_hex(&bytes);
        let observed_line_count = count_jsonl_lines(&bytes);
        let observed_byte_len = bytes.len() as i64;

        let Some(baseline) = baseline else {
            write_baseline(
                &conn,
                &path_key,
                observed_line_count,
                observed_byte_len,
                &observed_hash,
                now_ms,
            )?;
            return Ok(IntegrityObservation {
                verdict: "BOOTSTRAP",
                baseline: None,
                observed: Observed {
                    exists: true,
                    line_count: Some(observed_line_count),
                    byte_len: observed_byte_len,
                    hash: Some(observed_hash),
                    prefix_hash: None,
                },
                advanced: true,
            });
        };

        if observed_byte_len < baseline.byte_len {
            return Ok(IntegrityObservation {
                verdict: "TRUNCATED",
                baseline: Some(baseline),
                observed: Observed {
                    exists: true,
                    line_count: Some(observed_line_count),
                    byte_len: observed_byte_len,
                    hash: Some(observed_hash),
                    prefix_hash: None,
                },
                advanced: false,
            });
        }

        // Same impossible-row guard as the oversized path: a negative
        // byte_len skips the TRUNCATED compare above (observed is never
        // negative), so it must fire here instead of erroring out.
        let prefix_len = usize::try_from(baseline.byte_len).unwrap_or(usize::MAX);
        if prefix_len > bytes.len() {
            return Ok(IntegrityObservation {
                verdict: "MUTATED",
                baseline: Some(baseline),
                observed: Observed {
                    exists: true,
                    line_count: Some(observed_line_count),
                    byte_len: observed_byte_len,
                    hash: Some(observed_hash),
                    prefix_hash: None,
                },
                advanced: false,
            });
        }
        let observed_prefix_hash = sha256_hex(&bytes[..prefix_len]);
        if observed_prefix_hash != baseline.prefix_sha256 {
            return Ok(IntegrityObservation {
                verdict: "MUTATED",
                baseline: Some(baseline),
                observed: Observed {
                    exists: true,
                    line_count: Some(observed_line_count),
                    byte_len: observed_byte_len,
                    hash: Some(observed_hash),
                    prefix_hash: Some(observed_prefix_hash),
                },
                advanced: false,
            });
        }

        let advanced = observed_byte_len > baseline.byte_len;
        if advanced {
            write_baseline(
                &conn,
                &path_key,
                observed_line_count,
                observed_byte_len,
                &observed_hash,
                now_ms,
            )?;
        }

        Ok(IntegrityObservation {
            verdict: "HEALTHY",
            baseline: Some(baseline),
            observed: Observed {
                exists: true,
                line_count: Some(observed_line_count),
                byte_len: observed_byte_len,
                hash: Some(observed_hash),
                prefix_hash: Some(observed_prefix_hash),
            },
            advanced,
        })
    }

    fn payload(&self, obs: IntegrityObservation) -> serde_json::Value {
        let evidence = match obs.verdict {
            "TRUNCATED" | "MUTATED" | "NOT_REGULAR" => obs.baseline.as_ref().map(|baseline| {
                evidence_json(obs.verdict, &self.index_path, baseline, &obs.observed)
            }),
            _ => None,
        };

        serde_json::json!({
            "path": path_key(&self.index_path),
            "verdict": obs.verdict,
            "baseline": obs.baseline.as_ref().map(baseline_json),
            "observed": observed_json(&obs.observed),
            "advanced": obs.advanced,
            "max_byte_len": MAX_INDEX_BYTES,
            "manual_resync": MANUAL_RESYNC,
            "evidence": evidence,
        })
    }
}

#[async_trait]
impl Sentinel for PrecedentIntegritySentinel {
    fn name(&self) -> &str {
        &self.name
    }

    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn tier(&self) -> Tier {
        Tier::Polling
    }

    fn cooldown(&self) -> Duration {
        self.cooldown
    }

    async fn observe(&self) -> Result<SentinelState, ObserveError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let watch_db_path = self.watch_db_path.clone();
        let index_path = self.index_path.clone();
        let obs = tokio::task::spawn_blocking(move || {
            Self::observe_blocking(&watch_db_path, &index_path, now_ms)
        })
        .await
        .map_err(|e| ObserveError::Fatal(format!("join: {e}")))?
        .map_err(ObserveError::TransientUpstream)?;

        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at: now_ms,
            payload: self.payload(obs),
        })
    }

    fn interesting(&self, state: &SentinelState) -> Option<String> {
        let verdict = state.payload["verdict"].as_str().unwrap_or("UNKNOWN");
        if verdict != "TRUNCATED" && verdict != "MUTATED" && verdict != "NOT_REGULAR" {
            return None;
        }
        let evidence = &state.payload["evidence"];
        let baseline = &evidence["baseline"];
        let observed = &evidence["observed"];
        Some(format!(
            "precedent index {verdict}: line_count {} -> {}, byte_len {} -> {}",
            baseline["line_count"].as_i64().unwrap_or(0),
            observed["line_count"].as_i64().unwrap_or(0),
            baseline["byte_len"].as_i64().unwrap_or(0),
            observed["byte_len"].as_i64().unwrap_or(0)
        ))
    }

    async fn escalate(
        &self,
        state: SentinelState,
        reason: String,
    ) -> Result<Escalation, EscalateError> {
        Ok(Escalation {
            state,
            reason,
            urgency: Urgency::High,
        })
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Open read-only with `O_NONBLOCK | O_NOFOLLOW` so a FIFO planted at the
/// index path can never hang the open (writer-less FIFO opens block
/// indefinitely without it) and a symlink swap is refused instead of
/// silently followed — the caller maps the resulting `ELOOP` to the same
/// tamper verdict as a non-regular file. For regular files the flags have
/// no effect on reads.
fn open_nonblocking(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
}

/// `open(2)` with `O_NOFOLLOW` fails with `ELOOP` (or `EMLINK` on some
/// BSDs) when the final path component is a symlink.
fn is_symlink_refusal(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(code) if code == libc::ELOOP || code == libc::EMLINK)
}

fn read_baseline(conn: &rusqlite::Connection, path: &str) -> Result<Option<Baseline>, String> {
    conn.query_row(
        "SELECT line_count, prefix_sha256, byte_len
         FROM precedent_integrity_state
         WHERE path = ?1",
        rusqlite::params![path],
        |r| {
            Ok(Baseline {
                line_count: r.get(0)?,
                prefix_sha256: r.get(1)?,
                byte_len: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("query precedent_integrity_state: {e}"))
}

fn write_baseline(
    conn: &rusqlite::Connection,
    path: &str,
    line_count: i64,
    byte_len: i64,
    prefix_sha256: &str,
    now_ms: i64,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO precedent_integrity_state
            (path, line_count, prefix_sha256, byte_len, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(path) DO UPDATE SET
            line_count = excluded.line_count,
            prefix_sha256 = excluded.prefix_sha256,
            byte_len = excluded.byte_len,
            updated_at_ms = excluded.updated_at_ms",
        rusqlite::params![path, line_count, prefix_sha256, byte_len, now_ms],
    )
    .map(|_| ())
    .map_err(|e| format!("upsert precedent_integrity_state: {e}"))
}

fn count_jsonl_lines(bytes: &[u8]) -> i64 {
    bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .count() as i64
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn hash8(hash: &str) -> String {
    hash.chars().take(8).collect()
}

fn baseline_json(baseline: &Baseline) -> serde_json::Value {
    serde_json::json!({
        "line_count": baseline.line_count,
        "byte_len": baseline.byte_len,
        "hash8": hash8(&baseline.prefix_sha256),
    })
}

fn observed_json(observed: &Observed) -> serde_json::Value {
    serde_json::json!({
        "exists": observed.exists,
        "line_count": observed.line_count,
        "byte_len": observed.byte_len,
        "hash8": observed.hash.as_deref().map(hash8),
        "prefix_hash8": observed.prefix_hash.as_deref().map(hash8),
    })
}

fn evidence_json(
    verdict: &str,
    path: &Path,
    baseline: &Baseline,
    observed: &Observed,
) -> serde_json::Value {
    serde_json::json!({
        "path": path_key(path),
        "verdict": verdict,
        "baseline": baseline_json(baseline),
        "observed": observed_json(observed),
    })
}
