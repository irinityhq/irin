//! p0e-real-charge — Invariant + P0-E.
//!
//! "One real/sandbox charge-count integration test reconciling billed == M,
//! across the real CouncilState idem handlers."
//!
//! The heavy lifting lives in `test/p0e_real_charge.sh` (service orchestration:
//! gateway + sidecar via docker compose, council on 127.0.0.1:8765, CDC seed,
//! N=3 idempotency-colliding submissions, ledger + header reconciliation).
//! This file contributes:
//!
//! 1. Always-on, no-spend guard tests that pin the harness contract in CI:
//!    the script exists, refuses without `P0E_ENABLE=1`, defaults its hard cap
//!    to $5, and is wired into NO default cargo/Makefile/CI path.
//! 2. A unit test pinning the harness's shell-side Idempotency-Key formula
//!    (`<tenant>:<pending_id>`) to the dispatcher's real
//!    `build_council_triage_headers` output, so the collider submissions are
//!    guaranteed to land on the SAME tenant-scoped key the live dispatcher
//!    uses (the whole point of the collision).
//! 3. The `#[ignore]`d, env-gated wrapper `test_real_charge_count_idem_collision`
//!    that runs the script for cargo discoverability. It NEVER runs in CI
//!    default (ignored) and refuses even under `--ignored` unless
//!    `P0E_ENABLE=1` (double gate). Real spend is bounded by the harness-local
//!    $5 cap + the p0c per-directive reservation ceiling.

use std::path::PathBuf;

/// Repo root = parent of CARGO_MANIFEST_DIR (sidecar-rs/..).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("sidecar-rs has a parent dir")
        .to_path_buf()
}

/// P0-E guard: the orchestration harness exists, is executable, refuses
/// loudly without the opt-in env gate, and defaults its hard cap to $5.
/// This is the CI-enforced contract for the script (same pattern as
/// `test_runbook_arming_authorization_exists` for the runbook).
#[test]
fn test_p0e_harness_script_exists_and_gated() {
    let script = repo_root().join("test/p0e_real_charge.sh");
    assert!(
        script.is_file(),
        "test/p0e_real_charge.sh missing — the P0-E real-charge harness (charge-count invariant) \
         must live at test/p0e_real_charge.sh"
    );

    let body = std::fs::read_to_string(&script).expect("readable harness script");

    // Opt-in gate: the script must check P0E_ENABLE and refuse (exit non-zero)
    // when it is not exactly "1".
    assert!(
        body.contains("P0E_ENABLE"),
        "harness must be gated on P0E_ENABLE=1"
    );
    assert!(
        body.contains("REFUSING"),
        "harness must refuse loudly (the word REFUSING) when P0E_ENABLE != 1"
    );

    // $5 hard cap, env-overridable but defaulting to 5.
    assert!(
        body.contains("P0E_SPEND_CAP:-5"),
        "harness must default P0E_SPEND_CAP to 5 (the P0-E $5 hard cap)"
    );

    // Live mode requires a second explicit confirmation on top of P0E_ENABLE.
    assert!(
        body.contains("P0E_CONFIRM_REAL_SPEND"),
        "live (real-spend) mode must require P0E_CONFIRM_REAL_SPEND=yes in addition to P0E_ENABLE=1"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = script.metadata().unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "test/p0e_real_charge.sh must be executable"
        );
    }
}

/// actually RUN the harness
/// with `P0E_ENABLE` scrubbed from the environment and assert it exits 2
/// (the refuse path) without touching docker/services. The string-presence
/// test above pins the contract text; this pins the behavior — a future
/// refactor that breaks the gate while keeping the words fails HERE.
#[test]
fn test_p0e_harness_refuses_without_enable_gate() {
    let root = repo_root();
    let script = root.join("test/p0e_real_charge.sh");
    assert!(script.is_file(), "test/p0e_real_charge.sh missing");

    let output = std::process::Command::new("bash")
        .arg(&script)
        .env_remove("P0E_ENABLE")
        .current_dir(&root)
        .output()
        .expect("failed to spawn p0e_real_charge.sh");

    assert_eq!(
        output.status.code(),
        Some(2),
        "harness must exit 2 (refuse) when P0E_ENABLE is unset; stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("REFUSING"),
        "refusal must be loud; got: {combined}"
    );
}

/// P0-E guard: the harness must be impossible to trigger from any default
/// path — no Makefile target and no CI workflow may reference it. `cargo
/// test` cannot run it either: the wrapper below is #[ignore]'d AND
/// env-gated.
#[test]
fn test_p0e_never_wired_into_default_ci_paths() {
    let root = repo_root();

    let makefile =
        std::fs::read_to_string(root.join("Makefile")).expect("Makefile readable at repo root");
    assert!(
        !makefile.contains("p0e_real_charge"),
        "Makefile must NOT wire the P0-E real-charge harness into any target — \
         it is operator-invoked only (P0E_ENABLE=1 bash test/p0e_real_charge.sh)"
    );

    let workflows = root.join(".github/workflows");
    if workflows.is_dir() {
        for entry in std::fs::read_dir(&workflows).expect("readable workflows dir") {
            let path = entry.expect("dir entry").path();
            if path.is_file() {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                assert!(
                    !text.contains("p0e_real_charge"),
                    "CI workflow {} must NOT reference the P0-E real-charge harness",
                    path.display()
                );
            }
        }
    }
}

/// Load-bearing link between the shell harness and the real dispatcher code:
/// the script computes the collider Idempotency-Key as
/// `"$P0E_TENANT:$PENDING_ID"` (plain shell concatenation). That is only
/// correct because BOTH legs pass `safe_tenant_token` /
/// `safe_escalation_id_segment` sanitization unchanged for the harness's
/// charset (tenant `p0e-real-charge`, producer ids `causal-<hex>`). Pin it
/// against the real `build_council_triage_headers` so any future
/// sanitization change breaks THIS test instead of silently de-colliding
/// the harness.
#[test]
fn test_p0e_idem_key_formula_matches_dispatcher() {
    use gateway_sidecar::watch::dispatcher::build_council_triage_headers;

    let tenant = "p0e-real-charge";
    let pending_id = "causal-0123456789abcdef";

    let headers = build_council_triage_headers(tenant, pending_id);
    let key = headers
        .get("idempotency-key")
        .expect("dispatcher emits Idempotency-Key")
        .to_str()
        .expect("header is ascii");

    assert_eq!(
        key,
        format!("{tenant}:{pending_id}"),
        "shell-side `$P0E_TENANT:$PENDING_ID` must equal the dispatcher's qualified \
         Idempotency-Key for the harness charset — collider requests would otherwise \
         miss the dedup namespace and trigger a SECOND real deliberation"
    );
}

/// The runbook must document the separately gated real-charge reconciliation
/// path and its exact billing invariant.
#[test]
fn test_p0e_runbook_documents_real_charge_reconciliation() {
    let runbook =
        std::fs::read_to_string(repo_root().join("docs/runbooks/arming-authorization.md"))
            .expect("docs/runbooks/arming-authorization.md readable");

    assert!(
        runbook.contains("## Optional real-charge reconciliation"),
        "arming-authorization.md must document optional real-charge reconciliation"
    );
    assert!(
        runbook.contains("billed == M"),
        "runbook P0-E section must state the billed == M reconciliation bar"
    );
    assert!(
        runbook.contains("p0e_real_charge.sh"),
        "runbook P0-E section must point at the harness script"
    );
}

/// charge-count invariant + P0-E: the real charge-count integration test.
///
/// NEVER runs by default: `#[ignore]` keeps it out of `cargo test`, and even
/// `cargo test -- --ignored` refuses unless `P0E_ENABLE=1` (double gate).
/// With the gate open it delegates to `test/p0e_real_charge.sh`, which:
///
///   * brings up gateway + sidecar (current tree, local-overlay rebuild) with
///     the dispatcher/producer armed and council on 127.0.0.1:8765
///     (`P0E_MODE=stub` no-spend stub by default; `P0E_MODE=live` +
///     `P0E_CONFIRM_REAL_SPEND=yes` for the one sanctioned real charge),
///   * seeds ONE escalation; the live dispatcher's own submission is the
///     M=1 real deliberation,
///   * fires N-1 collider submissions on the SAME tenant-scoped
///     Idempotency-Key through the REAL CouncilState idem handlers
///     (council_idem_claim / PENDING_TTL=300s) and asserts every one of
///     them deduped (409 pending/conflict or 200 + X-Idempotency-Replay),
///   * reconciles billed == M == 1: exactly one spend_ledger settle for the
///     escalation (settled delta == realized_cost_usd, reservation NULLed),
///     zero fresh X-Total-Cost-Usd charges among colliders, total < $5 cap,
///   * cross-checks the p0d ReconSource shape (operator file export) when
///     P0E_RECON_IMPORT_PATH is provided; otherwise marks the confirmatory
///     recon PENDING-OPERATOR (provider usage APIs lag same-day).
#[test]
#[ignore = "P0-E real-spend harness: operator opt-in only (P0E_ENABLE=1), never CI"]
fn test_real_charge_count_idem_collision() {
    if std::env::var("P0E_ENABLE").as_deref() != Ok("1") {
        eprintln!(
            "SKIPPED (refusing): P0E_ENABLE=1 not set — this test orchestrates real services \
             and (in live mode) spends real money under a $5 cap. Run: \
             P0E_ENABLE=1 cargo test --test p0e_real_charge -- --ignored"
        );
        return;
    }

    let root = repo_root();
    let script = root.join("test/p0e_real_charge.sh");
    assert!(script.is_file(), "test/p0e_real_charge.sh missing");

    let status = std::process::Command::new("bash")
        .arg(&script)
        .current_dir(&root)
        .status()
        .expect("failed to spawn p0e_real_charge.sh");

    assert!(
        status.success(),
        "p0e_real_charge.sh failed (exit {:?}) — see harness output above for the \
         billed==M reconciliation diagnostics",
        status.code()
    );
}
