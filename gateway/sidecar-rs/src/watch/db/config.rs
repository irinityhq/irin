//! WatchDb configuration knobs, boot-time env resolution, and shared report types.

use rand_core::{OsRng, RngCore};

/// Frozen distinct watch-genesis hash. The digest is stored directly so the
/// original domain-separation value never needs to remain in product source.
/// Changing this digest invalidates every existing per-tenant chain.
pub const WATCH_DISTINCT_GENESIS_HASH: &str =
    "0ffed28740318eb8e9fa37cc3d034394c1eea87a273ecdbfcdcb937af502acba";

/// Returns the distinct genesis hash as a lowercase hex string. Cheap to
/// return; keeping the frozen digest as a literal avoids a lazy static.
pub fn watch_distinct_genesis() -> String {
    WATCH_DISTINCT_GENESIS_HASH.to_owned()
}

#[cfg(test)]
mod genesis_tests {
    use super::{watch_distinct_genesis, WATCH_DISTINCT_GENESIS_HASH};

    #[test]
    fn frozen_watch_genesis_digest_is_stable() {
        assert_eq!(
            WATCH_DISTINCT_GENESIS_HASH,
            "0ffed28740318eb8e9fa37cc3d034394c1eea87a273ecdbfcdcb937af502acba"
        );
        assert_eq!(watch_distinct_genesis(), WATCH_DISTINCT_GENESIS_HASH);
    }
}

/// atomic spend ledger (Q5: UTC day bucket). Derives the calendar UTC day string
/// `'YYYY-MM-DD'` for the spend-ledger key from epoch-millis using pure integer
/// math (civil-from-days, Howard Hinnant's algorithm). chrono is NOT a direct
/// dependency of this crate (the rest of the codebase derives time via
/// `SystemTime::now().duration_since(UNIX_EPOCH)`), so we keep that convention
/// and never touch SQLite localtime/'now' — that would re-introduce TZ drift.
/// All processes call this with their own `Utc::now`-equivalent epoch millis, so
/// cross-process skew on a day boundary is bounded by host-clock skew (documented
/// in the max-loss runbook).
pub fn utc_day_bucket(now_ms: i64) -> String {
    // Days since the Unix epoch (1970-01-01). Floor-divide so pre-epoch millis
    // (recovery saturating path) still map to a sane bucket.
    let days = now_ms.div_euclid(86_400_000);
    // civil_from_days: convert a count of days since 1970-01-01 to (y, m, d).
    // Shift epoch to 0000-03-01 so leap-day lands at the end of the 400y cycle.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", year, m, d)
}

/// atomic spend ledger per-directive max-fanout ceiling. A directive that may
/// non-deterministically fan out (SpecOps/Sheldon blind spot) reserves this
/// worst-case ceiling up front so a single fan-out cannot bust the day cap.
/// Settle still writes the realized truth; reserve-at-ceiling is the safety,
/// settle-at-realized is the truth. Override via env `WATCH_MAX_FANOUT_COST_USD`.
pub const MAX_FANOUT_COST_USD_DEFAULT: f64 = 5.0;

/// Reads the per-directive reservation ceiling, env-overridable. Defaults to
/// `MAX_FANOUT_COST_USD_DEFAULT` (5.0). Negative / unparseable values fall back
/// to the default (never reserve <= 0, which would defeat the cap).
pub fn max_fanout_cost_usd() -> f64 {
    std::env::var("WATCH_MAX_FANOUT_COST_USD")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(MAX_FANOUT_COST_USD_DEFAULT)
}

/// Enqueue backpressure for `pending_escalations` (§7 item 9): the ceiling on
/// NON-TERMINAL rows (`queued`, `claimed`, `council_response_staged`,
/// `failed`) per tenant. Non-terminal rows are deliberately never pruned, so
/// without this cap an env-armed (or DB-write) producer accumulates without
/// bound — storage DoS plus a latent burst of deliberation reserves the
/// moment a real arm lands. At the cap the producer's enqueue is refused as
/// a TRANSIENT error (the CDC sweep stalls loudly and retries next tick;
/// the cursor does not advance, so no escalation is ever dropped).
/// Override via env `WATCH_PENDING_ESCALATIONS_MAX_NONTERMINAL`.
pub const PENDING_ESCALATIONS_MAX_NONTERMINAL_DEFAULT: i64 = 1_000;

/// Reads the non-terminal row ceiling, env-overridable. Zero / negative /
/// unparseable values fall back to the default (a cap of 0 would wedge the
/// producer permanently; fail toward the known-good default, the cap itself
/// still bounds the table).
pub fn pending_escalations_max_nonterminal() -> i64 {
    std::env::var("WATCH_PENDING_ESCALATIONS_MAX_NONTERMINAL")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(PENDING_ESCALATIONS_MAX_NONTERMINAL_DEFAULT)
}

/// p0c/p0d — the UTC calendar-day council spend cap ceiling (USD). Env
/// `DAILY_SPEND_CAP_USD` may only *lower* this value (fail-closed parse;
/// garbage or above-ceiling refuses startup). The boot-resolved value from
/// `daily_spend_cap()` is the single source of truth for both claim-time
/// reserve and p0d's `spend_cap_usd` gauge.
pub const DAILY_SPEND_CAP: f64 = 50.0;

/// Env var for an operator-lowering of the day cap (canary). Unset → ceiling.
pub const DAILY_SPEND_CAP_ENV_VAR: &str = "DAILY_SPEND_CAP_USD";

/// Fail-closed daily-cap config error. Mirrors `socket::SocketConfigError`:
/// malformed or above-ceiling values refuse startup; there is no fallback to
/// a looser cap.
#[derive(Debug, Clone, PartialEq)]
pub enum DailySpendCapError {
    BadValue { raw: String },
    AboveCeiling { value: f64, ceiling: f64 },
    AlreadyInitialized,
}

impl std::fmt::Display for DailySpendCapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DailySpendCapError::BadValue { raw } => write!(
                f,
                "{DAILY_SPEND_CAP_ENV_VAR}={raw:?} is not a valid positive finite USD cap; \
                 refusing to start — env may only LOWER the {DAILY_SPEND_CAP} ceiling, never raise it"
            ),
            DailySpendCapError::AboveCeiling { value, ceiling } => write!(
                f,
                "{DAILY_SPEND_CAP_ENV_VAR}={value} exceeds the signed ceiling {ceiling}; \
                 refusing to start"
            ),
            DailySpendCapError::AlreadyInitialized => {
                write!(f, "daily spend cap already resolved at boot")
            }
        }
    }
}

impl std::error::Error for DailySpendCapError {}

/// Parse a daily spend cap override. Must be finite, > 0, and <= [`DAILY_SPEND_CAP`].
pub fn parse_daily_spend_cap(raw: &str) -> Result<f64, DailySpendCapError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(DailySpendCapError::BadValue {
            raw: raw.to_string(),
        });
    }
    let value = trimmed
        .parse::<f64>()
        .map_err(|_| DailySpendCapError::BadValue {
            raw: raw.to_string(),
        })?;
    if !value.is_finite() || value <= 0.0 {
        return Err(DailySpendCapError::BadValue {
            raw: raw.to_string(),
        });
    }
    // The cap is the source of the
    // attested ceiling, which `derive_arm_content` carries as INTEGER CENTS. A
    // sub-cent cap (`$0.004`) or any fractional-cent value (`$0.005`) rounds to a
    // different number of cents than its USD face value — distinct caps collide
    // to the same i64 bound, and `$0.004` rounds to 0 cents (which the new
    // `active_arm.CHECK(effective_daily_cap_cents >= 1)` would reject only at
    // arm time). Reject it HERE, at config parse, so a non-whole-cent or
    // rounds-to-zero cap fails boot loudly. Pairs with attestation format's float-saturation
    // guard (`derive_arm_content`) and the new STRICT/CHECK table floor.
    let cents = value * 100.0;
    if (cents.round() - cents).abs() > 1e-9 || cents.round() < 1.0 {
        return Err(DailySpendCapError::BadValue {
            raw: raw.to_string(),
        });
    }
    if value > DAILY_SPEND_CAP {
        return Err(DailySpendCapError::AboveCeiling {
            value,
            ceiling: DAILY_SPEND_CAP,
        });
    }
    Ok(value)
}

/// Resolve the day cap from an optional env value. `None` / empty → ceiling.
/// `Some(_)` is parsed strictly (fail-closed). Pure seam for tests + boot.
pub fn daily_spend_cap_from_env(value: Option<&str>) -> Result<f64, DailySpendCapError> {
    match value {
        None => Ok(DAILY_SPEND_CAP),
        Some(raw) if raw.trim().is_empty() => Ok(DAILY_SPEND_CAP),
        Some(raw) => parse_daily_spend_cap(raw),
    }
}

static DAILY_SPEND_CAP_RESOLVED: std::sync::OnceLock<f64> = std::sync::OnceLock::new();

/// Called once at sidecar boot (main.rs). Refuses startup on bad config.
pub fn init_daily_spend_cap_at_boot(value: Option<&str>) -> Result<(), DailySpendCapError> {
    let cap = daily_spend_cap_from_env(value)?;
    DAILY_SPEND_CAP_RESOLVED
        .set(cap)
        .map_err(|_| DailySpendCapError::AlreadyInitialized)
}

/// Boot-resolved UTC-day spend cap. Before `init_daily_spend_cap_at_boot` (unit
/// tests only) falls back to [`DAILY_SPEND_CAP`]; production always inits first.
pub fn daily_spend_cap() -> f64 {
    *DAILY_SPEND_CAP_RESOLVED.get().unwrap_or(&DAILY_SPEND_CAP)
}

// Attested-arm (HIGH finding — removed): there is NO `GW_REQUIRE_ATTESTED_ARM`
// runtime enforcement-disable. A boot-bound OnceLock still let an attacker who
// owns the laptop turn the safety off with one env var + a restart. Enforcement
// is now UNCONDITIONAL: the reserve always requires a signature-re-verified
// active_arm; no row → refuse. The documented rollback path is to redeploy the
// prior binary (the active_arm table is additive; dropping it is safe).

/// Attested-arm (B1) — the attested-arm SPEND-WINDOW lifetime in ms, measured from
/// the SIGNED tap time (`iat_ms`). The reserve refuses once
/// `now >= signed.iat_ms + arm_window_ms_bootlocked()`; the confirm handler
/// stamps the SAME formula into `active_arm.exp_at_ms` so the column and the
/// reserve gate share one source of truth.
///
/// `GW_ARM_WINDOW_MS` is a BOOT-TIME knob (read ONCE at startup; changing it
/// needs a sidecar restart) so an attacker who owns the box cannot extend a
/// live spend window with a runtime env flip — the same bypass class the
/// removed `GW_REQUIRE_ATTESTED_ARM` flag would have opened. Non-positive /
/// unparseable values fall back to the 24h default (one UTC-day-scale window,
/// matching the daily cap horizon).
pub const ARM_WINDOW_MS_DEFAULT: i64 = 24 * 60 * 60 * 1000;

/// HIGH (spend-window split-brain) — the boot-resolved spend window. A LIVE
/// `std::env::var` read on the spend path would let a box-owning attacker set
/// `GW_ARM_WINDOW_MS=<huge>` and extend the window indefinitely. Resolve once.
static ARM_WINDOW_MS_RESOLVED: std::sync::OnceLock<i64> = std::sync::OnceLock::new();

/// Boot-time resolution of `GW_ARM_WINDOW_MS`. Called once from main.rs.
/// Non-positive / unparseable values resolve to [`ARM_WINDOW_MS_DEFAULT`].
/// Idempotent-safe: a second set is ignored (returns Ok) so re-init in tests
/// never panics; the spend path always reads the FIRST resolved value.
pub fn init_arm_window_ms_at_boot() {
    let window = std::env::var("GW_ARM_WINDOW_MS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(ARM_WINDOW_MS_DEFAULT);
    let _ = ARM_WINDOW_MS_RESOLVED.set(window);
}

/// Boot-locked spend window for the SPEND path (reserve + confirm column).
/// Before `init_arm_window_ms_at_boot` (unit tests that never boot) it
/// fail-SAFE-SHORT to [`ARM_WINDOW_MS_DEFAULT`] — never an attacker-controlled
/// live env read. This is the ONLY window the spend gate may use.
pub fn arm_window_ms_bootlocked() -> i64 {
    *ARM_WINDOW_MS_RESOLVED
        .get()
        .unwrap_or(&ARM_WINDOW_MS_DEFAULT)
}

/// Live `GW_ARM_WINDOW_MS` read — RETAINED ONLY for the stage/confirm-CEREMONY
/// horizon, NEVER the spend window. Non-positive / unparseable values fall back
/// to the default. The SPEND gate MUST use [`arm_window_ms_bootlocked`].
pub fn arm_window_ms() -> i64 {
    std::env::var("GW_ARM_WINDOW_MS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(ARM_WINDOW_MS_DEFAULT)
}

/// Attested-arm (invariant) — named rollback flag
/// `GW_ARM_SIGNED_WINDOW`, DEFAULT-ON. When on (the closure), the spend gate
/// reads the window from the SIGNED challenge (`signed.spend_window_ms`), so a
/// post-tap `GW_ARM_WINDOW_MS` restart cannot extend a genuine tap's horizon.
/// When explicitly `false`/`0`, the gate reverts to the legacy
/// [`arm_window_ms_bootlocked`] path WITHOUT a redeploy — the named rollback for
/// a JCS/signing regression. Boot-resolved once (like the window itself) so the
/// confirm stamp and the reserve gate always agree within a boot.
static SIGNED_SPEND_WINDOW_RESOLVED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Boot-time resolution of `GW_ARM_SIGNED_WINDOW`. Called once from main.rs.
/// Absent / unparseable ⇒ ON (default-on). Idempotent-safe.
pub fn init_signed_spend_window_at_boot() {
    let on = std::env::var("GW_ARM_SIGNED_WINDOW")
        .ok()
        .map(|v| !(v == "false" || v == "0"))
        .unwrap_or(true);
    let _ = SIGNED_SPEND_WINDOW_RESOLVED.set(on);
}

/// Whether the spend gate reads the SIGNED window (default-on). Before boot
/// resolution (unit tests) it defaults ON — the secure path — so a test that
/// never boots still exercises the signed-window gate.
pub fn signed_spend_window_enabled() -> bool {
    *SIGNED_SPEND_WINDOW_RESOLVED.get().unwrap_or(&true)
}

/// lease liveness — deliberation lease duration default (the historical fixed
/// 150s lease per invariant 14c4d63c-900). Env-overridable
/// via `WATCH_LEASE_DURATION_MS` so tests can compress time without touching
/// prod values; non-positive / unparseable values fall back to the default.
pub const LEASE_DURATION_MS_DEFAULT: i64 = 150_000;

/// lease liveness — renewal interval default: LEASE/3 = 50s (standard K8s Lease
/// ratio; survives 2 missed renewals before the lease expires).
pub const LEASE_RENEW_INTERVAL_MS_DEFAULT: i64 = 50_000;

/// Reads the deliberation lease duration (`WATCH_LEASE_DURATION_MS`,
/// default 150_000). Used by both the claim tx and the renewal driver.
pub fn lease_duration_ms() -> i64 {
    std::env::var("WATCH_LEASE_DURATION_MS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(LEASE_DURATION_MS_DEFAULT)
}

/// Reads the lease renewal interval (`WATCH_LEASE_RENEW_MS`). When unset,
/// defaults to lease/3 (preserving the K8s 1/3 ratio even when only the lease
/// is overridden), bottoming out at 1ms.
pub fn lease_renew_interval_ms() -> i64 {
    std::env::var("WATCH_LEASE_RENEW_MS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or_else(|| (lease_duration_ms() / 3).max(1))
}

/// single-writer (single-writer invariant) — staleness window for the singleton
/// writer claim: a holder whose `heartbeat_at_ms` is older than
/// `now - WRITER_CLAIM_STALE_MS` is presumed crashed and can be taken
/// over. MUST stay > 3x the heartbeat interval (default 90s vs 30s) so a
/// live writer survives two missed heartbeats before losing the claim
/// (same ratio as the deliberation lease above).
pub const WRITER_CLAIM_STALE_MS_DEFAULT: i64 = 90_000;

/// Reads the writer-claim staleness window (`WRITER_CLAIM_STALE_MS`,
/// default 90_000). Non-positive / unparseable values fall back to the
/// default (a zero/negative window would let any second writer "take
/// over" a live holder, defeating single-writer).
pub fn writer_claim_stale_ms() -> i64 {
    std::env::var("WRITER_CLAIM_STALE_MS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(WRITER_CLAIM_STALE_MS_DEFAULT)
}

/// single-writer — writer-claim heartbeat cadence default (30s; 1/3 of
/// the stale window, K8s Lease ratio).
pub const WRITER_CLAIM_HEARTBEAT_MS_DEFAULT: u64 = 30_000;

/// Reads the writer-claim heartbeat cadence (`WRITER_CLAIM_HEARTBEAT_MS`,
/// default 30_000). Non-positive / unparseable values fall back.
pub fn writer_claim_heartbeat_ms() -> u64 {
    std::env::var("WRITER_CLAIM_HEARTBEAT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(WRITER_CLAIM_HEARTBEAT_MS_DEFAULT)
}

/// single-writer — this process's writer identity. Generated once at
/// first use (OsRng, same source as the claim fencing token) and stable
/// for the process lifetime: every arm / producer-spawn / heartbeat in
/// this sidecar instance claims under the same uuid, so a re-arm by the
/// same instance re-acquires its own claim while a SECOND instance
/// (different uuid) is refused.
pub fn process_instance_uuid() -> &'static str {
    static INSTANCE_UUID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE_UUID.get_or_init(|| {
        let mut rng = OsRng;
        let mut bytes = [0u8; 16];
        rng.fill_bytes(&mut bytes);
        hex::encode(bytes)
    })
}

/// lease liveness — outcome of a deliberation-lease renewal attempt.
/// `Lost` means the WHERE predicate matched 0 rows: the claim_token was
/// superseded (competing reclaim) or the status moved on. Renewal NEVER
/// extends a lease the holder no longer owns — claim_token fencing is the
/// safety, mirroring `heartbeat_outbox`'s claim_handle check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewOutcome {
    Renewed { claimed_until_ms: i64 },
    Lost,
}

/// lease liveness — report from one phantom-claim sweep. `swept` is every
/// expired-lease 'claimed' row flipped back to 'failed'; `in_flight_expired`
/// is the subset that was a REAL in-flight claim (non-null claim_token and
/// attempts > 0, i.e. a dispatcher actually held it), which feeds the
/// `lease_expired_during_deliberation` counter (telemetry invariant telemetry).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhantomSweepReport {
    pub swept: usize,
    pub in_flight_expired: usize,
}

/// watch telemetry (telemetry invariant) — outcome of `store_council_response_and_stage`.
/// `dup_realized_cost` is the idempotency-dedup MISS detector: true when the
/// settle landed on a (tenant, id) row that ALREADY carried a
/// realized_cost_usd from a previous settle — i.e. a realized cost was
/// written twice for the same escalation. The OCC claim_token fence makes
/// this impossible in normal operation (a settled row is never 'claimed'
/// again), so any true here is an invariant breach. Route through
/// `dispatcher::note_settle_report` to bump the dup-charge alarm.
///
/// /// * `settled_at_estimate_usd` — `Some(est)` when the `x-total-cost-usd`
///   header was missing/unparseable/non-finite/negative/>= ceiling and the
///   ledger was settled FAIL-CLOSED at the stamped reservation estimate
///   instead of the old `0.0` (which made the day cap fail OPEN under a
///   single upstream header drift).
/// * `ceiling_overshoot_usd` — `Some(realized - est)` when a VALID realized
///   cost exceeded the per-directive reservation ceiling (settle-at-realized
///   is still the truth; the overshoot is flagged so the p0d alarm path can
///   page instead of silently absorbing it).
///
/// `#[must_use]`: dropping this report silently skips the dup-charge /
/// overshoot alarms — every caller must route it through
/// `dispatcher::note_settle_report` (or explicitly assert on it in tests).
#[must_use = "route through dispatcher::note_settle_report — dropping it silently skips the dup-charge/overshoot alarms"]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SettleReport {
    pub dup_realized_cost: bool,
    pub settled_at_estimate_usd: Option<f64>,
    pub ceiling_overshoot_usd: Option<f64>,
}

/// watch telemetry (telemetry invariant) — one out-of-band recon divergence alarm
/// row (`recon_alarm` table). Carries BOTH sides of the cross-check: the
/// local ledger's settled truth and the external billing source's number.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconAlarmRow {
    pub id: i64,
    pub at_ms: i64,
    pub day_bucket: String,
    pub local_usd: f64,
    pub external_usd: f64,
    pub divergence_usd: f64,
    pub source: String,
}

#[cfg(test)]
mod daily_spend_cap_tests {
    use super::{
        daily_spend_cap_from_env, parse_daily_spend_cap, DailySpendCapError, DAILY_SPEND_CAP,
    };

    #[test]
    fn cap_env_unset_uses_ceiling() {
        assert_eq!(daily_spend_cap_from_env(None).unwrap(), DAILY_SPEND_CAP);
        assert_eq!(daily_spend_cap_from_env(Some("")).unwrap(), DAILY_SPEND_CAP);
        assert_eq!(
            daily_spend_cap_from_env(Some("  ")).unwrap(),
            DAILY_SPEND_CAP
        );
    }

    #[test]
    fn cap_env_lowers_when_valid() {
        assert_eq!(daily_spend_cap_from_env(Some("25")).unwrap(), 25.0);
        assert_eq!(daily_spend_cap_from_env(Some("2.5")).unwrap(), 2.5);
    }

    #[test]
    fn cap_env_above_const_refuses() {
        let err = daily_spend_cap_from_env(Some("50.01")).unwrap_err();
        assert!(matches!(
            err,
            DailySpendCapError::AboveCeiling {
                value: 50.01,
                ceiling: 50.0
            }
        ));
        let err = parse_daily_spend_cap("100").unwrap_err();
        assert!(matches!(err, DailySpendCapError::AboveCeiling { .. }));
    }

    #[test]
    fn cap_env_garbage_refuses() {
        for bad in ["2O", "not-a-number", "-1", "0", "NaN", "inf"] {
            let err = daily_spend_cap_from_env(Some(bad)).unwrap_err();
            assert!(
                matches!(err, DailySpendCapError::BadValue { .. }),
                "expected BadValue for {bad:?}, got {err}"
            );
        }
    }

    /// Attested-arm cap-safety: a sub-cent or fractional-cent cap collides distinct
    /// USD values onto the same i64-cents bound (or rounds to 0), so it is
    /// rejected at parse — pairs with the new `active_arm.CHECK(cap_cents >= 1)`.
    #[test]
    fn cap_sub_cent_and_fractional_cent_refuse() {
        // Rounds to 0 cents.
        for sub in ["0.004", "0.001", "0.0049"] {
            let err = parse_daily_spend_cap(sub).unwrap_err();
            assert!(
                matches!(err, DailySpendCapError::BadValue { .. }),
                "expected BadValue for sub-cent {sub:?}, got {err}"
            );
        }
        // Fractional cents (a half-cent) — not a whole number of cents.
        for frac in ["0.005", "1.005", "0.015"] {
            let err = parse_daily_spend_cap(frac).unwrap_err();
            assert!(
                matches!(err, DailySpendCapError::BadValue { .. }),
                "expected BadValue for fractional-cent {frac:?}, got {err}"
            );
        }
        // Whole-cent values still bind.
        assert_eq!(parse_daily_spend_cap("0.01").unwrap(), 0.01);
        assert_eq!(parse_daily_spend_cap("2.50").unwrap(), 2.5);
        assert_eq!(parse_daily_spend_cap("50").unwrap(), 50.0);
    }
}
