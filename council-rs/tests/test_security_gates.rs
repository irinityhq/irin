use std::fs;
use std::path::Path;

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
    // Fail-closed: expected sources must exist and be readable. Silent skip
    // on missing files made this gate a permanent green no-op.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Bind logic lives in server/mod.rs since server.rs became a module tree.
    let server_path = manifest.join("src/server/mod.rs");
    let content = fs::read_to_string(&server_path).unwrap_or_else(|e| {
        panic!(
            "must read server bind source {}: {e}",
            server_path.display()
        )
    });
    assert!(
        !content.contains("\"0.0.0.0\""),
        "server/mod.rs contains 0.0.0.0 bind!"
    );
    assert!(
        content.contains("127.0.0.1"),
        "server/mod.rs must explicitly bind to 127.0.0.1"
    );

    // Sidecar bind/serve lives in boot.rs (main.rs no longer owns the listener).
    let sidecar_path = manifest.join("../gateway/sidecar-rs/src/boot.rs");
    let content = fs::read_to_string(&sidecar_path).unwrap_or_else(|e| {
        panic!(
            "must read sidecar bind source {}: {e}",
            sidecar_path.display()
        )
    });
    assert!(
        !content.contains("\"0.0.0.0\""),
        "sidecar-rs contains 0.0.0.0 bind!"
    );
    assert!(
        content.contains("127.0.0.1") || content.contains("uds") || content.contains("UDS"),
        "sidecar-rs must bind to 127.0.0.1 or UDS"
    );
}

/// Lint short-form `ports:` entries for loopback-only host binds.
///
/// Only lines under a YAML `ports:` key are checked. Volume mounts, env
/// strings, and `extra_hosts` entries also match naive `- "...:..."` patterns
/// and must not be treated as port mappings.
fn assert_compose_ports_loopback(content: &str, rel: &str) {
    let mut in_ports = false;
    let mut ports_indent: Option<usize> = None;
    let mut mappings = 0usize;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();

        if trimmed == "ports:" || trimmed.starts_with("ports:") {
            in_ports = true;
            ports_indent = Some(indent);
            continue;
        }

        if !in_ports {
            continue;
        }

        let pi = ports_indent.expect("ports indent set while in_ports");
        if indent <= pi {
            // Left the ports block (next sibling or parent key).
            in_ports = false;
            ports_indent = None;
            // Current line may open another block; re-check next iteration only.
            // If this line itself is another ports: it will be handled next loop —
            // fall through by re-evaluating as non-ports unless it is ports.
            if trimmed == "ports:" || trimmed.starts_with("ports:") {
                in_ports = true;
                ports_indent = Some(indent);
            }
            continue;
        }

        // Short-form publish: - "ip:host:container" or - "host:container"
        if trimmed.starts_with("- \"") && trimmed.ends_with('"') {
            assert!(
                trimmed.starts_with("- \"127.0.0.1:"),
                "docker-compose port mapping not bound to localhost in {}: {}",
                rel,
                line
            );
            mappings += 1;
        }
    }

    assert!(
        mappings > 0,
        "compose file {rel} defines no short-form ports: entries to lint"
    );
}

#[test]
fn test_p1_b3_compose_lint_loopback_only() {
    // Fail-closed: expected compose files must exist. Prior paths
    // (../docker-compose.yml, ../../docker-compose.yml) never matched the
    // monorepo layout, so if-let Ok skipped every assertion and the test
    // stayed green forever.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let compose_paths = [
        "gateway/docker-compose.yml",
        "gateway/docker-compose.demo.yml",
        "packaging/gateway-pack/docker-compose.yml",
    ];

    for rel in compose_paths {
        let path = repo.join(rel);
        let content = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "expected compose file missing or unreadable at {}: {e}",
                path.display()
            )
        });
        assert_compose_ports_loopback(&content, rel);
    }
}
