#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::mode::Mode;
    use crate::precedent;
    use crate::types::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_quality_penalty_env<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let saved = std::env::var_os("COUNCIL_CONVERGENCE_QUALITY_PENALTY");
        unsafe {
            match value {
                Some(value) => std::env::set_var("COUNCIL_CONVERGENCE_QUALITY_PENALTY", value),
                None => std::env::remove_var("COUNCIL_CONVERGENCE_QUALITY_PENALTY"),
            }
        }

        let result = f();

        unsafe {
            match saved {
                Some(value) => std::env::set_var("COUNCIL_CONVERGENCE_QUALITY_PENALTY", value),
                None => std::env::remove_var("COUNCIL_CONVERGENCE_QUALITY_PENALTY"),
            }
        }

        result
    }

    #[test]
    fn seat_response_from_provider_marks_empty_success_as_error() {
        let resp = ProviderResponse {
            model: "test-model".into(),
            text: " \n\t ".into(),
            tokens_in: 12,
            tokens_out: 0,
            error: None,
            ..Default::default()
        };

        let seat = SeatResponse::from_provider("Checker", "gpt", 1, resp, crate::scrub::redact);

        let err = seat.error.expect("empty provider text should be an error");
        assert!(err.contains("empty provider response"));
        assert_eq!(seat.text, " \n\t ");
        assert_eq!(seat.model, "test-model");
    }

    #[test]
    fn seat_response_persists_gateway_owned_request_id() {
        let resp = ProviderResponse {
            model: "grok-4.3".into(),
            text: "usable response".into(),
            gateway_provenance: Some(GatewayProvenance {
                routed_model: "grok-4.3".into(),
                routed_provider: "xai".into(),
                fallback_used: false,
                gateway_request_id: "gw-seat-ledger-id".into(),
            }),
            ..Default::default()
        };

        let seat = SeatResponse::from_provider("Analyst", "xai", 1, resp, crate::scrub::redact);
        assert_eq!(
            seat.gateway.expect("gateway provenance").gateway_request_id,
            "gw-seat-ledger-id"
        );
    }

    // ── Config loading ──────────────────────────────────────────

    #[test]
    fn config_loads_from_base_dir() {
        let config = Config::load(std::path::Path::new("."))
            .expect("Config::load should succeed from project root");
        assert!(
            !config.cabinets.is_empty(),
            "should load at least one cabinet"
        );
        assert!(config.models.models.contains_key("grok_reasoning"));
        assert!(config.models.models.contains_key("claude_opus_48"));
        assert!(config.models.models.contains_key("gpt_flagship"));
    }

    #[test]
    fn config_loads_all_expected_cabinets() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let expected = [
            "standard",
            "quick",
            "duo",
            "heritage",
            "reflection",
            "warroom",
            "wargame",
            "sovereign",
            "freeride",
            "trinity",
            "code-verify",
            "triad-strategy",
            "triad-architecture",
            "triad-debugging",
            "triad-product",
            "triad-risk",
            "triad-shipping",
        ];
        for name in expected {
            assert!(
                config.cabinets.contains_key(name),
                "missing cabinet: {}",
                name
            );
        }
    }

    #[test]
    fn config_get_cabinet_returns_error_for_unknown() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        assert!(config.get_cabinet("nonexistent").is_err());
    }

    #[test]
    fn config_external_cabinet_loading() {
        let mut config = Config::load(std::path::Path::new(".")).unwrap();
        let key = config
            .load_external_cabinet(std::path::Path::new("cabinets/heritage.yaml"))
            .expect("should load external cabinet");
        assert_eq!(key, "heritage");
    }

    #[test]
    fn code_verify_chair_declares_verifier_system_prompt() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let cabinet = config
            .get_cabinet("code-verify")
            .expect("code-verify cabinet must be loadable");
        assert!(
            cabinet.local_code_only,
            "code-verify must disable global provider preflights"
        );
        assert_eq!(cabinet.chair.provider, "codex_cli");
        assert!(
            cabinet
                .seats
                .iter()
                .all(|seat| matches!(seat.provider.as_str(), "grok_build" | "codex_cli"))
        );
        let system = cabinet
            .chair
            .system
            .as_deref()
            .expect("code-verify chair must have verifier system prompt");

        assert!(system.contains("local-code verifier Chair"));
        assert!(system.contains("seat outputs"));
        assert!(system.contains("NO_EVIDENCE"));
    }

    #[test]
    fn local_code_only_external_cabinet_rejects_non_cli_provider() {
        let mut config = Config::load(std::path::Path::new(".")).unwrap();
        let path =
            std::env::temp_dir().join(format!("bad-local-code-{}.yaml", uuid::Uuid::new_v4()));
        let yaml = r#"
name: Bad Local Code
rounds: 1
local_code_only: true
seats:
  - name: API Seat
    provider: grok
    model: grok-4.3
    system: verifier
chair:
  name: Chair
  provider: codex_cli
  model: gpt-5.6-sol
"#;
        std::fs::write(&path, yaml).unwrap();

        let err = config
            .load_external_cabinet(&path)
            .expect_err("local_code_only cabinet must reject non-CLI providers")
            .to_string();
        let _ = std::fs::remove_file(&path);

        assert!(err.contains("local_code_only"));
        assert!(err.contains("API Seat"));
    }

    #[test]
    fn local_code_only_custom_cabinet_validator_rejects_non_cli_provider() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let yaml = r#"
name: Bad Custom Local Code
rounds: 1
local_code_only: true
seats:
  - name: API Seat
    provider: grok
    model: grok-4.3
    system: verifier
chair:
  name: Chair
  provider: codex_cli
  model: gpt-5.6-sol
"#;
        let cabinet: Cabinet = serde_yaml::from_str(yaml).unwrap();

        let err = config
            .validate_cabinet_for_execution("custom_cabinet", &cabinet)
            .expect_err("custom local_code_only cabinet must reject non-CLI providers")
            .to_string();

        assert!(err.contains("local_code_only"));
        assert!(err.contains("API Seat"));
    }

    #[test]
    fn local_code_only_custom_cabinet_rejects_unknown_cli_provider() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let yaml = r#"
name: Bad Custom Local Code
rounds: 1
local_code_only: true
seats:
  - name: Unknown CLI Seat
    provider: foo_cli
    model: foo-model
    system: verifier
chair:
  name: Chair
  provider: codex_cli
  model: gpt-5.6-sol
"#;
        let cabinet: Cabinet = serde_yaml::from_str(yaml).unwrap();

        let err = config
            .validate_cabinet_for_execution("custom_cabinet", &cabinet)
            .expect_err("local_code_only cabinet must reject unknown CLI providers")
            .to_string();

        assert!(err.contains("read-only CLI-agent provider"));
        assert!(err.contains("Unknown CLI Seat"));
    }

    #[test]
    fn local_code_only_custom_cabinet_rejects_tools_cli_provider() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let yaml = r#"
name: Bad Custom Local Code
rounds: 1
local_code_only: true
seats:
  - name: AGY Seat
    provider: agy_cli
    model: agy-default
    system: verifier
chair:
  name: Chair
  provider: codex_cli
  model: gpt-5.6-sol
"#;
        let cabinet: Cabinet = serde_yaml::from_str(yaml).unwrap();

        let err = config
            .validate_cabinet_for_execution("custom_cabinet", &cabinet)
            .expect_err("local_code_only cabinet must reject non-read-only CLI providers")
            .to_string();

        assert!(err.contains("read-only CLI-agent provider"));
        assert!(err.contains("AGY Seat"));
    }

    #[test]
    fn code_verify_engine_chair_uses_verifier_system_prompt() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let cabinet = config.get_cabinet("code-verify").unwrap();
        let system = crate::engine::deliberate::chair_system_for(cabinet, Mode::Harden);

        assert!(system.contains("local-code verifier Chair"));
        assert!(system.contains("seat outputs"));
        assert!(system.contains("PASS only when"));
        assert!(system.contains(Mode::Harden.chair_instruction()));
    }

    #[test]
    fn session_serializes_chair_provider_provenance() {
        let session = CouncilSession {
            session_id: "test-session".to_string(),
            parent_request_id: None,
            topic: "Test Topic".to_string(),
            cabinet_name: "test-cabinet".to_string(),
            rounds: vec![],
            synthesis: Some("Final decision.".to_string()),
            synthesis_model: Some("claude-opus-4-8".to_string()),
            total_tokens: 1500,
            total_latency_ms: 5000,
            total_cost_usd: 0.15,
            specops_triggered: false,
            specops_cost_usd: 0.0,
            mode: SessionMode::TearDown,
            precedent_ids: vec![],
            timestamp: chrono::Utc::now(),
            schema_version: 2,
            tier: "best".to_string(),
            budget: None,
            context_sources: vec![],
            origin: SessionOrigin::Cli,
            execution_route: ExecutionRoute::Governed,
            gateway_sensitivity: Some("yellow".to_string()),
            chair_tokens_in: 0,
            chair_tokens_out: 0,
            chair_cost_usd: 0.0,
            chair_provider_provenance: Some(ProviderProvenance::cli_readonly(
                "codex_cli",
                "usage_unavailable",
            )),
            chair_gateway_provenance: Some(GatewayProvenance {
                gateway_request_id: "gw-chair-123".to_string(),
                routed_model: "claude-opus-4-8".to_string(),
                routed_provider: "anthropic".to_string(),
                fallback_used: false,
            }),
            worker_metrics: None,
            worker_provenance: None,
        };

        let value = serde_json::to_value(&session).unwrap();

        assert_eq!(
            value["chair_provider_provenance"]["access_mode"],
            "cli_agent_readonly"
        );
        assert_eq!(
            value["chair_provider_provenance"]["filesystem"],
            "read_only"
        );
        assert_eq!(value["execution_route"], "governed");
        assert_eq!(value["gateway_sensitivity"], "yellow");
        assert_eq!(
            value["chair_gateway_provenance"]["gateway_request_id"],
            "gw-chair-123"
        );
    }

    #[test]
    fn round_persists_all_judge_gateway_attempts_and_defaults_legacy_empty() {
        let round = RoundResult {
            round_num: 1,
            responses: vec![],
            convergence_score: 0.72,
            converged: false,
            judge_provider: Some("xai".into()),
            judge_assessment: None,
            judge_gateway_attempts: vec![
                GatewayProvenance {
                    gateway_request_id: "gw-judge-failed".into(),
                    routed_model: "first-judge".into(),
                    routed_provider: "nvidia".into(),
                    fallback_used: false,
                },
                GatewayProvenance {
                    gateway_request_id: "gw-judge-success".into(),
                    routed_model: "second-judge".into(),
                    routed_provider: "xai".into(),
                    fallback_used: false,
                },
            ],
            flip_flop_hash: None,
            validation_report: None,
        };

        let value = serde_json::to_value(&round).unwrap();
        assert_eq!(
            value["judge_gateway_attempts"][0]["gateway_request_id"],
            "gw-judge-failed"
        );
        assert_eq!(
            value["judge_gateway_attempts"][1]["gateway_request_id"],
            "gw-judge-success"
        );

        let legacy = serde_json::json!({
            "round_num": 1,
            "responses": [],
            "convergence_score": 0.5,
            "converged": false
        });
        let parsed: RoundResult = serde_json::from_value(legacy).unwrap();
        assert!(parsed.judge_gateway_attempts.is_empty());
    }

    #[test]
    fn triage_cabinet_declares_directive_proposal_v1_synthesis_mode() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let triage = config
            .get_cabinet("triage")
            .expect("triage cabinet must be loadable");
        assert_eq!(triage.synthesis_mode, SynthesisMode::DirectiveProposalV1);
    }

    #[test]
    fn non_triage_cabinets_default_to_generic_synthesis_mode() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        for name in ["standard", "warroom", "wargame"] {
            let c = config.get_cabinet(name).unwrap();
            assert_eq!(
                c.synthesis_mode,
                SynthesisMode::Generic,
                "cabinet {} should be generic",
                name
            );
        }
    }

    #[test]
    fn directive_proposal_v1_mode_does_not_use_generic_scaffold() {
        // The Chair system prompt for directive_proposal_v1 must never contain
        // the generic numbered 1-7 synthesis scaffold used by other cabinets.
        let scaffold = "Structure your synthesis:\n1. Summary of positions";
        let directive_system = crate::engine::deliberate::DIRECTIVE_TRIAGE_CHAIR_SYSTEM;

        assert!(
            !directive_system.contains(scaffold),
            "DirectiveProposalV1 Chair prompt must suppress generic synthesis scaffold"
        );
        assert!(
            directive_system.contains("machine-output JSON fence"),
            "DirectiveProposalV1 Chair prompt must reference the strict fence contract"
        );
        assert!(
            directive_system.contains("No prose, no numbered lists"),
            "DirectiveProposalV1 Chair prompt must explicitly forbid prose and lists"
        );
    }

    #[test]
    fn suspect_judge_quality_raises_convergence_threshold_when_enabled() {
        let assessment = JudgeAssessment {
            convergence: 0.60,
            intent_aligned: true,
            drift: None,
            quality_flag: Some("circular".into()),
            homogeneity_score: None,
            quick_agreement: None,
            recommendation: "converged".into(),
            confidence: 0.8,
        };

        let threshold = crate::engine::deliberate::effective_convergence_threshold(
            0.60,
            Some(&assessment),
            true,
        );

        assert_eq!(threshold, 0.75);
        assert!(
            0.60 < threshold,
            "suspect quality should prevent pathfind's 60% base threshold from early-stopping"
        );
        assert!(
            0.90 >= threshold,
            "high convergence can still early-stop after the quality penalty"
        );
    }

    #[test]
    fn suspect_judge_quality_preserves_threshold_when_disabled() {
        let assessment = JudgeAssessment {
            convergence: 0.60,
            intent_aligned: true,
            drift: None,
            quality_flag: Some("circular".into()),
            homogeneity_score: None,
            quick_agreement: None,
            recommendation: "converged".into(),
            confidence: 0.8,
        };

        let threshold = crate::engine::deliberate::effective_convergence_threshold(
            0.60,
            Some(&assessment),
            false,
        );

        assert_eq!(threshold, 0.60);
    }

    #[test]
    fn clean_judge_quality_preserves_base_convergence_threshold() {
        let assessment = JudgeAssessment {
            convergence: 0.70,
            intent_aligned: true,
            drift: None,
            quality_flag: None,
            homogeneity_score: None,
            quick_agreement: None,
            recommendation: "converged".into(),
            confidence: 0.9,
        };

        let threshold = crate::engine::deliberate::effective_convergence_threshold(
            0.70,
            Some(&assessment),
            true,
        );

        assert_eq!(threshold, 0.70);
    }

    #[test]
    fn convergence_quality_penalty_env_defaults_to_validate_flag() {
        with_quality_penalty_env(None, || {
            assert!(crate::engine::deliberate::convergence_quality_penalty_enabled(true));
            assert!(!crate::engine::deliberate::convergence_quality_penalty_enabled(false));
        });
    }

    #[test]
    fn convergence_quality_penalty_env_can_force_always_or_off() {
        with_quality_penalty_env(Some("always"), || {
            assert!(crate::engine::deliberate::convergence_quality_penalty_enabled(false));
        });
        with_quality_penalty_env(Some("off"), || {
            assert!(!crate::engine::deliberate::convergence_quality_penalty_enabled(true));
        });
    }

    // ── Model cost matching ─────────────────────────────────────

    #[test]
    fn model_cost_exact_match() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let cost = config
            .models
            .estimate_cost("grok-4.3", 1_000_000, 1_000_000, 0);
        let expected = 1.25 + 2.50; // $1.25/MTok in + $2.50/MTok out
        assert!(
            (cost - expected).abs() < 0.01,
            "grok-4.3 cost for 1M in + 1M out: expected ~{}, got {}",
            expected,
            cost
        );
    }

    #[test]
    fn model_cost_with_cache() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        // 500k uncached + 500k cached input, 1M output
        let cost = config
            .models
            .estimate_cost("grok-4.3", 1_000_000, 1_000_000, 500_000);
        // uncached: 500k * $1.25/M = $0.625
        // cached: 500k * $0.125/M = $0.0625
        // output: 1M * $2.50/M = $2.50
        let expected = 0.625 + 0.0625 + 2.50;
        assert!(
            (cost - expected).abs() < 0.01,
            "cached cost: expected ~{}, got {}",
            expected,
            cost
        );
    }

    #[test]
    fn model_cost_prefix_match_nim() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let cost =
            config
                .models
                .estimate_cost("nvidia/nemotron-3-nano-30b-a3b", 1_000_000, 1_000_000, 0);
        assert_eq!(cost, 0.0, "NIM models should be free");
    }

    #[test]
    fn model_cost_unknown_returns_zero() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let cost =
            config
                .models
                .estimate_cost("totally-unknown-model-xyz", 1_000_000, 1_000_000, 0);
        assert_eq!(cost, 0.0, "unknown model should return 0 cost");
    }

    #[test]
    fn grok_model_id_is_4_3() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let entry = config.models.models.get("grok_reasoning").unwrap();
        assert_eq!(
            entry.id, "grok-4.3",
            "grok_reasoning must point to grok-4.3"
        );
    }

    #[test]
    fn grok_pricing_updated() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let entry = config.models.models.get("grok_reasoning").unwrap();
        assert_eq!(entry.pricing.input, 1.25);
        assert_eq!(entry.pricing.cached_input, 0.125);
        assert_eq!(entry.pricing.output, 2.50);
    }

    // ── v2 index serialization ──────────────────────────────────

    #[test]
    fn precedent_entry_serializes_v2_field_names() {
        let entry = PrecedentEntry {
            schema_version: 2,
            session_id: "abc123".into(),
            timestamp: "2026-05-08T12:00:00".into(),
            topic: "test topic".into(),
            keywords: vec!["rust".into(), "council".into()],
            digest: "test digest".into(),
            confidence: "HIGH".into(),
            cabinet: "standard".into(),
            convergence: 0.85,
            mode: "teardown".into(),
            seat_count: 3,
            rounds: 2,
            synthesis_model: Some("claude-opus-4-8".into()),
            version: "0.3.0".into(),
            tier: "best".into(),
            judge_provider: Some("gpt".into()),
            failure_status: None,
            cited_by: vec![],
            challenged_by: vec![],
            origin: Default::default(),
            execution_route: Default::default(),
            gateway_sensitivity: None,
            worker_provenance: None,
            parent_request_id: None,
        };

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // v2 field names
        assert!(
            parsed.get("schema_version").is_some(),
            "must have schema_version"
        );
        assert!(parsed.get("id").is_some(), "session_id serializes as 'id'");
        assert!(parsed.get("ts").is_some(), "timestamp serializes as 'ts'");
        assert!(
            parsed.get("ruling_digest").is_some(),
            "digest serializes as 'ruling_digest'"
        );
        assert!(parsed.get("confidence").is_some());
        assert!(parsed.get("convergence").is_some());
        assert!(parsed.get("mode").is_some());
        assert!(parsed.get("seat_count").is_some());
        assert!(parsed.get("rounds").is_some());
        assert!(parsed.get("tier").is_some());

        // must NOT have old field names
        assert!(
            parsed.get("session_id").is_none(),
            "should not serialize as 'session_id'"
        );
        assert!(
            parsed.get("timestamp").is_none(),
            "should not serialize as 'timestamp'"
        );
        assert!(
            parsed.get("digest").is_none(),
            "should not serialize as 'digest'"
        );
    }

    #[test]
    fn precedent_entry_deserializes_legacy_field_names() {
        let legacy_json = r#"{
            "session_id": "old123",
            "topic": "legacy topic",
            "cabinet": "standard",
            "keywords": ["test"],
            "digest": "old digest",
            "timestamp": "2026-01-01T00:00:00"
        }"#;

        let entry: PrecedentEntry = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(entry.session_id, "old123");
        assert_eq!(entry.digest, "old digest");
        assert_eq!(entry.timestamp, "2026-01-01T00:00:00");
        assert_eq!(entry.schema_version, 2); // default
    }

    #[test]
    fn precedent_entry_deserializes_v2_field_names() {
        let v2_json = r#"{
            "schema_version": 2,
            "id": "new456",
            "ts": "2026-05-08T12:00:00",
            "topic": "v2 topic",
            "keywords": ["test"],
            "ruling_digest": "v2 digest",
            "confidence": "HIGH",
            "cabinet": "warroom",
            "convergence": 0.92,
            "mode": "teardown",
            "seat_count": 5,
            "rounds": 3,
            "synthesis_model": "claude-opus-4-8",
            "version": "0.3.0",
            "tier": "best",
            "judge_provider": "gpt",
            "failure_status": null,
            "cited_by": [],
            "challenged_by": ["xyz789"]
        }"#;

        let entry: PrecedentEntry = serde_json::from_str(v2_json).unwrap();
        assert_eq!(entry.session_id, "new456");
        assert_eq!(entry.digest, "v2 digest");
        assert_eq!(entry.timestamp, "2026-05-08T12:00:00");
        assert_eq!(entry.confidence, "HIGH");
        assert_eq!(entry.convergence, 0.92);
        assert_eq!(entry.seat_count, 5);
        assert_eq!(entry.rounds, 3);
        assert_eq!(entry.challenged_by, vec!["xyz789"]);
    }

    // ── Session deserialization ──────────────────────────────────

    #[test]
    fn session_deserializes_v2() {
        let json = r#"{
            "session_id": "test-sess",
            "topic": "Test",
            "cabinet_name": "standard",
            "rounds": [{
                "round_num": 1,
                "responses": [],
                "convergence_score": 0.8,
                "converged": false,
                "judge_provider": "gpt",
                "judge_assessment": null,
                "flip_flop_hash": null
            }],
            "synthesis": "The answer is 42.",
            "total_tokens": 1000,
            "total_latency_ms": 5000,
            "total_cost_usd": 0.05,
            "mode": "teardown",
            "precedent_ids": [],
            "timestamp": "2026-05-08T12:00:00Z",
            "schema_version": 2,
            "tier": "best"
        }"#;

        let session: CouncilSession = serde_json::from_str(json).unwrap();
        assert_eq!(session.session_id, "test-sess");
        assert_eq!(session.schema_version, 2);
        assert_eq!(session.tier, "best");
        assert!(matches!(session.mode, SessionMode::TearDown));
        assert_eq!(session.rounds.len(), 1);
    }

    #[test]
    fn session_deserializes_legacy_python_mode() {
        let json = r#"{
            "session_id": "legacy",
            "topic": "Old",
            "cabinet_name": "standard",
            "rounds": [],
            "timestamp": "2025-12-01T00:00:00Z",
            "mode": "normal"
        }"#;

        let session: CouncilSession = serde_json::from_str(json).unwrap();
        assert!(matches!(session.mode, SessionMode::Normal));
        assert_eq!(session.schema_version, 2); // default
    }

    #[test]
    fn session_deserializes_null_mode() {
        let json = r#"{
            "session_id": "nullmode",
            "topic": "Null",
            "cabinet_name": "standard",
            "rounds": [],
            "timestamp": "2025-12-01T00:00:00Z",
            "mode": null
        }"#;

        let session: CouncilSession = serde_json::from_str(json).unwrap();
        // null mode = Python-era session predating the mode flag → Normal,
        // matching the sessions_list "normal" default (B04 reconciliation).
        assert!(matches!(session.mode, SessionMode::Normal));
    }

    #[test]
    fn session_deserializes_absent_mode_as_normal() {
        let json = r#"{
            "session_id": "nomode",
            "topic": "Absent",
            "cabinet_name": "standard",
            "rounds": [],
            "timestamp": "2025-12-01T00:00:00Z"
        }"#;

        let session: CouncilSession = serde_json::from_str(json).unwrap();
        // Absent key = legacy file written before the mode flag existed.
        assert!(matches!(session.mode, SessionMode::Normal));
        assert!(matches!(session.execution_route, ExecutionRoute::Unknown));
    }

    #[test]
    fn session_deserializes_known_and_unknown_mode_strings() {
        let mk = |mode: &str| -> CouncilSession {
            let json = format!(
                r#"{{
                    "session_id": "modes",
                    "topic": "Modes",
                    "cabinet_name": "standard",
                    "rounds": [],
                    "timestamp": "2025-12-01T00:00:00Z",
                    "mode": "{mode}"
                }}"#
            );
            serde_json::from_str(&json).unwrap()
        };

        assert!(matches!(mk("teardown").mode, SessionMode::TearDown));
        assert!(matches!(mk("wargame").mode, SessionMode::Wargame));
        // Unknown STRINGS hit #[serde(other)] → Unknown, not the Normal fallback.
        assert!(matches!(mk("garbage").mode, SessionMode::Unknown));
    }

    #[test]
    fn session_without_synthesis_omits_keys_on_wire() {
        // B05 wire contract: synthesis / synthesis_model carry
        // skip_serializing_if = Option::is_none — the keys must be ABSENT
        // (not null) when unset. The UI types them as optional accordingly.
        let json = r#"{
            "session_id": "nosynth",
            "topic": "Unsynthesized",
            "cabinet_name": "standard",
            "rounds": [],
            "timestamp": "2025-12-01T00:00:00Z"
        }"#;

        let session: CouncilSession = serde_json::from_str(json).unwrap();
        assert!(session.synthesis.is_none());

        let wire = serde_json::to_value(&session).unwrap();
        let obj = wire.as_object().unwrap();
        assert!(!obj.contains_key("synthesis"));
        assert!(!obj.contains_key("synthesis_model"));
        // mode has no skip attr — always present on the wire.
        assert_eq!(wire["mode"], "normal");
    }

    // ── Precedent index round-trip ──────────────────────────────

    #[test]
    fn entry_from_session_produces_v2() {
        let session = CouncilSession {
            session_id: "test-123".into(),
            parent_request_id: None,
            topic: "Unit test topic for council-rs".into(),
            cabinet_name: "standard".into(),
            rounds: vec![RoundResult {
                round_num: 1,
                responses: vec![
                    SeatResponse {
                        seat_name: "Analyst".into(),
                        provider: "grok".into(),
                        model: "grok-4.3".into(),
                        text: "Analysis here".into(),
                        ..Default::default()
                    },
                    SeatResponse {
                        seat_name: "Checker".into(),
                        provider: "gpt".into(),
                        model: "gpt-5.6-sol".into(),
                        text: "Check here".into(),
                        ..Default::default()
                    },
                ],
                convergence_score: 0.87,
                converged: false,
                judge_provider: Some("gpt".into()),
                judge_assessment: None,
                judge_gateway_attempts: vec![],
                flip_flop_hash: None,
                validation_report: None,
            }],
            synthesis: Some("HIGH confidence ruling: test passes.".into()),
            synthesis_model: Some("claude-opus-4-8".into()),
            total_tokens: 5000,
            total_latency_ms: 10000,
            total_cost_usd: 0.15,
            specops_triggered: false,
            specops_cost_usd: 0.0,
            mode: SessionMode::TearDown,
            precedent_ids: vec![],
            timestamp: chrono::Utc::now(),
            schema_version: 2,
            tier: "best".into(),
            budget: None,
            context_sources: vec![],
            origin: Default::default(),
            execution_route: Default::default(),
            gateway_sensitivity: None,
            chair_tokens_in: 0,
            chair_tokens_out: 0,
            chair_cost_usd: 0.0,
            chair_provider_provenance: None,
            chair_gateway_provenance: None,
            worker_metrics: None,
            worker_provenance: None,
        };

        let entry = precedent::entry_from_session(&session);

        assert_eq!(entry.schema_version, 2);
        assert_eq!(entry.session_id, "test-123");
        assert_eq!(entry.cabinet, "standard");
        assert_eq!(entry.confidence, "HIGH");
        assert_eq!(entry.convergence, 0.87);
        assert_eq!(entry.seat_count, 2);
        assert_eq!(entry.rounds, 1);
        assert_eq!(entry.synthesis_model, Some("claude-opus-4-8".into()));
        assert_eq!(entry.tier, "best");
        assert_eq!(entry.judge_provider, Some("gpt".into()));
        assert!(entry.failure_status.is_none());
        assert!(entry.cited_by.is_empty());
        assert!(entry.challenged_by.is_empty());
        assert!(!entry.keywords.is_empty());
        assert!(!entry.digest.is_empty());

        // Round-trip through JSON
        let json = serde_json::to_string(&entry).unwrap();
        let rt: PrecedentEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.session_id, entry.session_id);
        assert_eq!(rt.confidence, "HIGH");
    }

    #[test]
    fn flight_record_markdown_keeps_full_synthesis() {
        let long_synthesis = format!(
            "HIGH confidence ruling: {}\n\nFinal action: keep the complete chair output.",
            "full-transcript ".repeat(80)
        );
        let session = CouncilSession {
            session_id: "flight-full".into(),
            parent_request_id: None,
            topic: "Keep the flight recorder complete".into(),
            cabinet_name: "standard".into(),
            rounds: vec![RoundResult {
                round_num: 1,
                responses: vec![SeatResponse {
                    seat_name: "Recorder".into(),
                    provider: "gpt".into(),
                    model: "gpt-5.6-sol".into(),
                    text: "Seat text".into(),
                    ..Default::default()
                }],
                convergence_score: 0.91,
                converged: true,
                judge_provider: Some("gpt".into()),
                judge_assessment: None,
                judge_gateway_attempts: vec![],
                flip_flop_hash: None,
                validation_report: None,
            }],
            synthesis: Some(long_synthesis.clone()),
            synthesis_model: Some("claude".into()),
            total_tokens: 1000,
            total_latency_ms: 5000,
            total_cost_usd: 0.01,
            specops_triggered: false,
            specops_cost_usd: 0.0,
            mode: SessionMode::TearDown,
            precedent_ids: vec![],
            timestamp: chrono::Utc::now(),
            schema_version: 2,
            tier: "best".into(),
            budget: None,
            context_sources: vec![],
            origin: Default::default(),
            execution_route: Default::default(),
            gateway_sensitivity: None,
            chair_tokens_in: 0,
            chair_tokens_out: 0,
            chair_cost_usd: 0.0,
            chair_provider_provenance: None,
            chair_gateway_provenance: None,
            worker_metrics: None,
            worker_provenance: None,
        };

        let md = precedent::flight_record_markdown(&session);

        assert!(md.contains(&long_synthesis));
        assert!(!md.contains("...(truncated)"));
        assert!(md.contains("## Seats (preview-only; full responses and provider metadata are stored in session JSON)"));
        assert!(!md.contains("preview-only summaries"));
    }
}
