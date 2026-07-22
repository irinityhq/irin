//! `sentinels.yaml` loader and boot validation.
//!
//! Validates that `SentinelRegistry::load_from_yaml(path)`:
//!   - parses stock-Sentinel entries into `Arc<dyn Sentinel>`,
//!   - rejects unknown tiers, zero cooldowns, and unknown sentinel names,
//!   - calls `FileInboxSentinel::validate_path()` post-construction (P0-4),
//!   - honors `cooldown_ms` from yaml on the constructed instance.
//!
//! Yaml shape (per spec §6.2 + §9.4) — top-level array of entries:
//!   - name / tenant / tier / cooldown_ms / config
//!
//! Additional top-level policy fields are tolerated.

use gateway_sidecar::watch::registry::SentinelRegistry;
use gateway_sidecar::watch::Tier;
use std::time::Duration;

/// Builds the yaml body for a 4-sentinel config rooted at `inbox_path`.
/// Tests can substitute that path with a TempDir to satisfy validate_path().
fn yaml_four_sentinels(inbox_path: &str) -> String {
    format!(
        r#"
- name: file-inbox-watch
  tenant: sovereign
  tier: polling
  cooldown_ms: 5000
  config:
    path: {inbox_path}
    patterns: ["*.pdf", "*.md", "*.txt"]
    debounce_ms: 500
- name: silence-watch
  tenant: sovereign
  tier: polling
  cooldown_ms: 60000
  config:
    threshold_hours: 1
    ledger_db_path: /tmp/sentinel-test-ledger.db
    backlog_path: /tmp/sentinel-test-backlog
- name: gateway-active-watch
  tenant: sovereign
  tier: polling
  cooldown_ms: 30000
  config:
    url: http://127.0.0.1:8081/council/stats
    jsonpath: $.active_total
    threshold: 5
- name: watch-health-watch
  tenant: sovereign
  tier: polling
  cooldown_ms: 600000
  config:
    watch_db_path: /tmp/sentinel-test-watch.db
- name: completion-verify-watch
  tenant: sovereign
  tier: polling
  cooldown_ms: 300000
  config:
    watch_db_path: /tmp/sentinel-test-watch.db
"#
    )
}

fn write_yaml(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
    let path = dir.path().join("sentinels.yaml");
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn t24_1_happy_path_loads_four_sentinels() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();
    let body = yaml_four_sentinels(inbox.to_str().unwrap());
    let yaml_path = write_yaml(&tmp, &body);

    let sentinels =
        SentinelRegistry::load_from_yaml(&yaml_path).expect("happy-path yaml must load");
    assert_eq!(sentinels.len(), 5, "expected 5 loaded sentinels");
    let names: Vec<&str> = sentinels.iter().map(|s| s.sentinel.name()).collect();
    assert!(names.contains(&"file-inbox-watch"));
    assert!(names.contains(&"silence-watch"));
    assert!(names.contains(&"gateway-active-watch"));
    assert!(names.contains(&"watch-health-watch"));
    assert!(names.contains(&"completion-verify-watch"));
}

#[test]
fn t24_2_tier_snake_case_deserializes_from_yaml() {
    // Belt-and-braces check that #[serde(rename_all = "snake_case")] on
    // Tier behaves the same under serde_yaml as it does under serde_json
    // (the test below uses the live registry, not a private serde call).
    let tmp = tempfile::tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();
    let body = yaml_four_sentinels(inbox.to_str().unwrap());
    let yaml_path = write_yaml(&tmp, &body);

    let sentinels = SentinelRegistry::load_from_yaml(&yaml_path).unwrap();
    // All four spec'd as `polling`.
    for s in &sentinels {
        assert_eq!(
            s.sentinel.tier(),
            Tier::Polling,
            "sentinel {} had wrong tier",
            s.sentinel.name()
        );
    }
}

#[test]
fn t24_3_bad_tier_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let body = r#"
- name: file-inbox-watch
  tenant: sovereign
  tier: turbo
  cooldown_ms: 5000
  config:
    path: /tmp
    patterns: ["*.pdf"]
    debounce_ms: 500
"#;
    let yaml_path = write_yaml(&tmp, body);
    let err = SentinelRegistry::load_from_yaml(&yaml_path)
        .map(drop)
        .expect_err("bad tier must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("tier") || msg.contains("turbo") || msg.contains("variant"),
        "error should mention tier issue, got: {msg}"
    );
}

#[test]
fn t24_4_zero_cooldown_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();
    let body = format!(
        r#"
- name: file-inbox-watch
  tenant: sovereign
  tier: polling
  cooldown_ms: 0
  config:
    path: {}
    patterns: ["*.pdf"]
    debounce_ms: 500
"#,
        inbox.to_str().unwrap()
    );
    let yaml_path = write_yaml(&tmp, &body);
    let err = SentinelRegistry::load_from_yaml(&yaml_path)
        .map(drop)
        .expect_err("cooldown_ms=0 must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("cooldown"),
        "error should mention cooldown, got: {msg}"
    );
}

#[test]
fn t24_5_unknown_sentinel_name_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let body = r#"
- name: rando-watch
  tenant: sovereign
  tier: polling
  cooldown_ms: 5000
  config: {}
"#;
    let yaml_path = write_yaml(&tmp, body);
    let err = SentinelRegistry::load_from_yaml(&yaml_path)
        .map(drop)
        .expect_err("unknown sentinel name must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("rando-watch") || msg.contains("unknown"),
        "error should mention the unknown name, got: {msg}"
    );
    assert!(
        msg.contains("file-inbox-watch"),
        "error should list known sentinel names, got: {msg}"
    );
}

#[test]
fn t24_6_missing_inbox_path_rejected_with_bind_mount_hint() {
    // P0-4 fail-fast: file-inbox validate_path() runs at boot.
    let tmp = tempfile::tempdir().unwrap();
    let body = r#"
- name: file-inbox-watch
  tenant: sovereign
  tier: polling
  cooldown_ms: 5000
  config:
    path: /this/path/does/not/exist/inbox
    patterns: ["*.pdf"]
    debounce_ms: 500
"#;
    let yaml_path = write_yaml(&tmp, body);
    let err = SentinelRegistry::load_from_yaml(&yaml_path)
        .map(drop)
        .expect_err("missing inbox path must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("/this/path/does/not/exist/inbox"),
        "error should name the bad path, got: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("missing") || msg.to_lowercase().contains("unreadable"),
        "error should indicate missing/unreadable, got: {msg}"
    );
}

#[test]
fn t24_7b_declared_tier_must_match_implementation_tier() {
    // Defense in depth: yaml says `tier: fast` for file-inbox-watch (which
    // is hardcoded Polling) — loader must reject, not silently accept.
    let tmp = tempfile::tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();
    let body = format!(
        r#"
- name: file-inbox-watch
  tenant: sovereign
  tier: fast
  cooldown_ms: 5000
  config:
    path: {}
    patterns: ["*.pdf"]
    debounce_ms: 500
"#,
        inbox.to_str().unwrap()
    );
    let yaml_path = write_yaml(&tmp, &body);
    let err = SentinelRegistry::load_from_yaml(&yaml_path)
        .map(drop)
        .expect_err("declared/impl tier mismatch must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("tier"),
        "error should mention tier mismatch, got: {msg}"
    );
}

#[test]
fn t24_7_yaml_cooldown_is_honored_on_constructed_sentinel() {
    // Loader must call .with_cooldown() so the yaml value reaches the
    // running sentinel — not just pass validation and then be discarded.
    let tmp = tempfile::tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    std::fs::create_dir(&inbox).unwrap();
    let body = format!(
        r#"
- name: file-inbox-watch
  tenant: sovereign
  tier: polling
  cooldown_ms: 12345
  config:
    path: {}
    patterns: ["*.pdf"]
    debounce_ms: 500
"#,
        inbox.to_str().unwrap()
    );
    let yaml_path = write_yaml(&tmp, &body);
    let sentinels = SentinelRegistry::load_from_yaml(&yaml_path).unwrap();
    assert_eq!(sentinels.len(), 1);
    assert_eq!(
        sentinels[0].sentinel.cooldown(),
        Duration::from_millis(12345),
        "cooldown_ms from yaml must be honored on the constructed sentinel"
    );
    // And the raw config blob is captured for /watch/list.
    assert_eq!(
        sentinels[0].config_json["patterns"][0],
        serde_json::Value::String("*.pdf".into())
    );
}

#[test]
fn t24_8_ledger_delta_watch_loads_from_yaml() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = tmp.path().join("watch.db");
    // Minimal schema: validate_path only needs a readable sqlite file.
    rusqlite::Connection::open(&watch_db).unwrap();

    let body = format!(
        r#"
- name: ledger-delta-watch
  tenant: sovereign
  tier: fast
  cooldown_ms: 45000
  config:
    watch_db_path: {}
    threshold_pct: 40.0
    min_baseline_usd: 0.05
    min_absolute_delta_usd: 0.50
"#,
        watch_db.display()
    );
    let yaml_path = write_yaml(&tmp, &body);
    let sentinels = SentinelRegistry::load_from_yaml(&yaml_path).unwrap();
    assert_eq!(sentinels.len(), 1);
    assert_eq!(sentinels[0].sentinel.name(), "ledger-delta-watch");
    assert_eq!(sentinels[0].sentinel.tier(), Tier::Fast);
    assert_eq!(
        sentinels[0].sentinel.cooldown(),
        Duration::from_millis(45000)
    );
}

#[test]
fn t24_9_anomaly_watch_loads_from_yaml() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = tmp.path().join("watch.db");
    rusqlite::Connection::open(&watch_db).unwrap();

    let body = format!(
        r#"
- name: anomaly-watch
  tenant: sovereign
  tier: fast
  cooldown_ms: 60000
  config:
    watch_db_path: {}
    window_ms: 600000
    threshold_pct: 75.0
    min_samples: 4
    min_failures: 2
    min_error_rate: 0.30
    ewma_alpha: 0.25
    consecutive_windows: 2
"#,
        watch_db.display()
    );
    let yaml_path = write_yaml(&tmp, &body);
    let sentinels = SentinelRegistry::load_from_yaml(&yaml_path).unwrap();
    assert_eq!(sentinels.len(), 1);
    assert_eq!(sentinels[0].sentinel.name(), "anomaly-watch");
    assert_eq!(sentinels[0].sentinel.tier(), Tier::Fast);
}

#[test]
fn t24_10_precedent_integrity_watch_loads_from_yaml() {
    let tmp = tempfile::tempdir().unwrap();
    let watch_db = tmp.path().join("watch.db");
    rusqlite::Connection::open(&watch_db).unwrap();
    let index_path = tmp.path().join("sessions").join("index.jsonl");

    let body = format!(
        r#"
- name: precedent-integrity-watch
  tenant: sovereign
  tier: polling
  cooldown_ms: 60000
  config:
    watch_db_path: {}
    index_path: {}
"#,
        watch_db.display(),
        index_path.display()
    );
    let yaml_path = write_yaml(&tmp, &body);
    let sentinels = SentinelRegistry::load_from_yaml(&yaml_path).unwrap();
    assert_eq!(sentinels.len(), 1);
    assert_eq!(sentinels[0].sentinel.name(), "precedent-integrity-watch");
    assert_eq!(sentinels[0].sentinel.tier(), Tier::Polling);
    assert_eq!(
        sentinels[0].sentinel.cooldown(),
        Duration::from_millis(60000)
    );
}
