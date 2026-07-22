//! POST /api/cabinets/save — validation + atomic write for War Room cabinet
//! drafts (feature contract).
//!
//! Pinned contract:
//!   - `name` must match `^[a-z0-9][a-z0-9_-]{0,63}$` — no slashes, dots, or
//!     traversal; the name doubles as the registry key / file stem.
//!   - `yaml` must parse as a valid `Cabinet` (serde_yaml) before anything
//!     touches disk.
//!   - EMBEDDED (git-tracked) cabinet keys can never be overwritten;
//!     re-saving a previously saved cabinet under the same name is allowed.
//!   - Writes `<base_dir>/cabinets/<name>.yaml` (tmp + rename, atomic).
//!
//! Saved cabinets appear in `GET /api/cabinets` without a restart via
//! `config::scan_cabinets_dir`, and are launched by registry name: the WS
//! streaming engine, `POST /api/deliberate`, and the smoke path all resolve a
//! cabinet through `Config::resolve_cabinet_owned`, which falls back to
//! `<base_dir>/cabinets/<name>.yaml` (parse + canonical hash + the same per-run
//! `validate_cabinet_for_execution` gate) on a registry miss. The startup
//! `Arc<Config>` snapshot stays immutable.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::Cabinet;

/// Monotonic counter making concurrent `write_cabinet_yaml` tmp names unique
/// within a process; combined with the PID it is unique across processes too.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The git-tracked cabinet YAML stems. The registry carries no
/// embedded-vs-saved provenance flag and saved cabinets land in the same
/// directory (loaded identically at next startup), so a hardcoded list is the
/// only strategy consistent with "never overwrite a built-in, but re-saving a
/// saved cabinet is fine". Update when checking in a new cabinet YAML.
pub const EMBEDDED_CABINET_KEYS: &[&str] = &[
    "code-verify",
    "duo",
    "freeride",
    "heritage",
    "quick",
    "reflection",
    "sovereign",
    "standard",
    "triad-architecture",
    "triad-debugging",
    "triad-product",
    "triad-risk",
    "triad-shipping",
    "triad-strategy",
    "triage",
    "trinity",
    "wargame",
    "warroom",
];

/// `^[a-z0-9][a-z0-9_-]{0,63}$` — hand-rolled byte checks (no regex compile on
/// the request path). Rejects traversal ("../x"), separators ("a/b"), hidden
/// stems (".hidden"), uppercase, and empty/oversized names by construction.
pub fn is_valid_cabinet_name(name: &str) -> bool {
    fn head(b: u8) -> bool {
        b.is_ascii_lowercase() || b.is_ascii_digit()
    }
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 || !head(bytes[0]) {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| head(b) || b == b'_' || b == b'-')
}

#[derive(Debug)]
pub enum SaveError {
    InvalidName,
    EmbeddedKey(String),
    InvalidYaml(String),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::InvalidName => {
                write!(f, "name must match ^[a-z0-9][a-z0-9_-]{{0,63}}$")
            }
            SaveError::EmbeddedKey(name) => write!(
                f,
                "'{name}' is a built-in cabinet and cannot be overwritten"
            ),
            SaveError::InvalidYaml(e) => {
                write!(f, "yaml does not parse as a Cabinet: {e}")
            }
        }
    }
}

impl std::error::Error for SaveError {}

/// Validate a save request without touching disk. Returns the parsed cabinet
/// (canonical hash stamped) so the caller can run the full execution
/// validation (`validate_cabinet_for_execution`) before writing.
pub fn validate_save_request(name: &str, yaml: &str) -> Result<Cabinet, SaveError> {
    if !is_valid_cabinet_name(name) {
        return Err(SaveError::InvalidName);
    }
    if EMBEDDED_CABINET_KEYS.contains(&name) {
        return Err(SaveError::EmbeddedKey(name.to_string()));
    }
    crate::config::parse_cabinet_yaml(yaml).map_err(|e| SaveError::InvalidYaml(format!("{e:#}")))
}

/// Write the raw YAML to `<base_dir>/cabinets/<name>.yaml` atomically
/// (tmp write → fsync → rename). The caller must have validated `name` and
/// `yaml` via `validate_save_request` first.
pub fn write_cabinet_yaml(base_dir: &Path, name: &str, yaml: &str) -> std::io::Result<PathBuf> {
    debug_assert!(is_valid_cabinet_name(name), "caller must validate the name");
    let dir = base_dir.join("cabinets");
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(format!("{name}.yaml"));
    // Per-write unique tmp name so concurrent saves of the same cabinet don't
    // clobber each other's tmp file before the atomic rename. PID guards
    // cross-process collisions; the counter guards intra-process ones.
    let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("{name}.yaml.{}.{nonce}.tmp", std::process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(yaml.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_YAML: &str = r#"
name: Test Cabinet
description: saved by test
rounds: 2
seats:
  - name: skeptic
    provider: grok
    model: grok-4
    system: sys
chair:
  name: chair
  provider: gemini
  model: gemini-3.1-pro-preview
"#;

    fn temp_base() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "council_rs_cabinet_save_{}_{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn name_validation_accepts_contract_shapes() {
        let max = "a".repeat(64);
        for good in ["a", "0", "my-cab", "x_1", "0day", "quick2", max.as_str()] {
            assert!(is_valid_cabinet_name(good), "{good:?} should be accepted");
        }
    }

    #[test]
    fn name_validation_rejects_traversal_and_bad_shapes() {
        let over = "a".repeat(65);
        for bad in [
            "",
            "../x",
            "a/b",
            "a\\b",
            ".hidden",
            "Upper",
            "UPPER",
            "-lead",
            "_lead",
            "has space",
            "dot.yaml",
            "café",
            over.as_str(),
        ] {
            assert!(!is_valid_cabinet_name(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn embedded_keys_cannot_be_overwritten() {
        for key in ["standard", "warroom", "triad-strategy", "quick"] {
            assert!(
                matches!(
                    validate_save_request(key, VALID_YAML),
                    Err(SaveError::EmbeddedKey(_))
                ),
                "embedded key {key:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_yaml_that_is_not_a_cabinet() {
        for bad in [
            "rounds: 2",                     // missing name/seats/chair
            "name: x\nrounds: 1\nseats: []", // missing chair
            "{ not yaml ::",                 // not YAML at all
            "- just\n- a\n- list",           // wrong top-level shape
        ] {
            assert!(
                matches!(
                    validate_save_request("my-cab", bad),
                    Err(SaveError::InvalidYaml(_))
                ),
                "{bad:?} should fail Cabinet parse"
            );
        }
    }

    #[test]
    fn valid_request_parses_and_stamps_canonical_hash() {
        let cab = validate_save_request("my-cab", VALID_YAML).expect("valid request");
        assert_eq!(cab.name, "Test Cabinet");
        assert!(!cab.hash.is_empty(), "canonical hash must be stamped");
    }

    /// Pinned roundtrip: save into a temp base_dir, then the live re-scan used
    /// by GET /api/cabinets sees the cabinet (with the same canonical hash),
    /// and re-saving the same non-embedded name is allowed.
    #[test]
    fn save_then_scan_roundtrip_with_temp_base_dir() {
        let base = temp_base();
        let cab = validate_save_request("my-cab", VALID_YAML).unwrap();
        let path = write_cabinet_yaml(&base, "my-cab", VALID_YAML).unwrap();
        assert!(path.ends_with("cabinets/my-cab.yaml"), "path: {path:?}");

        let scanned = crate::config::scan_cabinets_dir(&base);
        let listed = scanned
            .get("my-cab")
            .expect("saved cabinet appears in scan");
        assert_eq!(listed.name, "Test Cabinet");
        assert_eq!(
            listed.hash, cab.hash,
            "re-scan recomputes the same canonical hash"
        );

        // Overwriting a previously saved (non-embedded) cabinet is allowed.
        validate_save_request("my-cab", VALID_YAML).expect("re-save validates");
        write_cabinet_yaml(&base, "my-cab", VALID_YAML).expect("re-save writes");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Pinned save→launch path (feature contract): a cabinet saved after startup is
    /// absent from the immutable startup registry, but `resolve_cabinet_owned`
    /// — the resolver every launch path uses (WS stream, POST /api/deliberate,
    /// smoke) — falls back to the saved YAML on disk and returns a launchable,
    /// canonical-hash-stamped cabinet. Without the fallback the Run flow would
    /// fail with "Unknown cabinet" for anything saved after server start.
    #[test]
    fn save_then_resolve_owned_fallback_is_launchable() {
        // Empty startup registry simulates a cabinet saved after the immutable
        // Arc<Config> snapshot was taken. Vault check is bypassed so the test
        // does not require a live xmcp (matches model_check_blocking tolerance).
        // SAFETY: test-only env mutation; single-threaded within this test.
        unsafe {
            std::env::set_var("COUNCIL_SKIP_VAULT_CHECK", "1");
        }
        let base = temp_base();
        let saved = validate_save_request("after-start", VALID_YAML).unwrap();
        write_cabinet_yaml(&base, "after-start", VALID_YAML).unwrap();

        let config = crate::config::Config {
            cabinets: std::collections::HashMap::new(),
            models: crate::types::ModelRegistry {
                models: std::collections::HashMap::new(),
            },
            roles: crate::types::RolesConfig::default(),
            tera: tera::Tera::default(),
            base_dir: base.clone(),
        };

        // Registry miss → disk fallback resolves the saved cabinet.
        let resolved = config
            .resolve_cabinet_owned("after-start")
            .expect("saved cabinet must resolve via disk fallback");
        assert_eq!(resolved.name, "Test Cabinet");
        assert_eq!(
            resolved.hash, saved.hash,
            "disk fallback stamps the same canonical hash as the save path"
        );

        // A name with no saved file (and not in the registry) still errors.
        assert!(
            config.resolve_cabinet_owned("never-saved").is_err(),
            "unknown cabinet with no file must still fail"
        );

        // Traversal / invalid stems are rejected before touching disk.
        assert!(config.resolve_cabinet_owned("../etc/passwd").is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Optional chair knobs + synthesis_mode round-trip through save → re-scan
    /// so GET /api/cabinets has them to emit (PR review minor #3). The required
    /// fields stay populated and the non-default synthesis_mode survives.
    const YAML_WITH_OPTIONALS: &str = r#"
name: Optional Cabinet
description: exercises optional chair + synthesis fields
rounds: 1
seats:
  - name: skeptic
    provider: grok
    model: grok-4
    system: sys
chair:
  name: chair
  provider: gemini
  model: gemini-3.1-pro-preview
  system: synthesize strictly
  thinking_effort: high
synthesis_mode: directive_proposal_v1
"#;

    #[test]
    fn save_then_scan_preserves_optional_chair_and_synthesis_fields() {
        use crate::types::SynthesisMode;
        let base = temp_base();
        validate_save_request("with-opts", YAML_WITH_OPTIONALS).unwrap();
        write_cabinet_yaml(&base, "with-opts", YAML_WITH_OPTIONALS).unwrap();

        let scanned = crate::config::scan_cabinets_dir(&base);
        let cab = scanned.get("with-opts").expect("scanned");
        assert_eq!(cab.chair.system.as_deref(), Some("synthesize strictly"));
        assert_eq!(cab.chair.thinking_effort.as_deref(), Some("high"));
        assert_eq!(cab.synthesis_mode, SynthesisMode::DirectiveProposalV1);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Malformed neighbours must not break the live listing (the tolerant
    /// scan skips them with a warning).
    #[test]
    fn scan_skips_malformed_files() {
        let base = temp_base();
        write_cabinet_yaml(&base, "good-cab", VALID_YAML).unwrap();
        std::fs::write(base.join("cabinets").join("broken.yaml"), "{ nope ::").unwrap();

        let scanned = crate::config::scan_cabinets_dir(&base);
        assert!(scanned.contains_key("good-cab"));
        assert!(!scanned.contains_key("broken"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
