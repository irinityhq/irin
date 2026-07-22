use std::collections::HashSet;
use std::fs;

use serde_yaml::Value;

fn nvidia_models(path: &str) -> Vec<String> {
    let yaml = fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
    let cabinet: Value =
        serde_yaml::from_str(&yaml).unwrap_or_else(|e| panic!("failed to parse {path}: {e}"));

    let mut models = Vec::new();
    if let Some(seats) = cabinet.get("seats").and_then(Value::as_sequence) {
        for seat in seats {
            if seat.get("provider").and_then(Value::as_str) == Some("nvidia") {
                models.push(
                    seat.get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| panic!("nvidia seat without model in {path}"))
                        .to_string(),
                );
            }
        }
    }
    if let Some(chair) = cabinet.get("chair")
        && chair.get("provider").and_then(Value::as_str) == Some("nvidia")
    {
        models.push(
            chair
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("nvidia chair without model in {path}"))
                .to_string(),
        );
    }
    models
}

#[test]
fn courtesy_cabinet_nvidia_models_are_invokable_allowlisted() {
    let allowlist: HashSet<String> = fs::read_to_string("config/nim-invokable-allowlist.txt")
        .expect("failed to read NIM invokable allowlist")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect();

    for cabinet in [
        "cabinets/starter-nvidia.yaml",
        "cabinets/freeride.yaml",
        "cabinets/sovereign.yaml",
    ] {
        let models = nvidia_models(cabinet);
        assert!(!models.is_empty(), "no NVIDIA models found in {cabinet}");
        for model in models {
            assert!(
                allowlist.contains(&model),
                "{cabinet} uses NVIDIA model {model} outside config/nim-invokable-allowlist.txt"
            );
        }
    }
}
