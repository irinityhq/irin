use crate::keymgmt::DirectiveVerifier;
use crate::watch::db::WatchDb;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct WatchWorkerConfig {
    pub enabled: bool,
    pub tick_interval_ms: u64,
    pub max_claims_per_tick: u32,
    pub lease_duration_ms: i64,
    pub tenant_scope: String,
}

pub fn live_worker_config_from_env() -> WatchWorkerConfig {
    let vars: std::collections::HashMap<String, String> = std::env::vars().collect();
    live_worker_config_from_vars(vars)
}

pub fn live_worker_config_from_vars(
    vars: std::collections::HashMap<String, String>,
) -> WatchWorkerConfig {
    let get = |key: &str| vars.get(key).map(|s| s.trim().to_string());

    let enabled = get("WATCH_WORKER_ENABLED")
        .map(|v| {
            let v = v.to_lowercase();
            v == "true" || v == "1" || v == "yes"
        })
        .unwrap_or(false);

    let tick_interval_ms = get("WATCH_WORKER_TICK_INTERVAL_MS")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1000);

    let max_claims_per_tick = get("WATCH_WORKER_MAX_CLAIMS_PER_TICK")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(10);

    let lease_duration_ms = get("WATCH_WORKER_LEASE_DURATION_MS")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(30_000);

    let tenant_scope = get("WATCH_WORKER_TENANT_SCOPE").unwrap_or_else(|| "default".to_string());

    WatchWorkerConfig {
        enabled,
        tick_interval_ms,
        max_claims_per_tick,
        lease_duration_ms,
        tenant_scope,
    }
}

pub fn should_spawn_live_worker(config: &WatchWorkerConfig) -> bool {
    config.enabled
}

#[derive(Debug, Default)]
pub struct WorkerTickReport {
    pub claimed_count: u32,
    pub executed_count: u32,
    pub failed_count: u32,
    pub idle: bool,
}

pub async fn run_worker_tick(
    db: &WatchDb,
    config: &WatchWorkerConfig,
    verifier: Option<&DirectiveVerifier>,
) -> anyhow::Result<WorkerTickReport> {
    let mut report = WorkerTickReport::default();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let claims = db
        .claim_outbox(
            &config.tenant_scope,
            config.max_claims_per_tick,
            now_ms,
            config.lease_duration_ms,
        )
        .await?;

    if claims.is_empty() {
        report.idle = true;
        return Ok(report);
    }

    report.claimed_count = claims.len() as u32;

    // Pre-seal W2: fail CLOSED if the worker has no pinned Council verifier.
    // Without it we cannot prove a claimed row was actually authorized by
    // Council, so we refuse to act on anything this tick and nack every claim
    // back to staged. (Production always supplies one — see
    // spawn_live_worker_loop; absence here means a boot/wiring fault.)
    let verifier = match verifier {
        Some(v) => v,
        None => {
            for claim in claims {
                let claim_handle = claim
                    .worker_provenance
                    .as_ref()
                    .and_then(|g| g.opaque_handle.as_deref())
                    .unwrap_or_default();
                tracing::error!(
                    tenant = %claim.tenant,
                    id = %claim.id,
                    "worker has no pinned Council verifier — refusing to act (fail closed)"
                );
                crate::watch::dispatcher::bump_directive_verify_failed();
                let reason =
                    serde_json::to_string(&sovereign_protocol::types::ProblemDetails::new(
                        "no-directive-verifier",
                        "worker has no pinned Council verifier; refusing to execute (fail closed)",
                    ))
                    .unwrap_or_else(|_| "no directive verifier".to_string());
                db.nack_outbox(&claim.tenant, &claim.id, claim_handle, &reason)
                    .await?;
                report.failed_count += 1;
            }
            return Ok(report);
        }
    };

    for claim in claims {
        let claim_handle = claim
            .worker_provenance
            .as_ref()
            .and_then(|g| g.opaque_handle.as_deref())
            .unwrap_or_default();

        // ── Pre-seal W2 gate 1: cryptographic provenance ──────────────────
        // Before ANY action, verify the Ed25519 signature on the stored
        // canonical envelope against the pinned Council key + kid. Verify the
        // SAME bytes that were signed (envelope_json_canonical verbatim — no
        // re-canonicalization). Any failure → nack, loud error, never execute.
        if let Err(e) = verifier.verify(
            &claim.envelope_json_canonical,
            &claim.signature_b64,
            &claim.signing_kid,
        ) {
            tracing::error!(
                tenant = %claim.tenant,
                id = %claim.id,
                stored_kid = %claim.signing_kid,
                pinned_kid = %verifier.pinned_kid(),
                error = %e,
                "directive envelope failed Ed25519 verification — refusing to execute (forged/tampered/unpinned)"
            );
            crate::watch::dispatcher::bump_directive_verify_failed();
            let reason = serde_json::to_string(&sovereign_protocol::types::ProblemDetails::new(
                "directive-verification-failed",
                format!("directive envelope verification failed: {}", e),
            ))
            .unwrap_or_else(|_| "directive verification failed".to_string());
            db.nack_outbox(&claim.tenant, &claim.id, claim_handle, &reason)
                .await?;
            report.failed_count += 1;
            continue;
        }

        // The signature is valid, so the canonical envelope is authentic. From
        // here, derive the authority from the VERIFIED envelope (not from the
        // possibly-untrusted persisted `authority` column). This is the
        // authority-integrity property: capability gating keys off what Council
        // actually signed.
        //
        // ── Pre-seal W2 P1-B: canonical-parse fail-closed ─────────────────
        // The signature verified over these exact bytes, so in practice they
        // parse (the same-process signer always emits valid JSON). But a silent
        // `{}` fallback would let a future schema/encoder drift downgrade an
        // unparseable envelope into a default-authority stub execution — a live
        // bypass one drift away, and it contradicts "no silent pass". Parse
        // failure => refuse, never execute.
        let verified_envelope: serde_json::Value = match serde_json::from_str(
            &claim.envelope_json_canonical,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    tenant = %claim.tenant,
                    id = %claim.id,
                    error = %e,
                    "verified canonical envelope failed to parse — refusing to execute (no silent downgrade)"
                );
                crate::watch::dispatcher::bump_directive_verify_failed();
                let reason =
                    serde_json::to_string(&sovereign_protocol::types::ProblemDetails::new(
                        "directive-canonical-unparseable",
                        format!("verified canonical envelope failed to parse: {}", e),
                    ))
                    .unwrap_or_else(|_| "canonical unparseable".to_string());
                // fail-closed nack: never executes; TTL reaper bounds the loop. Terminal
                // 'dead_lettered' upgrade tracked as task #24 (needs directive_outbox CHECK migration).
                db.nack_outbox(&claim.tenant, &claim.id, claim_handle, &reason)
                    .await?;
                report.failed_count += 1;
                continue;
            }
        };

        // ── Pre-seal W2 P1-B: authority whitelist (no silent downgrade) ───
        // The authority MUST be one of the three known values. Anything else
        // (missing, null, unknown, wrong type) is NOT silently treated as
        // "recommend" (which would skip the captoken gate and execute) — it is
        // refused. Only an explicit, signed, recognized authority proceeds.
        let verified_authority = match verified_envelope.get("authority").and_then(|v| v.as_str()) {
            Some(a @ ("recommend" | "prepare" | "execute")) => a.to_string(),
            other => {
                tracing::error!(
                    tenant = %claim.tenant,
                    id = %claim.id,
                    authority = ?other,
                    "verified envelope authority missing/unknown — refusing to execute (no silent downgrade to recommend)"
                );
                crate::watch::dispatcher::bump_directive_verify_failed();
                let reason = serde_json::to_string(
                    &sovereign_protocol::types::ProblemDetails::new(
                        "directive-bad-authority",
                        format!(
                            "verified envelope authority not in {{recommend,prepare,execute}}: {:?}",
                            other
                        ),
                    ),
                )
                .unwrap_or_else(|_| "bad authority".to_string());
                // fail-closed nack: never executes; TTL reaper bounds the loop. Terminal
                // 'dead_lettered' upgrade tracked as task #24 (needs directive_outbox CHECK migration).
                db.nack_outbox(&claim.tenant, &claim.id, claim_handle, &reason)
                    .await?;
                report.failed_count += 1;
                continue;
            }
        };

        // ── Pre-seal W2 gate 2: capability tokens (fail closed, option a) ──
        // For elevated authorities (prepare/execute) a valid capability token
        // is REQUIRED. Missing or invalid → refuse (nack), never proceed.
        let mut valid_authority = true;
        let mut error_reason = String::new();

        if verified_authority == "prepare" || verified_authority == "execute" {
            // Extract the capability token from the VERIFIED canonical envelope
            // (the signed bytes), not the unsigned envelope_json — the token is
            // part of what Council signed.
            let token_opt = verified_envelope
                .get("capability_token")
                .and_then(|v| v.as_str());

            match token_opt {
                Some(token) => {
                    let is_valid = db
                        .is_capability_token_valid(&claim.tenant, token, &verified_authority)
                        .await?;

                    if !is_valid {
                        valid_authority = false;
                        error_reason =
                            serde_json::to_string(&sovereign_protocol::types::ProblemDetails::new(
                                "invalid-capability-token",
                                format!(
                                    "invalid capability_token for authority {}",
                                    verified_authority
                                ),
                            ))
                            .unwrap_or_else(|_| "invalid capability_token".to_string());
                    }
                }
                None => {
                    valid_authority = false;
                    error_reason =
                        serde_json::to_string(&sovereign_protocol::types::ProblemDetails::new(
                            "missing-capability-token",
                            format!(
                                "missing capability_token for authority {}",
                                verified_authority
                            ),
                        ))
                        .unwrap_or_else(|_| "missing capability_token".to_string());
                }
            }
        }

        if !valid_authority {
            tracing::warn!(
                tenant = %claim.tenant,
                id = %claim.id,
                authority = %verified_authority,
                "worker execution blocked by authority/capability check (fail closed)"
            );
            db.nack_outbox(&claim.tenant, &claim.id, claim_handle, &error_reason)
                .await?;
            report.failed_count += 1;
            continue;
        }

        // Stub execution — operate on the verified canonical envelope.
        let envelope_val = &verified_envelope;

        let worker_result = serde_json::json!({
            "status": "completed",
            "extracted_data": null
        });

        let worker_metrics = serde_json::json!({
            "execution_ms": 42,
            "tokens_used": 0,
            "cost": 0.0,
            "job": envelope_val.get("job"),
            "scope": envelope_val.get("scope")
        });

        tracing::info!(
            tenant = %claim.tenant,
            id = %claim.id,
            job = ?envelope_val.get("job"),
            scope = ?envelope_val.get("scope"),
            stop_condition = ?envelope_val.get("stop_condition"),
            return_expectation = ?envelope_val.get("return_expectation"),
            worker_result = ?worker_result,
            worker_metrics = ?worker_metrics,
            "worker execution stub (success path)"
        );

        // After all verification gates, emit VerifiedExact provenance (JCS on storage) so ack path returns it.
        // claim_handle kept for the lease check; full guard written to worker_provenance col.
        let verified_provenance = sovereign_protocol::types::WorkerProvenanceGuard {
            status: sovereign_protocol::types::WorkerProvenanceStatus::VerifiedExact,
            fabrication_guard: true,
            opaque_handle: Some(claim_handle.to_string()),
        };
        db.worker_ack_outbox(&claim.tenant, &claim.id, claim_handle, verified_provenance)
            .await?;
        report.executed_count += 1;
    }

    Ok(report)
}

pub fn spawn_live_worker_loop(
    db: WatchDb,
    config: WatchWorkerConfig,
) -> Option<(
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
)> {
    if !config.enabled {
        return None;
    }

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let interval = config.tick_interval_ms;

    let handle = tokio::spawn(async move {
        tracing::info!("live worker loop started (enabled=true)");

        // Pre-seal W2: pin the Council verifier from the globally published
        // Directive signing key (set during boot before this loop spawns).
        // If it is absent, every tick fails closed (run_worker_tick nacks all
        // claims) — we still loop so the loud per-tick error surfaces the fault.
        let verifier =
            crate::keymgmt::try_directive_signing_key().map(DirectiveVerifier::from_signing_key);
        if verifier.is_none() {
            tracing::error!(
                "live worker loop has no pinned Council verifier (DirectiveSigningKey not initialized) — every tick will fail closed"
            );
        }

        loop {
            if shutdown_rx.try_recv().is_ok() {
                tracing::info!("live worker loop received shutdown");
                break;
            }

            match run_worker_tick(&db, &config, verifier.as_ref()).await {
                Ok(report) => {
                    if report.idle {
                        tracing::debug!("live worker tick: idle");
                    } else {
                        tracing::info!(
                            claimed = report.claimed_count,
                            executed = report.executed_count,
                            failed = report.failed_count,
                            "live worker tick completed"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "live worker tick error");
                }
            }

            tokio::select! {
                _ = &mut shutdown_rx => {
                    tracing::info!("live worker loop shutdown during sleep");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(interval)) => {}
            }
        }

        tracing::info!("live worker loop stopped");
    });

    Some((handle, shutdown_tx))
}
