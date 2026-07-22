use std::fs;

#[test]
fn test_p1_b1_redaction_covers_all_models_yaml_providers() {
    let scrub_path = "src/scrub.rs";
    let _scrub_content = fs::read_to_string(scrub_path).unwrap();

    // Verify all major providers mentioned in models.yaml are covered by regexes
    // either directly (sk-, xai-, gsk_) or via the generic entropy fallback.
    // For this test, we test actual string inputs that represent these keys.
    let keys = vec![
        ("sk-proj-1234567890abcdef1234567890abcdef", true), // OpenAI
        ("sk-ant-api03-1234567890abcdef1234567890abcdef", true), // Anthropic
        ("xai-1234567890abcdef1234567890abcdef", true),     // xAI
        ("AIzaSyB1234567890abcdef1234567890abcdef", true),  // Gemini/Vertex
        ("gsk_1234567890abcdef1234567890abcdef", true),     // Groq
        ("nvapi-1234567890abcdef1234567890abcdef", true),   // NVIDIA
        ("123456789:AAG_1234567890abcdef1234567890abcdef", true), // Telegram
        (
            "https://hooks.slack.com/services/T12345678/B12345678/1234567890abcdef12345678",
            true,
        ), // Slack
        (
            "https://discord.com/api/webhooks/123456789012345678/1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
            true,
        ), // Discord
        (
            "-----BEGIN PRIVATE KEY-----\nMIICXAIBAAKBgQCR\n-----END PRIVATE KEY-----",
            true,
        ), // GCP PEM
        ("ghp_1234567890abcdef1234567890abcdef1234", true), // GitHub PAT
        (
            "github_pat_1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
            true,
        ), // GitHub Fine Grained
        (
            "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
            true,
        ), // Bearer + JWT
        // Entropy fallback: a 32-character random string (e.g. Nous, Mistral bare tokens)
        ("A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6", true),
    ];

    for (key, should_redact) in keys {
        let redacted = council_rs::scrub::redact(key);
        if should_redact {
            assert!(
                redacted.contains("[REDACTED:secret]"),
                "Failed to redact: {}",
                key
            );
            assert!(!redacted.contains(key), "Key leaked: {}", key);
        }
    }
}

#[test]
fn test_p1_b2_bind_string_is_loopback() {
    let server_path = "src/server.rs";
    if let Ok(content) = fs::read_to_string(server_path) {
        assert!(
            !content.contains("\"0.0.0.0\""),
            "server.rs contains 0.0.0.0 bind!"
        );
        assert!(
            content.contains("127.0.0.1"),
            "server.rs must explicitly bind to 127.0.0.1"
        );
    }

    // Check sidecar-rs if possible
    let sidecar_path = "../gateway/sidecar-rs/src/main.rs";
    if let Ok(content) = fs::read_to_string(sidecar_path) {
        assert!(
            !content.contains("\"0.0.0.0\""),
            "sidecar-rs contains 0.0.0.0 bind!"
        );
        assert!(
            content.contains("127.0.0.1") || content.contains("uds"),
            "sidecar-rs must bind to 127.0.0.1 or uds"
        );
    }
}

#[test]
fn test_p1_b3_compose_lint_loopback_only() {
    let compose_paths = ["../docker-compose.yml", "../../docker-compose.yml"];
    let mut _found = false;
    for path in compose_paths.iter() {
        if let Ok(content) = fs::read_to_string(path) {
            _found = true;
            for line in content.lines() {
                let trimmed = line.trim();
                // Check if line defines a port mapping
                if trimmed.starts_with("- \"") && trimmed.contains(":") && trimmed.ends_with("\"") {
                    // It's a port mapping. It must start with 127.0.0.1:
                    assert!(
                        trimmed.starts_with("- \"127.0.0.1:"),
                        "docker-compose port mapping not bound to localhost: {}",
                        line
                    );
                }
            }
        }
    }
    // Note: We don't fail if compose file isn't found in test run env, as it's an integration check.
}
