//! precedent-integrity-watch sentinel tests.

use gateway_sidecar::watch::db::WatchDb;
use gateway_sidecar::watch::sentinels::precedent_integrity::PrecedentIntegritySentinel;
use gateway_sidecar::watch::Sentinel;
use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};

const LINE_A: &str = r#"{"session_id":"a","topic":"alpha","digest":"a"}"#;
const LINE_B: &str = r#"{"session_id":"b","topic":"bravo","digest":"b"}"#;
const LINE_C: &str = r#"{"session_id":"c","topic":"charl","digest":"c"}"#;

async fn migrated_watch_db(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("watch.db");
    let db = WatchDb::open(&path).await.unwrap();
    db.run_migrations().await.unwrap();
    path
}

fn write_index(path: &Path, lines: &[&str]) {
    let mut body = String::new();
    for line in lines {
        body.push_str(line);
        body.push('\n');
    }
    std::fs::write(path, body).unwrap();
}

fn append_index(path: &Path, line: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

fn flip_existing_byte(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let pos = bytes
        .iter()
        .position(|b| *b == b'a')
        .expect("fixture contains byte to flip");
    bytes[pos] = b'z';
    std::fs::write(path, bytes).unwrap();
}

fn stored_baseline(watch_db: &Path, index_path: &Path) -> Option<(i64, i64, String)> {
    let conn = rusqlite::Connection::open(watch_db).unwrap();
    conn.query_row(
        "SELECT line_count, byte_len, prefix_sha256
         FROM precedent_integrity_state
         WHERE path = ?1",
        rusqlite::params![index_path.to_string_lossy().as_ref()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
    .unwrap()
}

fn sentinel(watch_db: &Path, index_path: &Path) -> PrecedentIntegritySentinel {
    PrecedentIntegritySentinel::new(
        "precedent-integrity-watch",
        "sovereign",
        watch_db,
        index_path,
    )
}

/// `observe()` with production semantics for SQLITE_BUSY: the sentinel's
/// 50ms busy_timeout (sized to the ≤100ms fire budget) can lose a race with
/// the fixture WatchDb's async close on a loaded CI box — production surfaces
/// that as `TransientUpstream` and simply observes again next tick. Retry
/// here the same way instead of unwrapping the transient.
async fn observe_settled(s: &PrecedentIntegritySentinel) -> gateway_sidecar::watch::SentinelState {
    let mut last = String::new();
    for _ in 0..40 {
        match s.observe().await {
            Ok(state) => return state,
            Err(e) => {
                let msg = format!("{e:?}");
                assert!(
                    msg.contains("database is locked"),
                    "non-busy observe error: {msg}"
                );
                last = msg;
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
    }
    panic!("observe never settled past SQLITE_BUSY: {last}");
}

#[tokio::test]
async fn t12_precedent_integrity_bootstrap_writes_baseline_without_fire() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    write_index(&index_path, &[LINE_A, LINE_B]);

    let sentinel = sentinel(&watch_db, &index_path);
    let state = observe_settled(&sentinel).await;

    assert_eq!(state.payload["verdict"].as_str().unwrap(), "BOOTSTRAP");
    assert!(sentinel.interesting(&state).is_none());
    let stored = stored_baseline(&watch_db, &index_path).expect("baseline row");
    assert_eq!(stored.0, 2);
    assert_eq!(
        stored.1,
        std::fs::metadata(&index_path).unwrap().len() as i64
    );
    assert_eq!(stored.2.len(), 64, "full sha256 is persisted");
}

#[tokio::test]
async fn t12b_precedent_integrity_append_stays_healthy_and_advances_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    write_index(&index_path, &[LINE_A, LINE_B]);

    let sentinel = sentinel(&watch_db, &index_path);
    let _ = observe_settled(&sentinel).await;
    append_index(&index_path, LINE_C);

    let state = observe_settled(&sentinel).await;

    assert_eq!(state.payload["verdict"].as_str().unwrap(), "HEALTHY");
    assert!(state.payload["advanced"].as_bool().unwrap());
    assert!(sentinel.interesting(&state).is_none());
    let stored = stored_baseline(&watch_db, &index_path).expect("advanced baseline row");
    assert_eq!(stored.0, 3);
    assert_eq!(
        stored.1,
        std::fs::metadata(&index_path).unwrap().len() as i64
    );
}

#[tokio::test]
async fn t12c_precedent_integrity_mutation_fires_with_evidence_only_payload() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    write_index(&index_path, &[LINE_A, LINE_B]);

    let sentinel = sentinel(&watch_db, &index_path);
    let _ = observe_settled(&sentinel).await;
    flip_existing_byte(&index_path);

    let state = observe_settled(&sentinel).await;
    let reason = sentinel.interesting(&state).expect("mutation should fire");

    assert_eq!(state.payload["verdict"].as_str().unwrap(), "MUTATED");
    assert!(reason.contains("MUTATED"), "got: {reason}");
    assert_eq!(
        state.payload["evidence"]["verdict"].as_str().unwrap(),
        "MUTATED"
    );
    assert_eq!(
        state.payload["evidence"]["path"].as_str().unwrap(),
        index_path.to_string_lossy()
    );
    assert_eq!(
        state.payload["evidence"]["baseline"]["hash8"]
            .as_str()
            .unwrap()
            .len(),
        8
    );
    assert!(
        !state.payload.to_string().contains("alpha"),
        "payload must not leak session content"
    );

    let esc = sentinel.escalate(state, reason).await.unwrap();
    assert!(format!("{:?}", esc.urgency).contains("High"));
}

#[tokio::test]
async fn t12d_precedent_integrity_truncation_fires_with_old_and_new_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    write_index(&index_path, &[LINE_A, LINE_B, LINE_C]);

    let sentinel = sentinel(&watch_db, &index_path);
    let _ = observe_settled(&sentinel).await;
    write_index(&index_path, &[LINE_A, LINE_B]);

    let state = observe_settled(&sentinel).await;
    let reason = sentinel
        .interesting(&state)
        .expect("truncation should fire");

    assert_eq!(state.payload["verdict"].as_str().unwrap(), "TRUNCATED");
    assert!(reason.contains("TRUNCATED"), "got: {reason}");
    assert!(reason.contains("line_count 3 -> 2"), "got: {reason}");
    assert_eq!(
        state.payload["evidence"]["baseline"]["line_count"]
            .as_i64()
            .unwrap(),
        3
    );
    assert_eq!(
        state.payload["evidence"]["observed"]["line_count"]
            .as_i64()
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn t12e_precedent_integrity_fire_does_not_auto_resync_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    write_index(&index_path, &[LINE_A, LINE_B]);

    let sentinel = sentinel(&watch_db, &index_path);
    let _ = observe_settled(&sentinel).await;
    let baseline_before = stored_baseline(&watch_db, &index_path).expect("baseline row");
    flip_existing_byte(&index_path);

    let first = observe_settled(&sentinel).await;
    assert!(sentinel.interesting(&first).is_some());
    let baseline_after_first = stored_baseline(&watch_db, &index_path).expect("baseline row");
    assert_eq!(baseline_after_first, baseline_before);

    let second = observe_settled(&sentinel).await;
    let reason = sentinel
        .interesting(&second)
        .expect("second observe should still fire until manual resync");
    assert!(reason.contains("MUTATED"), "got: {reason}");
    let baseline_after_second = stored_baseline(&watch_db, &index_path).expect("baseline row");
    assert_eq!(baseline_after_second, baseline_before);
}

#[tokio::test]
async fn t12f_precedent_integrity_oversized_file_is_refused_without_fire() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(&index_path).unwrap();
    file.set_len((8 * 1024 * 1024) + 1).unwrap();

    let sentinel = sentinel(&watch_db, &index_path);
    let state = observe_settled(&sentinel).await;

    assert_eq!(state.payload["verdict"].as_str().unwrap(), "OVERSIZED");
    assert!(sentinel.interesting(&state).is_none());
    assert!(
        stored_baseline(&watch_db, &index_path).is_none(),
        "oversized file must not seed a baseline"
    );
}

#[tokio::test]
async fn t12h_precedent_integrity_fifo_at_index_path_fires_not_regular_without_hanging() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    write_index(&index_path, &[LINE_A, LINE_B]);

    let sentinel = sentinel(&watch_db, &index_path);
    let _ = observe_settled(&sentinel).await;
    let baseline_before = stored_baseline(&watch_db, &index_path).expect("baseline row");

    // Swap the index for a writer-less FIFO: without O_NONBLOCK the open(2)
    // alone would hang this test forever — the timeout IS the assertion.
    std::fs::remove_file(&index_path).unwrap();
    let c_path = std::ffi::CString::new(index_path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

    let state = tokio::time::timeout(std::time::Duration::from_secs(5), sentinel.observe())
        .await
        .expect("observe must not block on a FIFO")
        .unwrap();

    assert_eq!(state.payload["verdict"].as_str().unwrap(), "NOT_REGULAR");
    let reason = sentinel
        .interesting(&state)
        .expect("type swap with a baseline is tamper evidence");
    assert!(reason.contains("NOT_REGULAR"), "got: {reason}");
    assert_eq!(
        stored_baseline(&watch_db, &index_path).expect("baseline row"),
        baseline_before,
        "type swap must not advance the baseline"
    );
}

#[tokio::test]
async fn t12i_precedent_integrity_oversized_growth_with_mutated_prefix_still_fires() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    write_index(&index_path, &[LINE_A, LINE_B]);

    let sentinel = sentinel(&watch_db, &index_path);
    let _ = observe_settled(&sentinel).await;

    // Mutate an existing byte, then bloat the file past the cap — the bloat
    // must not hide the mutation behind the OVERSIZED verdict.
    flip_existing_byte(&index_path);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&index_path)
        .unwrap();
    file.set_len((8 * 1024 * 1024) + 64).unwrap();

    let state = observe_settled(&sentinel).await;
    let reason = sentinel
        .interesting(&state)
        .expect("oversized bloat must not mask a prefix mutation");

    assert_eq!(state.payload["verdict"].as_str().unwrap(), "MUTATED");
    assert!(reason.contains("MUTATED"), "got: {reason}");
}

#[tokio::test]
async fn t12j_precedent_integrity_oversized_growth_with_clean_prefix_refuses_without_fire() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    write_index(&index_path, &[LINE_A, LINE_B]);

    let sentinel = sentinel(&watch_db, &index_path);
    let _ = observe_settled(&sentinel).await;
    let baseline_before = stored_baseline(&watch_db, &index_path).expect("baseline row");

    // Grow past the cap without touching the historical prefix (sparse tail).
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&index_path)
        .unwrap();
    file.set_len((8 * 1024 * 1024) + 64).unwrap();

    let state = observe_settled(&sentinel).await;

    assert_eq!(state.payload["verdict"].as_str().unwrap(), "OVERSIZED");
    assert!(sentinel.interesting(&state).is_none());
    assert_eq!(
        stored_baseline(&watch_db, &index_path).expect("baseline row"),
        baseline_before,
        "oversized file must not advance the baseline"
    );
}

#[tokio::test]
async fn t12k_precedent_integrity_symlink_swap_fires_not_regular_without_following() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    write_index(&index_path, &[LINE_A, LINE_B]);

    let sentinel = sentinel(&watch_db, &index_path);
    let _ = observe_settled(&sentinel).await;
    let baseline_before = stored_baseline(&watch_db, &index_path).expect("baseline row");

    // Swap the index for a symlink to a forged file whose prefix matches the
    // baseline exactly — following it would look HEALTHY. O_NOFOLLOW must
    // refuse the open and fire NOT_REGULAR instead.
    let forged = tmp.path().join("forged.jsonl");
    std::fs::copy(&index_path, &forged).unwrap();
    append_index(&forged, LINE_C);
    std::fs::remove_file(&index_path).unwrap();
    std::os::unix::fs::symlink(&forged, &index_path).unwrap();

    let state = observe_settled(&sentinel).await;

    assert_eq!(state.payload["verdict"].as_str().unwrap(), "NOT_REGULAR");
    let reason = sentinel
        .interesting(&state)
        .expect("symlink swap with a baseline is tamper evidence");
    assert!(reason.contains("NOT_REGULAR"), "got: {reason}");
    assert_eq!(
        stored_baseline(&watch_db, &index_path).expect("baseline row"),
        baseline_before,
        "symlink swap must not advance the baseline"
    );
}

#[tokio::test]
async fn t12l_precedent_integrity_forged_oversized_baseline_fires_mutated_without_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    write_index(&index_path, &[LINE_A, LINE_B]);

    let sentinel = sentinel(&watch_db, &index_path);
    let _ = observe_settled(&sentinel).await;

    // Forge the stored baseline to claim more bytes than the bounded read
    // buffer can ever hold. A legitimate baseline is only written from a
    // read that fit under the cap, so this row is tamper evidence in itself
    // — the observe must fire MUTATED, not panic on the prefix slice.
    let conn = rusqlite::Connection::open(&watch_db).unwrap();
    conn.execute(
        "UPDATE precedent_integrity_state SET byte_len = ?1 WHERE path = ?2",
        rusqlite::params![
            (8 * 1024 * 1024_i64) + 128,
            index_path.to_string_lossy().as_ref()
        ],
    )
    .unwrap();
    drop(conn);

    // Bloat the file past the cap so the oversized prefix-verify path runs.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&index_path)
        .unwrap();
    file.set_len((8 * 1024 * 1024) + 256).unwrap();

    let state = observe_settled(&sentinel).await;
    let reason = sentinel
        .interesting(&state)
        .expect("impossible baseline row is tamper evidence");

    assert_eq!(state.payload["verdict"].as_str().unwrap(), "MUTATED");
    assert!(reason.contains("MUTATED"), "got: {reason}");
}

#[tokio::test]
async fn t12m_precedent_integrity_negative_baseline_byte_len_fires_mutated_without_error() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();
    write_index(&index_path, &[LINE_A, LINE_B]);

    let sentinel = sentinel(&watch_db, &index_path);
    let _ = observe_settled(&sentinel).await;

    // A negative byte_len can never be written by this code — it skips the
    // TRUNCATED compare (observed length is never negative) and must fire
    // MUTATED as an impossible row, not surface as a transient observe error.
    let conn = rusqlite::Connection::open(&watch_db).unwrap();
    conn.execute(
        "UPDATE precedent_integrity_state SET byte_len = -1 WHERE path = ?1",
        rusqlite::params![index_path.to_string_lossy().as_ref()],
    )
    .unwrap();
    drop(conn);

    let state = observe_settled(&sentinel).await;
    let reason = sentinel
        .interesting(&state)
        .expect("impossible negative baseline row is tamper evidence");

    assert_eq!(state.payload["verdict"].as_str().unwrap(), "MUTATED");
    assert!(reason.contains("MUTATED"), "got: {reason}");
}

#[tokio::test]
async fn t12g_precedent_integrity_missing_file_is_bootstrap_pending_without_fire() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = migrated_watch_db(&tmp).await;
    let index_path = tmp.path().join("sessions").join("index.jsonl");

    let sentinel = sentinel(&watch_db, &index_path);
    let state = observe_settled(&sentinel).await;

    assert_eq!(
        state.payload["verdict"].as_str().unwrap(),
        "BOOTSTRAP_PENDING"
    );
    assert!(sentinel.interesting(&state).is_none());
    assert!(stored_baseline(&watch_db, &index_path).is_none());
}
