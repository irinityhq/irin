use super::*;

#[test]
fn parse_ws_start_fields_budget_tier_then_tear_down_and_specops_threshold() {
    let payload = serde_json::json!({
        "topic": "smoke",
        "mode": "pathfind",
        "budget_max_usd": 0.01,
        "tier": "sovereign",
        "then_tear_down": true,
        "auto_specops_threshold": 0.65,
    });
    let out = parse_ws_start_fields(&payload, false).unwrap();
    assert_eq!(out.fields.budget_max_usd, Some(0.01));
    assert_eq!(out.fields.tier, "sovereign");
    assert!(out.fields.then_tear_down);
    assert!((out.fields.auto_specops_threshold - 0.65).abs() < f64::EPSILON);
    assert!(!out.coerce_then_tear_down);
}

#[test]
fn parse_ws_start_fields_rejects_invalid_worker_provenance() {
    let payload = serde_json::json!({
        "worker_provenance": "not-an-object"
    });
    assert!(parse_ws_start_fields(&payload, false).is_err());
}

#[test]
fn parse_ws_start_fields_accepts_gateway_and_direct_fire() {
    let payload = serde_json::json!({
        "topic": "smoke",
        "via_gateway": true,
        "sensitivity": "yellow",
        "direct_fire": "munger",
    });
    let out = parse_ws_start_fields(&payload, false).unwrap();
    assert_eq!(out.fields.via_gateway, Some(true));
    // Lowercase wire value normalized to the provider layer's UPPERCASE.
    assert_eq!(out.fields.sensitivity.as_deref(), Some("YELLOW"));
    assert_eq!(out.fields.direct_fire.as_deref(), Some("munger"));
}

#[test]
fn parse_ws_start_fields_defaults_gateway_fields_to_none() {
    let out = parse_ws_start_fields(&serde_json::json!({ "topic": "x" }), false).unwrap();
    assert_eq!(out.fields.via_gateway, None);
    assert_eq!(out.fields.sensitivity, None);
    assert_eq!(out.fields.direct_fire, None);
}

#[test]
fn parse_ws_start_fields_rejects_invalid_sensitivity() {
    // Pinned contract: lowercase green|yellow|red only — uppercase rejects too.
    for bad in [
        serde_json::json!("GREEN"),
        serde_json::json!("amber"),
        serde_json::json!(" red"),
        serde_json::json!(1),
    ] {
        let payload = serde_json::json!({ "sensitivity": bad });
        assert!(
            parse_ws_start_fields(&payload, false).is_err(),
            "sensitivity {bad} should be rejected"
        );
    }
}

#[test]
fn parse_ws_start_fields_rejects_unknown_direct_fire() {
    for bad in [
        serde_json::json!("kiss-review"),
        serde_json::json!("MUNGER"),
        serde_json::json!("wargame"),
        serde_json::json!(""),
        serde_json::json!(7),
    ] {
        let payload = serde_json::json!({ "direct_fire": bad });
        assert!(
            parse_ws_start_fields(&payload, false).is_err(),
            "direct_fire {bad} should be rejected"
        );
    }
}

#[test]
fn parse_ws_start_fields_caps_topic_length() {
    // Over-cap topic is rejected with a clear client error.
    let over = "a".repeat(MAX_WS_TOPIC_BYTES + 1);
    let payload = serde_json::json!({ "topic": over });
    assert!(
        parse_ws_start_fields(&payload, false).is_err(),
        "topic longer than {MAX_WS_TOPIC_BYTES} bytes should be rejected"
    );

    // A topic exactly at the cap is accepted.
    let at_cap = "a".repeat(MAX_WS_TOPIC_BYTES);
    let out = parse_ws_start_fields(&serde_json::json!({ "topic": at_cap }), false)
        .expect("topic exactly at the cap should be accepted");
    assert_eq!(out.fields.topic.len(), MAX_WS_TOPIC_BYTES);
}

#[test]
fn parse_ws_start_fields_caps_context_length() {
    // Over-cap context is rejected with a clear client error.
    let over = "a".repeat(MAX_WS_CONTEXT_BYTES + 1);
    let payload = serde_json::json!({ "topic": "x", "context": over });
    assert!(
        parse_ws_start_fields(&payload, false).is_err(),
        "context longer than {MAX_WS_CONTEXT_BYTES} bytes should be rejected"
    );

    // Context exactly at the cap is accepted.
    let at_cap = "a".repeat(MAX_WS_CONTEXT_BYTES);
    let out = parse_ws_start_fields(
        &serde_json::json!({ "topic": "x", "context": at_cap }),
        false,
    )
    .expect("context exactly at the cap should be accepted");
    assert_eq!(out.fields.context.len(), MAX_WS_CONTEXT_BYTES);
}

#[test]
fn parse_ws_start_fields_coerces_then_tear_down_to_pathfind() {
    let payload = serde_json::json!({
        "mode": "teardown",
        "then_tear_down": true
    });
    let out = parse_ws_start_fields(&payload, false).unwrap();
    assert!(out.coerce_then_tear_down);
    assert_eq!(out.fields.mode, Mode::Pathfind);
}

#[test]
fn normalize_ws_tier_unknown_defaults_to_best() {
    assert_eq!(normalize_ws_tier(Some("bogus")), "best");
    assert_eq!(
        normalize_ws_tier(Some("strict_sovereign")),
        "strict_sovereign"
    );
}

/// Phase 5 regression pin: the WS payload stays LENIENT — unknown mode,
/// tier, and non-positive budget silently coerce to defaults, never error.
/// (feature contract strictness applies only to POST /api/deliberate.)
#[test]
fn parse_ws_start_fields_stays_lenient_for_mode_tier_budget() {
    let payload = serde_json::json!({
        "topic": "x",
        "mode": "bogus-mode",
        "tier": "bogus-tier",
        "budget_max_usd": -3.0,
        "blind": "not-a-bool",
    });
    let out = parse_ws_start_fields(&payload, false).unwrap();
    assert_eq!(out.fields.mode, Mode::TearDown);
    assert_eq!(out.fields.tier, "best");
    assert_eq!(out.fields.budget_max_usd, None);
    assert!(!out.fields.blind);
}

#[test]
fn parse_deliberate_knobs_accepts_full_valid_set() {
    let payload = serde_json::json!({
        "mode": "pathfind",
        "tier": "sovereign",
        "budget_max_usd": 0.5,
        "validate": true,
        "validate_gate": true,
        "blind": true,
        "cabinet_name": "wargame",
    });
    let k = parse_deliberate_knobs(&payload).unwrap();
    assert_eq!(k.mode, Mode::Pathfind);
    assert_eq!(k.tier, "sovereign");
    assert_eq!(k.budget_max_usd, Some(0.5));
    assert!(k.validate);
    assert!(k.validate_gate);
    assert!(k.blind);
    assert_eq!(k.cabinet_name.as_deref(), Some("wargame"));
}

/// All knobs optional — defaults match the WS payload defaults.
#[test]
fn parse_deliberate_knobs_defaults_match_ws_defaults() {
    let k = parse_deliberate_knobs(&serde_json::json!({})).unwrap();
    assert_eq!(k.mode, Mode::TearDown);
    assert_eq!(k.tier, "best");
    assert_eq!(k.budget_max_usd, None);
    assert!(!k.validate);
    assert!(!k.validate_gate);
    assert!(!k.blind);
    assert_eq!(k.cabinet_name, None);
}

/// feature contract pinned contract: unknown/invalid values 4xx (Strict), unlike the
/// lenient WS coercion — exercised per-field.
#[test]
fn parse_deliberate_knobs_rejects_invalid_values() {
    for (field, bad) in [
        ("mode", serde_json::json!("bogus")),
        ("mode", serde_json::json!(7)),
        ("tier", serde_json::json!("bogus")),
        ("tier", serde_json::json!("")),
        ("budget_max_usd", serde_json::json!(0)),
        ("budget_max_usd", serde_json::json!(-1.0)),
        ("budget_max_usd", serde_json::json!("free")),
        ("validate", serde_json::json!("yes")),
        ("validate_gate", serde_json::json!(1)),
        ("blind", serde_json::json!("true")),
        ("cabinet_name", serde_json::json!("")),
        ("cabinet_name", serde_json::json!(7)),
    ] {
        let payload = serde_json::json!({ field: bad });
        let err = parse_deliberate_knobs(&payload)
            .expect_err(&format!("{field}={payload} should be rejected"));
        assert!(
            err.starts_with(&format!("{field}:")),
            "error should name the field: {err}"
        );
    }
}

/// PR fix: the strict REST path enforces an upper budget ceiling (default
/// 10.0). A value under the ceiling is accepted; an absurd value is
/// rejected with a field-named error. Negative / non-numeric are still
/// rejected by the pre-existing finite-and-positive guard. (NaN is not a
/// representable JSON number — serde renders it as `null`, which is treated
/// as an absent field, so it never reaches this path.)
#[test]
fn parse_deliberate_knobs_clamps_budget_to_max() {
    // Under the default ceiling — accepted.
    let ok = parse_deliberate_knobs(&serde_json::json!({ "budget_max_usd": 9.0 })).unwrap();
    assert_eq!(ok.budget_max_usd, Some(9.0));

    // Over the ceiling — rejected.
    let err = parse_deliberate_knobs(&serde_json::json!({ "budget_max_usd": 1e9 }))
        .expect_err("over-ceiling budget must be rejected");
    assert!(
        err.starts_with("budget_max_usd:"),
        "error names field: {err}"
    );

    // Negative and non-numeric are rejected by the finite/positive guard.
    for bad in [serde_json::json!(-1.0), serde_json::json!("free")] {
        assert!(
            parse_deliberate_knobs(&serde_json::json!({ "budget_max_usd": bad })).is_err(),
            "budget {bad} must be rejected"
        );
    }
}

/// The ceiling is overridable via COUNCIL_MAX_BUDGET_USD. This is the only
/// test that mutates that var (no other test reads it), so the set→read→
/// unset window does not race the parallel suite.
#[test]
fn budget_ceiling_honors_env_override() {
    // SAFETY: test-only env mutation; restored before returning.
    unsafe {
        std::env::set_var("COUNCIL_MAX_BUDGET_USD", "100.0");
    }
    let ok = parse_deliberate_knobs(&serde_json::json!({ "budget_max_usd": 50.0 }));
    let restore = ok.is_ok() && ok.as_ref().unwrap().budget_max_usd == Some(50.0);
    // SAFETY: test-only env mutation.
    unsafe {
        std::env::remove_var("COUNCIL_MAX_BUDGET_USD");
    }
    assert!(restore, "50.0 should pass when ceiling raised to 100.0");
}

/// WS stays lenient — a budget over the strict ceiling is kept verbatim,
/// never rejected (mode-union clients depend on the Phase 5 contract).
#[test]
fn ws_budget_not_clamped() {
    let payload = serde_json::json!({ "topic": "x", "budget_max_usd": 1e9 });
    let out = parse_ws_start_fields(&payload, false).unwrap();
    assert_eq!(out.fields.budget_max_usd, Some(1e9));
}

/// Explicit nulls behave like absent fields on both parse paths.
#[test]
fn parse_deliberate_knobs_treats_null_as_absent() {
    let payload = serde_json::json!({
        "mode": null,
        "tier": null,
        "budget_max_usd": null,
        "validate": null,
        "blind": null,
        "cabinet_name": null,
    });
    let k = parse_deliberate_knobs(&payload).unwrap();
    assert_eq!(k.mode, Mode::TearDown);
    assert_eq!(k.tier, "best");
    assert_eq!(k.budget_max_usd, None);
    assert_eq!(k.cabinet_name, None);
}

#[test]
fn clamp_ws_max_rounds_respects_cabinet_and_cap() {
    assert_eq!(clamp_ws_max_rounds(Some(99), 2), 2);
    assert_eq!(clamp_ws_max_rounds(Some(4), 8), 4);
    assert_eq!(clamp_ws_max_rounds(None, 3), 3);
}
