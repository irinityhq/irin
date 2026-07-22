//! `sentinels.yaml` loader and boot validation.
//!
//! Reads `SENTINELS_CONFIG_PATH` (or any path), parses a YAML array of
//! sentinel entries, validates `tier ∈ {fast,polling,deep}` + `cooldown_ms > 0`,
//! dispatches on `name` to construct a supported Sentinel, and runs
//! sentinel-specific fail-fast checks.
//!
//! Library-crate clean: takes a `&Path`, returns `Vec<Arc<dyn Sentinel>>`. The
//! main.rs wrapper layers env-var lookup + `process::exit(1)` on error per
//! the §6.2 boot-validation contract.
//!
//! YAML shape (spec §6.2):
//! ```yaml
//! - name: file-inbox-watch
//!   tenant: sovereign
//!   tier: polling
//!   cooldown_ms: 5000
//!   config:
//!     path: /var/lib/gateway/inbox
//!     patterns: ["*.pdf", "*.md", "*.txt"]
//!     debounce_ms: 500
//! ```
//!
//! Unknown top-level fields are tolerated so a configuration can carry
//! runtime policy fields owned by other layers.

use crate::watch::sentinels::anomaly::{AnomalyConfig, AnomalySentinel};
use crate::watch::sentinels::completion_verify::CompletionVerifySentinel;
use crate::watch::sentinels::file_inbox::FileInboxSentinel;
use crate::watch::sentinels::ledger_delta::LedgerDeltaSentinel;
use crate::watch::sentinels::precedent_integrity::PrecedentIntegritySentinel;
use crate::watch::sentinels::queue_depth::QueueDepthSentinel;
use crate::watch::sentinels::silence::SilenceSentinel;
use crate::watch::sentinels::watch_health::WatchHealthSentinel;
use crate::watch::{Sentinel, Tier};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;

const KNOWN_SENTINELS: &[&str] = &[
    "file-inbox-watch",
    "silence-watch",
    "gateway-active-watch",
    "watch-health-watch",
    "ledger-delta-watch",
    "anomaly-watch",
    "completion-verify-watch",
    "precedent-integrity-watch",
];

#[derive(Debug, Deserialize)]
struct SentinelConfig {
    name: String,
    tenant: String,
    tier: Tier,
    cooldown_ms: u64,
    #[serde(default)]
    config: serde_yaml::Value,
}

/// What the registry returns per yaml entry: the runtime Sentinel and the
/// raw `config:` blob (preserved as JSON for `/watch/list` display).
pub struct LoadedSentinel {
    pub sentinel: Arc<dyn Sentinel>,
    pub config_json: serde_json::Value,
}

pub struct SentinelRegistry;

impl SentinelRegistry {
    pub fn load_from_yaml(path: &Path) -> Result<Vec<LoadedSentinel>> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read sentinels.yaml at {}", path.display()))?;
        let configs: Vec<SentinelConfig> = serde_yaml::from_str(&raw)
            .with_context(|| format!("parse sentinels.yaml at {}", path.display()))?;
        let mut out: Vec<LoadedSentinel> = Vec::with_capacity(configs.len());
        for cfg in configs {
            if cfg.cooldown_ms == 0 {
                bail!("sentinel '{}': cooldown_ms must be > 0 (got 0)", cfg.name);
            }
            let declared_tier = cfg.tier;
            // Capture the raw yaml `config:` as JSON before the typed
            // dispatch consumes it. Goes to /watch/list so operators can
            // verify "the path I wrote in yaml is the path that loaded."
            let config_json = serde_json::to_value(&cfg.config).unwrap_or(serde_json::Value::Null);
            let sentinel = build_one(cfg)?;
            if sentinel.tier() != declared_tier {
                bail!(
                    "sentinel '{}': declared tier {:?} does not match implementation tier {:?}",
                    sentinel.name(),
                    declared_tier,
                    sentinel.tier()
                );
            }
            out.push(LoadedSentinel {
                sentinel,
                config_json,
            });
        }
        Ok(out)
    }
}

fn build_one(cfg: SentinelConfig) -> Result<Arc<dyn Sentinel>> {
    let cooldown = Duration::from_millis(cfg.cooldown_ms);
    match cfg.name.as_str() {
        "file-inbox-watch" => build_file_inbox(cfg, cooldown),
        "silence-watch" => build_silence(cfg, cooldown),
        "gateway-active-watch" => build_queue_depth(cfg, cooldown),
        "watch-health-watch" => build_watch_health(cfg, cooldown),
        "ledger-delta-watch" => build_ledger_delta(cfg, cooldown),
        "anomaly-watch" => build_anomaly(cfg, cooldown),
        "completion-verify-watch" => build_completion_verify(cfg, cooldown),
        "precedent-integrity-watch" => build_precedent_integrity(cfg, cooldown),
        other => bail!(
            "unknown sentinel name '{other}' (known: {})",
            KNOWN_SENTINELS.join(", ")
        ),
    }
}

#[derive(Debug, Deserialize)]
struct FileInboxCfg {
    path: PathBuf,
    patterns: Vec<String>,
    #[serde(default = "default_debounce_ms")]
    debounce_ms: u64,
}
fn default_debounce_ms() -> u64 {
    500
}

fn build_file_inbox(cfg: SentinelConfig, cooldown: Duration) -> Result<Arc<dyn Sentinel>> {
    let fc: FileInboxCfg = serde_yaml::from_value(cfg.config)
        .with_context(|| format!("sentinel '{}': parsing file-inbox config", cfg.name))?;
    let s = FileInboxSentinel::new(
        &cfg.name,
        &cfg.tenant,
        &fc.path,
        fc.patterns,
        Duration::from_millis(fc.debounce_ms),
    )
    .with_cooldown(cooldown);
    s.validate_path()
        .with_context(|| format!("sentinel '{}': validate_path", cfg.name))?;

    // Minimal activation fix (D9 blocker): start the internal PollWatcher for
    // polling-tier file-inbox sentinels so that last_path is populated on
    // real Create events and observe()/interesting()/escalate can fire live.
    // Conditional/safe: only Polling (all file-inbox are today) + after
    // validate_path() (guarantees valid path). A start failure is logged loudly
    // below at the start layer; observe()'s `alive` flag separately distinguishes
    // a healthy idle inbox ("no file yet") from a dead/never-started debouncer
    // ("never started"), so a failed start still quarantines at runtime.
    // The returned PollWatcher is leaked (std::mem::forget below) so its poll
    // thread survives for the process lifetime; dropping it would stop watching.
    if cfg.tier == Tier::Polling {
        // A start failure (permissions, path change post-validate) leaves the
        // debouncer un-spawned, so the sentinel's `alive` flag stays false:
        // observe() reports it dead (Fatal) and the runner quarantines it
        // (liveness regression). This warn is the EARLY, start-layer signal of the same
        // condition — emitted here so the cause is visible before the runtime
        // quarantine, rather than only inferable from a quarantined sentinel.
        //
        // v0.2: pass the current handle (if on watch-rt) so the async debouncer
        // for file_inbox can be spawned on the isolated runtime (fixes the
        // previous unbounded std::thread::spawn + blocking sleep).
        let handle = Handle::try_current().ok();
        match s.start_watching(handle.as_ref()) {
            Ok(watcher) => std::mem::forget(watcher),
            Err(e) => tracing::warn!(
                sentinel = %cfg.name,
                tenant = %cfg.tenant,
                error = %e,
                "watch::registry: file-inbox PollWatcher failed to start — sentinel \
                 registered but its debouncer is dead; observe() will report it \
                 Fatal and the runner will quarantine it"
            ),
        }
    }

    Ok(Arc::new(s))
}

#[derive(Debug, Deserialize)]
struct SilenceCfg {
    threshold_hours: i64,
    ledger_db_path: PathBuf,
    backlog_path: PathBuf,
}

fn build_silence(cfg: SentinelConfig, cooldown: Duration) -> Result<Arc<dyn Sentinel>> {
    let sc: SilenceCfg = serde_yaml::from_value(cfg.config)
        .with_context(|| format!("sentinel '{}': parsing silence config", cfg.name))?;
    let s = SilenceSentinel::new(
        &cfg.name,
        &cfg.tenant,
        sc.threshold_hours,
        &sc.ledger_db_path,
        &sc.backlog_path,
    )
    .with_cooldown(cooldown);
    Ok(Arc::new(s))
}

#[derive(Debug, Deserialize)]
struct QueueDepthCfg {
    url: String,
    jsonpath: String,
    threshold: i64,
}

fn build_queue_depth(cfg: SentinelConfig, cooldown: Duration) -> Result<Arc<dyn Sentinel>> {
    let qc: QueueDepthCfg = serde_yaml::from_value(cfg.config)
        .with_context(|| format!("sentinel '{}': parsing queue-depth config", cfg.name))?;
    let s = QueueDepthSentinel::new(&cfg.name, &cfg.tenant, &qc.url, &qc.jsonpath, qc.threshold)
        .with_cooldown(cooldown);
    Ok(Arc::new(s))
}

#[derive(Debug, Deserialize)]
struct WatchHealthCfg {
    watch_db_path: PathBuf,
}

fn build_watch_health(cfg: SentinelConfig, cooldown: Duration) -> Result<Arc<dyn Sentinel>> {
    let wc: WatchHealthCfg = serde_yaml::from_value(cfg.config)
        .with_context(|| format!("sentinel '{}': parsing watch-health config", cfg.name))?;
    let s =
        WatchHealthSentinel::new(&cfg.name, &cfg.tenant, &wc.watch_db_path).with_cooldown(cooldown);
    Ok(Arc::new(s))
}

#[derive(Debug, Deserialize)]
struct PrecedentIntegrityCfg {
    watch_db_path: PathBuf,
    #[serde(default)]
    index_path: Option<PathBuf>,
}

fn build_precedent_integrity(cfg: SentinelConfig, cooldown: Duration) -> Result<Arc<dyn Sentinel>> {
    let pc: PrecedentIntegrityCfg = serde_yaml::from_value(cfg.config).with_context(|| {
        format!(
            "sentinel '{}': parsing precedent-integrity config",
            cfg.name
        )
    })?;
    let index_path = match pc.index_path {
        Some(path) => path,
        None if PrecedentIntegritySentinel::should_register_from_env_or_default() => {
            PrecedentIntegritySentinel::index_path_from_env_or_default()
        }
        None => bail!(
            "sentinel '{}': PRECEDENT_INDEX_PATH is unset and default sessions/index.jsonl is absent",
            cfg.name
        ),
    };
    let s = PrecedentIntegritySentinel::new(&cfg.name, &cfg.tenant, &pc.watch_db_path, &index_path)
        .with_cooldown(cooldown);
    Ok(Arc::new(s))
}

#[derive(Debug, Deserialize)]
struct LedgerDeltaCfg {
    watch_db_path: PathBuf,
    #[serde(default = "default_threshold_pct")]
    threshold_pct: f64,
    #[serde(default = "default_min_baseline_usd")]
    min_baseline_usd: f64,
    #[serde(default = "default_min_absolute_delta_usd")]
    min_absolute_delta_usd: f64,
    #[serde(default)]
    baseline_usd: Option<f64>,
}

fn default_threshold_pct() -> f64 {
    50.0
}
fn default_min_baseline_usd() -> f64 {
    0.01
}
fn default_min_absolute_delta_usd() -> f64 {
    0.25
}

fn build_ledger_delta(cfg: SentinelConfig, cooldown: Duration) -> Result<Arc<dyn Sentinel>> {
    let lc: LedgerDeltaCfg = serde_yaml::from_value(cfg.config)
        .with_context(|| format!("sentinel '{}': parsing ledger-delta config", cfg.name))?;
    if lc.threshold_pct <= 0.0 {
        bail!(
            "sentinel '{}': threshold_pct must be > 0 (got {})",
            cfg.name,
            lc.threshold_pct
        );
    }
    let s = LedgerDeltaSentinel::new(
        &cfg.name,
        &cfg.tenant,
        &lc.watch_db_path,
        lc.threshold_pct,
        lc.min_baseline_usd,
        lc.min_absolute_delta_usd,
        lc.baseline_usd,
    )
    .with_cooldown(cooldown);
    s.validate_path()
        .with_context(|| format!("sentinel '{}': validate_path", cfg.name))?;
    Ok(Arc::new(s))
}

#[derive(Debug, Deserialize)]
struct AnomalyCfg {
    watch_db_path: PathBuf,
    #[serde(default = "default_anomaly_window_ms")]
    window_ms: i64,
    #[serde(default = "default_anomaly_threshold_pct")]
    threshold_pct: f64,
    #[serde(default = "default_anomaly_min_samples")]
    min_samples: i64,
    #[serde(default = "default_anomaly_min_failures")]
    min_failures: i64,
    #[serde(default = "default_anomaly_min_error_rate")]
    min_error_rate: f64,
    #[serde(default = "default_anomaly_ewma_alpha")]
    ewma_alpha: f64,
    #[serde(default = "default_anomaly_consecutive_windows")]
    consecutive_windows: u8,
}

fn default_anomaly_window_ms() -> i64 {
    900_000
}
fn default_anomaly_threshold_pct() -> f64 {
    50.0
}
fn default_anomaly_min_samples() -> i64 {
    5
}
fn default_anomaly_min_failures() -> i64 {
    2
}
fn default_anomaly_min_error_rate() -> f64 {
    0.25
}
fn default_anomaly_ewma_alpha() -> f64 {
    0.3
}
fn default_anomaly_consecutive_windows() -> u8 {
    2
}

fn build_anomaly(cfg: SentinelConfig, cooldown: Duration) -> Result<Arc<dyn Sentinel>> {
    let ac: AnomalyCfg = serde_yaml::from_value(cfg.config)
        .with_context(|| format!("sentinel '{}': parsing anomaly config", cfg.name))?;
    if ac.window_ms <= 0 {
        bail!(
            "sentinel '{}': window_ms must be > 0 (got {})",
            cfg.name,
            ac.window_ms
        );
    }
    if ac.threshold_pct <= 0.0 {
        bail!(
            "sentinel '{}': threshold_pct must be > 0 (got {})",
            cfg.name,
            ac.threshold_pct
        );
    }
    if !(0.0..=1.0).contains(&ac.ewma_alpha) {
        bail!(
            "sentinel '{}': ewma_alpha must be in (0, 1] (got {})",
            cfg.name,
            ac.ewma_alpha
        );
    }
    let s = AnomalySentinel::new(
        &cfg.name,
        &cfg.tenant,
        AnomalyConfig {
            watch_db_path: ac.watch_db_path,
            window_ms: ac.window_ms,
            threshold_pct: ac.threshold_pct,
            min_samples: ac.min_samples,
            min_failures: ac.min_failures,
            min_error_rate: ac.min_error_rate,
            ewma_alpha: ac.ewma_alpha,
            consecutive_windows_required: ac.consecutive_windows,
        },
    )
    .with_cooldown(cooldown);
    s.validate_path()
        .with_context(|| format!("sentinel '{}': validate_path", cfg.name))?;
    Ok(Arc::new(s))
}

#[derive(Debug, Deserialize)]
struct CompletionVerifyCfg {
    watch_db_path: PathBuf,
}

fn build_completion_verify(cfg: SentinelConfig, cooldown: Duration) -> Result<Arc<dyn Sentinel>> {
    let cc: CompletionVerifyCfg = serde_yaml::from_value(cfg.config)
        .with_context(|| format!("sentinel '{}': parsing completion-verify config", cfg.name))?;
    let s = CompletionVerifySentinel::new(&cfg.name, &cfg.tenant, &cc.watch_db_path)
        .with_cooldown(cooldown);
    Ok(Arc::new(s))
}
