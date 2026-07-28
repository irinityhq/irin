use super::cancel_and_join_ws_run;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn disconnect_cancellation_reaches_owned_run() {
    let cancel = CancellationToken::new();
    let child_cancel = cancel.clone();
    let observed = Arc::new(AtomicBool::new(false));
    let child_observed = observed.clone();
    let run = tokio::spawn(async move {
        child_cancel.cancelled().await;
        child_observed.store(true, Ordering::SeqCst);
    });

    let cooperative = cancel_and_join_ws_run(cancel, run, Duration::from_millis(100)).await;

    assert!(
        cooperative,
        "token-aware run should stop within the grace period"
    );
    assert!(observed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn disconnect_cleanup_aborts_an_uncooperative_run_after_grace() {
    let cancel = CancellationToken::new();
    let run = tokio::spawn(async { std::future::pending::<()>().await });

    let cooperative = cancel_and_join_ws_run(cancel, run, Duration::from_millis(10)).await;

    assert!(
        !cooperative,
        "stuck run must be aborted rather than detached"
    );
}
