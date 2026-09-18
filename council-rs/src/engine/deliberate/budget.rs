//! Budget integration: the BATS budget-signal fetch and the shared
//! round-boundary pause gate used by the CLI engine and the War Room stream.

/// BATS Wedge 1: Lightweight budget tracker injection.
/// Fetches real-time daily remaining from hermes-budget-guard.sh (respects caps, no bypass).
/// Returns (formatted_signal, tier). On guard miss, timeout, or failure: empty
/// signal + UNKNOWN (D-06 omit-signal path).
pub async fn fetch_budget_signal(
    profile: Option<&str>,
    _task_id: Option<&str>,
) -> (String, String) {
    let profile = profile.unwrap_or("default");
    // Overridable for non-default installs; an absent or failing guard emits no signal.
    let guard = std::env::var("HERMES_BUDGET_GUARD_SCRIPT").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.hermes/scripts/hermes-budget-guard.sh")
    });

    let child = match tokio::process::Command::new(&guard)
        .arg("--query-remaining")
        .arg(profile)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return (String::new(), "UNKNOWN".to_string()),
    };

    // Bounded wait so a hung guard cannot stall the async worker (B-23).
    // On timeout the Child drops with kill_on_drop(true) and is reaped.
    const GUARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    let out = match tokio::time::timeout(GUARD_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        _ => return (String::new(), "UNKNOWN".to_string()),
    };

    let (remaining, spent, cap, pct) = if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout);
        let mut rem = 7.0f64;
        let mut sp = 0.0f64;
        let mut c = 7.0f64;
        let mut p = 0i32;
        for line in s.lines() {
            if let Some(v) = line.strip_prefix("REMAINING_USD=") {
                rem = v.trim().parse().unwrap_or(7.0);
            } else if let Some(v) = line.strip_prefix("SPENT_USD=") {
                sp = v.trim().parse().unwrap_or(0.0);
            } else if let Some(v) = line.strip_prefix("CAP_USD=") {
                c = v.trim().parse().unwrap_or(7.0);
            } else if let Some(v) = line.strip_prefix("PERCENT_USED=") {
                p = v.trim().parse().unwrap_or(0);
            }
        }
        (rem, sp, c, p)
    } else {
        return (String::new(), "UNKNOWN".to_string());
    };

    let tier = if pct >= 90 || remaining < 0.5 {
        "CRITICAL"
    } else if pct > 70 || remaining < 2.1 {
        "LOW"
    } else if pct > 40 {
        "MEDIUM"
    } else {
        "HIGH"
    };

    let adapt = match tier {
        "HIGH" => "full exploration, classic profile, normal verbosity/rounds",
        "MEDIUM" => "standard depth and rounds",
        "LOW" => "lean budget/profile, reduce rounds/verbosity, bias concise high-confidence",
        _ => "minimal (critical budget or direct), short responses only, high-confidence paths",
    };

    let signal = format!(
        "**BUDGET SIGNAL (BATS Wedge 1):** Daily remaining: ${:.2} ({}% of ${:.2} cap). Spent today: ${:.2}. Tier: {}. Adapt: {}. Do not exceed budget. Task-aware: prioritize efficiency.",
        remaining, pct, cap, spent, tier, adapt
    );

    (signal, tier.to_string())
}

/// Shared budget gate for CLI engine and War Room stream (v9.12.0).
///
/// Pauses when running cost has reached the cap before all planned rounds finish.
pub fn should_pause_for_budget(
    budget_max_usd: Option<f64>,
    total_cost: f64,
    round_num: u32,
    rounds_planned: u32,
) -> bool {
    budget_max_usd.is_some_and(|max| total_cost >= max && round_num < rounds_planned)
}

#[cfg(test)]
mod budget_tests {
    use super::should_pause_for_budget;

    #[test]
    fn pauses_when_cost_at_cap_before_last_round() {
        assert!(should_pause_for_budget(Some(1.0), 1.0, 1, 3));
        assert!(!should_pause_for_budget(Some(1.0), 1.0, 3, 3));
        assert!(!should_pause_for_budget(None, 99.0, 1, 3));
    }
}
