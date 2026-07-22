//! P0-1 daily spend cap seam — integration tests mirroring socket_perms.rs.
//! Exercises the pure env parser the boot path calls; no process-global env.

use gateway_sidecar::watch::db::{
    daily_spend_cap, daily_spend_cap_from_env, init_daily_spend_cap_at_boot, DailySpendCapError,
    DAILY_SPEND_CAP, DAILY_SPEND_CAP_ENV_VAR,
};

#[test]
fn cap_env_above_const_refuses_startup_no_fallback() {
    let err = daily_spend_cap_from_env(Some("75")).unwrap_err();
    assert!(matches!(
        err,
        DailySpendCapError::AboveCeiling { value: 75.0, .. }
    ));
    // Must not silently fall back to the ceiling.
    assert_ne!(
        daily_spend_cap_from_env(Some("75")).ok(),
        Some(DAILY_SPEND_CAP)
    );
}

#[test]
fn cap_env_garbage_refuses_startup_no_fallback() {
    let err = daily_spend_cap_from_env(Some("twenty-five")).unwrap_err();
    assert!(matches!(err, DailySpendCapError::BadValue { .. }));
}

#[test]
fn boot_init_binds_gauge_and_enforced_cap() {
    // Uninitialized path (integration binary before init) falls back to ceiling.
    assert_eq!(daily_spend_cap(), DAILY_SPEND_CAP);

    // Boot init with a lowered canary cap. OnceLock is process-global; if another
    // test in this binary initialized first, still assert the bound cap.
    match init_daily_spend_cap_at_boot(Some("25")) {
        Ok(()) => {
            assert_eq!(daily_spend_cap(), 25.0);
            assert_eq!(daily_spend_cap_from_env(Some("25")).unwrap(), 25.0);
        }
        Err(DailySpendCapError::AlreadyInitialized) => {
            assert_eq!(
                daily_spend_cap(),
                25.0,
                "boot init must have bound canary cap before AlreadyInitialized"
            );
        }
        Err(e) => panic!("unexpected boot init error: {e}"),
    }
}

#[test]
fn env_var_name_documented() {
    assert_eq!(DAILY_SPEND_CAP_ENV_VAR, "DAILY_SPEND_CAP_USD");
}
