//! Mirror of council_stream.fork_session — build a fork-ready cabinet config.
//!
//! Caller posts /api/sessions/{id}/fork with seat swaps, gets back a payload
//! that can be fed into /ws/deliberate via `custom_cabinet`.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::Config;
use crate::precedent;

/// Serialize the cabinet matching what /api/cabinets returns (includes seats + chair).
fn cabinet_to_api_json(cab: &crate::types::Cabinet, registry_key: &str, is_triad: bool) -> Value {
    json!({
        "name": registry_key,
        "label": cab.name,
        "description": cab.description,
        "rounds": cab.rounds,
        "seats": cab.seats.iter().map(|s| json!({
            "name": s.name,
            "provider": s.provider,
            "model": s.model,
            "system": s.system,
        })).collect::<Vec<_>>(),
        "chair": {
            "name": cab.chair.name,
            "provider": cab.chair.provider,
            "model": cab.chair.model,
        },
        "is_triad": is_triad,
    })
}

/// Shared with the `GET /api/cabinets` handler (feature contract) — registry keys with
/// the `triad-` prefix render in the frontend's "Domain Triads" group.
pub(crate) fn is_triad_registry_key(key: &str) -> bool {
    key.starts_with("triad-")
}

/// Build a forked cabinet from a parent session_id with seat swaps applied.
/// Returns either {topic, cabinet, parent_id, parent_cabinet_key, parent_cabinet_label, swaps_applied}
/// or {error: "..."}.
pub fn fork_session(config: &Arc<Config>, session_id: &str, swaps: &[Value]) -> Value {
    let parent = match precedent::load_session(session_id) {
        Some(s) => s,
        None => return json!({"error": format!("session {} not found", session_id)}),
    };
    // feature contract parity: cabinets saved via POST /api/cabinets/save after server
    // start exist on disk but not in the startup snapshot — scan so their
    // sessions stay forkable, matching resolve_cabinet_owned's launch behavior.
    let scanned = crate::config::scan_cabinets_dir(&config.base_dir);
    fork_payload(config, &scanned, &parent, session_id, swaps)
}

/// Pure wire-shape builder — no filesystem access (the disk scan is supplied
/// by the caller). Split out of `fork_session` so the fork response shape can
/// be tested hermetically against a synthetic parent session (never the
/// operator's live `sessions/` directory).
fn fork_payload(
    config: &Config,
    scanned: &std::collections::HashMap<String, crate::types::Cabinet>,
    parent: &crate::types::CouncilSession,
    session_id: &str,
    swaps: &[Value],
) -> Value {
    let label = parent.cabinet_name.clone();

    // Find the cabinet by display name (cab.name), recording the registry key.
    // Startup snapshot first (embedded + boot-time YAML), then the live disk
    // scan for cabinets saved after server start.
    let mut cabinet_opt: Option<crate::types::Cabinet> = None;
    let mut parent_cabinet_key = String::new();
    let mut is_triad = false;
    for (key, cab) in config.cabinets.iter() {
        if cab.name == label {
            parent_cabinet_key = key.clone();
            is_triad = is_triad_registry_key(key);
            cabinet_opt = Some(cab.clone());
            break;
        }
    }
    if cabinet_opt.is_none() {
        for (key, cab) in scanned.iter() {
            if cab.name == label {
                parent_cabinet_key = key.clone();
                is_triad = is_triad_registry_key(key);
                cabinet_opt = Some(cab.clone());
                break;
            }
        }
    }
    let mut cabinet = match cabinet_opt {
        Some(c) => c,
        None => {
            return json!({
                "error": format!("cabinet '{}' not reproducible (custom)", label)
            });
        }
    };

    // Apply swaps; record before/after.
    let mut applied: Vec<Value> = Vec::new();
    for seat in cabinet.seats.iter_mut() {
        let swap = swaps
            .iter()
            .find(|s| s.get("seat_name").and_then(|x| x.as_str()) == Some(seat.name.as_str()));
        let swap = match swap {
            Some(s) => s,
            None => continue,
        };
        let before = json!({
            "provider": seat.provider,
            "model": seat.model,
            "system": seat.system,
        });
        if let Some(p) = swap.get("provider").and_then(|x| x.as_str()) {
            seat.provider = p.to_string();
        }
        if let Some(m) = swap.get("model").and_then(|x| x.as_str()) {
            seat.model = m.to_string();
        }
        if let Some(sy) = swap.get("system").and_then(|x| x.as_str()) {
            seat.system = sy.to_string();
        }
        applied.push(json!({
            "seat_name": seat.name,
            "before": before,
            "after": {
                "provider": seat.provider,
                "model": seat.model,
                "system": seat.system,
            },
        }));
    }
    cabinet.name = format!("{} (fork of {})", label, session_id);

    json!({
        "topic": parent.topic,
        "cabinet": cabinet_to_api_json(&cabinet, &parent_cabinet_key, is_triad),
        "parent_id": session_id,
        "parent_cabinet_key": parent_cabinet_key,
        "parent_cabinet_label": label,
        "swaps_applied": applied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Cabinet, Chair, Seat};

    fn sample_cabinet() -> Cabinet {
        Cabinet {
            hash: String::new(),
            name: "Standard Council".into(),
            description: "test".into(),
            rounds: 2,
            seats: vec![Seat {
                name: "skeptic".into(),
                provider: "grok".into(),
                model: "grok-4".into(),
                system: "sys".into(),
            }],
            chair: Chair {
                name: "chair".into(),
                provider: "gemini".into(),
                model: "gemini-3".into(),
                system: None,
                thinking_effort: None,
            },
            local_code_only: false,
            synthesis_mode: Default::default(),
        }
    }

    #[test]
    fn cabinet_to_api_json_includes_registry_key_label_and_triad() {
        let cab = sample_cabinet();
        let v = cabinet_to_api_json(&cab, "standard", false);
        assert_eq!(v["name"], "standard");
        assert_eq!(v["label"], "Standard Council");
        assert_eq!(v["is_triad"], false);
        assert_eq!(v["seats"][0]["name"], "skeptic");
    }

    #[test]
    fn is_triad_registry_key_detects_triad_prefix() {
        assert!(is_triad_registry_key("triad-strategy"));
        assert!(!is_triad_registry_key("standard"));
    }

    /// Build a synthetic parent session via serde so `#[serde(default)]`
    /// fills the long tail of fields — no live sessions/ dir involved.
    fn synthetic_parent(cabinet_name: &str) -> crate::types::CouncilSession {
        serde_json::from_value(json!({
            "session_id": "council_test_0001",
            "topic": "Synthetic fork parent",
            "cabinet_name": cabinet_name,
            "rounds": [],
            "timestamp": "2026-01-01T00:00:00Z",
        }))
        .expect("synthetic CouncilSession deserializes")
    }

    #[test]
    fn fork_response_shape_includes_parent_cabinet_key_field() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        let parent = synthetic_parent("Standard Council");
        let seat_name = config.cabinets["standard"].seats[0].name.clone();
        let swaps = vec![json!({"seat_name": seat_name, "model": "test-model"})];

        let result = fork_payload(
            &config,
            &std::collections::HashMap::new(),
            &parent,
            "council_test_0001",
            &swaps,
        );

        assert!(
            result.get("error").is_none(),
            "fork failed: {:?}",
            result.get("error")
        );
        assert_eq!(result["parent_cabinet_key"], "standard");
        assert_eq!(result["parent_cabinet_label"], "Standard Council");
        assert_eq!(result["parent_id"], "council_test_0001");
        let cabinet = &result["cabinet"];
        assert_eq!(cabinet["name"], "standard");
        assert_eq!(
            cabinet["label"],
            "Standard Council (fork of council_test_0001)"
        );
        assert_eq!(cabinet["is_triad"], false);
        assert_eq!(result["swaps_applied"][0]["seat_name"], json!(seat_name));
        assert_eq!(result["swaps_applied"][0]["after"]["model"], "test-model");
    }

    #[test]
    fn fork_payload_rejects_unreproducible_custom_cabinet() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        // Fork-of-fork: persisted cabinet_name is a provenance label, not a
        // registry display name — must surface the "not reproducible" error.
        let parent = synthetic_parent("War Room (fork of council_test_0001)");
        let result = fork_payload(
            &config,
            &std::collections::HashMap::new(),
            &parent,
            "council_test_0002",
            &[],
        );
        let err = result["error"].as_str().expect("error string");
        assert!(err.contains("not reproducible"), "unexpected error: {err}");
    }

    #[test]
    fn fork_payload_finds_post_boot_saved_cabinet_via_scan() {
        let config = Config::load(std::path::Path::new(".")).unwrap();
        // Cabinet absent from the startup snapshot but present in the
        // caller-supplied disk scan (feature contract save-after-boot parity).
        let saved: crate::types::Cabinet = serde_yaml::from_str(
            "name: Post Boot Cabinet\ndescription: saved after boot\nrounds: 1\nseats:\n  - name: Solo\n    provider: grok\n    model: grok-4.3\n    system: test\nchair:\n  name: Chair\n  provider: grok\n  model: grok-4.3\n",
        )
        .unwrap();
        let mut scanned = std::collections::HashMap::new();
        scanned.insert("post-boot-cabinet".to_string(), saved);
        let parent = synthetic_parent("Post Boot Cabinet");
        let result = fork_payload(&config, &scanned, &parent, "council_test_0003", &[]);
        assert!(
            result.get("error").is_none(),
            "fork failed: {:?}",
            result.get("error")
        );
        assert_eq!(result["parent_cabinet_key"], "post-boot-cabinet");
        assert_eq!(result["cabinet"]["name"], "post-boot-cabinet");
    }
}
