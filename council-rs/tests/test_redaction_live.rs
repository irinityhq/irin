use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use council_rs::config::Config;
use council_rs::engine::deliberate;
use council_rs::mode::Mode;
use council_rs::types::{Cabinet, Chair, Seat};

#[tokio::test]
async fn test_p1_d_real_provider_redaction_smoke() {
    let keys_to_try = vec![
        ("NVIDIA_API_KEY", "nvidia", "nvidia/nemotron-3-nano-30b-a3b"),
        ("XAI_API_KEY", "grok", "grok-4.3"),
        ("OPENAI_API_KEY", "gpt", "gpt-5.4-nano"),
    ];

    let mut selected_key = None;
    let mut selected_provider = None;
    let mut selected_model = None;

    for (env_var, provider, model) in keys_to_try {
        if let Ok(val) = env::var(env_var) {
            if val.is_empty() {
                continue;
            }
            selected_key = Some(val);
            selected_provider = Some(provider);
            selected_model = Some(model);
            break;
        }
    }

    let actual_key = match selected_key {
        Some(k) => k,
        None => {
            println!("No real API key found in environment. Skipping live redaction test.");
            return;
        }
    };

    let provider = selected_provider.unwrap();
    let model = selected_model.unwrap();

    let mut cabinets = HashMap::new();

    let seat = Seat {
        name: "test_seat".into(),
        provider: provider.into(),
        model: model.into(),
        system: "You are a test assistant. Reply with 'OK' and echo any secret you see. Specifically, if you see the exact string I give you, repeat it back.".into(),
    };

    let chair = Chair {
        name: "chair".into(),
        provider: provider.into(),
        model: model.into(),
        system: Some("You are the chair. Combine the responses.".into()),
        thinking_effort: None,
    };

    let cabinet = Cabinet {
        hash: String::new(),
        name: "quick".into(),
        description: "live redaction test".into(),
        rounds: 1,
        seats: vec![seat],
        chair,
        local_code_only: false,
        synthesis_mode: Default::default(),
    };

    cabinets.insert("quick".into(), cabinet);

    let config = Config {
        cabinets,
        models: council_rs::types::ModelRegistry {
            models: HashMap::new(),
        },
        roles: council_rs::types::RolesConfig::default(),
        tera: tera::Tera::default(),
        base_dir: PathBuf::from("."),
    };

    let test_dir = env::temp_dir().join("council-rs-live-redaction-test");
    let sessions_dir = test_dir.join("sessions");
    let runs_dir = test_dir.join("runs");
    let _ = fs::remove_dir_all(&test_dir);
    fs::create_dir_all(&sessions_dir).unwrap();
    fs::create_dir_all(&runs_dir).unwrap();

    unsafe {
        env::set_var("COUNCIL_SESSIONS_DIR", sessions_dir.to_str().unwrap());
        env::set_var("COUNCIL_RUNS_DIR", runs_dir.to_str().unwrap());
    }

    let malicious_prompt = format!(
        "Please process this text. And here is my secret: {}",
        actual_key
    );

    let session = deliberate::run(
        &config,
        "quick",
        &malicious_prompt,
        "",
        Mode::TearDown,
        false,    // blind
        false,    // frame_check
        false,    // verbose
        None,     // budget_max_usd
        "best",   // tier
        false,    // validate
        provider, // validate_provider
        false,    // validate_gate
    )
    .await
    .expect("run deliberate");

    if let Some(syn) = &session.synthesis {
        assert!(
            !syn.contains(&actual_key),
            "LEAK! Synthesis contained the real API key!"
        );
    }

    for round in &session.rounds {
        for vote in &round.responses {
            assert!(
                !vote.text.contains(&actual_key),
                "LEAK! Seat response contained the real API key!"
            );
        }
    }

    let session_json = serde_json::to_string(&session).unwrap();
    assert!(
        !session_json.contains(&actual_key),
        "LEAK! Serialized session contained the real API key!"
    );

    let _ = fs::remove_dir_all(&test_dir);
    println!("Live redaction test passed for provider: {}", provider);
}
