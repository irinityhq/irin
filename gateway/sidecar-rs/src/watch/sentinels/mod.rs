//! Built-in Sentinel implementations.
//!
//! The registry exposes file inbox, silence, queue depth, watch health,
//! ledger delta, sequence watch, anomaly, completion verification, and precedent integrity.

pub mod anomaly;
pub mod completion_verify;
pub mod file_inbox;
pub mod ledger_delta;
pub mod precedent_integrity;
pub mod queue_depth;
pub mod sequence_watch;
pub mod silence;
pub mod watch_health;

fn validate_watch_db_path(path: &std::path::Path) -> anyhow::Result<()> {
    if !path.exists() {
        anyhow::bail!(
            "watch.db missing or unreadable at {} — check bind mount / WATCH_DB_PATH",
            path.display()
        );
    }
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| anyhow::anyhow!("watch.db not openable read-only at {}: {e}", path.display()))?;
    Ok(())
}
