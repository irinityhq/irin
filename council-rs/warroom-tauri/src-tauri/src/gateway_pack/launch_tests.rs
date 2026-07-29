use super::*;

#[test]
fn packaged_frontend_must_not_start_council() {
    assert!(!frontend_may_start_council(true));
    assert!(frontend_may_start_council(false));
}

#[test]
fn cold_launch_single_governed_owner_when_frontend_defers() {
    // Policy: packaged + frontend deferred → exactly one native-owned route.
    assert!(!frontend_may_start_council(true));
    let route = cold_launch_owned_via_gateway(
        true,  // packaged
        true,  // via_gateway_default
        true,  // pack_auth_ok
        false, // frontend does not start
        true,  // would-have-won is irrelevant
    );
    assert_eq!(route, Some(true));
    // Only one owner: native governed, no second Direct child.
    assert_eq!(
        cold_launch_owned_via_gateway(true, true, true, false, false),
        Some(true)
    );
}

#[test]
fn cold_launch_race_sticks_direct_if_frontend_still_starts() {
    // Documents the fixed bug path: frontend Direct wins the lock and
    // native cannot correct ownership ("already tracked as running").
    let stuck = cold_launch_owned_via_gateway(true, true, true, true, true);
    assert_eq!(stuck, Some(false));
}

#[test]
fn decide_launch_via_gateway_requires_persisted_and_auth() {
    assert!(!decide_launch_via_gateway(false, true));
    assert!(!decide_launch_via_gateway(true, false));
    assert!(decide_launch_via_gateway(true, true));
}

#[test]
fn launch_resume_success_and_fail_closed() {
    assert_eq!(
        decide_launch_resume_outcome(true, true, false, true),
        LaunchResumeOutcome::Governed
    );
    assert_eq!(
        decide_launch_resume_outcome(true, false, true, true),
        LaunchResumeOutcome::Governed
    );
    // Pack never ready → Direct fail-closed (truthful; not invented governed).
    assert_eq!(
        decide_launch_resume_outcome(true, false, false, true),
        LaunchResumeOutcome::DirectFailClosed
    );
    // Pack ready but governed spawn fails → Direct fail-closed.
    assert_eq!(
        decide_launch_resume_outcome(true, true, false, false),
        LaunchResumeOutcome::DirectFailClosed
    );
    // Not enabled → Direct.
    assert_eq!(
        decide_launch_resume_outcome(false, true, true, true),
        LaunchResumeOutcome::DirectFailClosed
    );
}

#[test]
fn later_promote_without_manual_reenable() {
    assert!(may_promote_to_governed(true, Some(false), true));
    // Already governed: no promote needed.
    assert!(!may_promote_to_governed(true, Some(true), true));
    // No owned child.
    assert!(!may_promote_to_governed(true, None, true));
    // Operator disabled.
    assert!(!may_promote_to_governed(false, Some(false), true));
    // Pack still not ready.
    assert!(!may_promote_to_governed(true, Some(false), false));
}

#[test]
fn resume_action_avoids_full_start_when_project_running() {
    assert_eq!(
        decide_resume_pack_action(true, false),
        ResumePackAction::AlreadyReady
    );
    assert_eq!(
        decide_resume_pack_action(true, true),
        ResumePackAction::AlreadyReady
    );
    // Running but not yet auth-ready → wait/poll only (no Keychain compose rebuild).
    assert_eq!(
        decide_resume_pack_action(false, true),
        ResumePackAction::WaitOnly
    );
    // Down → full compose start once callers enter resume.
    assert_eq!(
        decide_resume_pack_action(false, false),
        ResumePackAction::FullStart
    );
}

#[test]
fn promote_early_window_bounds_resume_calls() {
    // Attempts 0..3 may resume; 4+ revalidate-only.
    assert!(promote_may_call_resume(0, 4, false));
    assert!(promote_may_call_resume(3, 4, false));
    assert!(!promote_may_call_resume(4, 4, false));
    assert!(!promote_may_call_resume(11, 4, false));
    // Ready pack never re-enters resume.
    assert!(!promote_may_call_resume(0, 4, true));
}
