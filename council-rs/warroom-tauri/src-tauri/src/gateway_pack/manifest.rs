//! Image manifest validation for the Gateway Pack.
//!
//! Production and local-dev modes both require `name@sha256:<64-hex>` for
//! gateway and sidecar. Tag-only refs are refused. Local-dev is explicitly
//! labeled so it cannot be mistaken for releasable GHCR refs.
//!
//! **Digest semantics:**
//! - `local-dev`: manifest digest may match Docker image **config Id** (`{{.Id}}`).
//! - `production`: manifest digest is the **registry manifest digest**; verify via
//!   `RepoDigests` (or equivalent), never by comparing to config Id alone.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Digested image reference: `name@sha256:hex64`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef(String);

impl ImageRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn name(&self) -> &str {
        self.0.split_once('@').map(|(n, _)| n).unwrap_or(&self.0)
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("empty image ref".to_string());
        }
        // Refuse tag-only (no @sha256:).
        let Some((name, digest_part)) = raw.split_once('@') else {
            return Err(format!(
                "tag-only image ref refused (require name@sha256:digest): {raw}"
            ));
        };
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(format!("invalid image name in ref: {raw}"));
        }
        if name.contains('@') {
            return Err(format!("invalid image name in ref: {raw}"));
        }
        let Some(hex) = digest_part.strip_prefix("sha256:") else {
            return Err(format!("image digest must use sha256: prefix: {raw}"));
        };
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("image digest must be 64 hex chars: {raw}"));
        }
        // Lowercase hex for stable comparison.
        let normalized = format!("{}@sha256:{}", name, hex.to_ascii_lowercase());
        Ok(ImageRef(normalized))
    }

    pub fn digest_hex(&self) -> &str {
        self.0.split_once("@sha256:").map(|(_, h)| h).unwrap_or("")
    }

    /// `name@sha256:digest` form for docker pull/inspect.
    pub fn digest_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ManifestMode {
    #[serde(rename = "production")]
    Production,
    #[serde(rename = "local-dev")]
    LocalDev,
}

impl ManifestMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ManifestMode::Production => "production",
            ManifestMode::LocalDev => "local-dev",
        }
    }

    pub fn is_production(&self) -> bool {
        matches!(self, ManifestMode::Production)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchInvariants {
    #[serde(rename = "WATCH_PRODUCER_ENABLED")]
    pub watch_producer_enabled: bool,
    #[serde(rename = "WATCH_DISPATCHER_ENABLED")]
    pub watch_dispatcher_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackImages {
    pub gateway: String,
    pub sidecar: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageManifest {
    pub schema_version: u32,
    pub mode: String,
    pub pack_version: String,
    pub images: PackImages,
    #[serde(default)]
    pub third_party_pins: std::collections::BTreeMap<String, String>,
    pub watch_invariants: WatchInvariants,
    #[serde(default)]
    pub image_ids: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub local_tags: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub source_sha: Option<String>,
    #[serde(default)]
    pub source_dirty: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ValidatedManifest {
    pub mode: ManifestMode,
    pub pack_version: String,
    pub gateway: ImageRef,
    pub sidecar: ImageRef,
    #[allow(dead_code)]
    pub third_party: Vec<(String, ImageRef)>,
    pub source_sha: Option<String>,
    // Carried through validation for provenance display; no reader yet.
    #[allow(dead_code)]
    pub source_dirty: Option<bool>,
    /// Local-dev only: optional config Ids for dual-check.
    pub local_image_ids: std::collections::BTreeMap<String, String>,
}

pub fn load_manifest(path: &Path) -> Result<ImageManifest, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read manifest: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse manifest: {e}"))
}

pub fn validate_manifest(m: &ImageManifest) -> Result<ValidatedManifest, String> {
    if m.schema_version != 1 {
        return Err(format!(
            "unsupported manifest schema_version {}",
            m.schema_version
        ));
    }
    let mode = match m.mode.as_str() {
        "production" => ManifestMode::Production,
        "local-dev" => ManifestMode::LocalDev,
        other => return Err(format!("unknown manifest mode: {other}")),
    };
    if m.pack_version.trim().is_empty() {
        return Err("pack_version is required".to_string());
    }
    if m.watch_invariants.watch_producer_enabled {
        return Err("manifest must set WATCH_PRODUCER_ENABLED=false".to_string());
    }
    if m.watch_invariants.watch_dispatcher_enabled {
        return Err("manifest must set WATCH_DISPATCHER_ENABLED=false".to_string());
    }

    let gateway = ImageRef::parse(&m.images.gateway)?;
    let sidecar = ImageRef::parse(&m.images.sidecar)?;

    // Refuse all-zero placeholder digests for any start path.
    if gateway.digest_hex().chars().all(|c| c == '0')
        || sidecar.digest_hex().chars().all(|c| c == '0')
    {
        return Err(
            "manifest image digests are placeholders; build local images or supply published digests"
                .to_string(),
        );
    }

    // Production must not use local-dev naming and must declare a clean source SHA.
    if mode.is_production() {
        if gateway.as_str().starts_with("irin-desktop/")
            || sidecar.as_str().starts_with("irin-desktop/")
        {
            return Err(
                "production manifest must not use irin-desktop/* local image names".to_string(),
            );
        }
        if m.source_dirty == Some(true) {
            return Err("production manifest must have source_dirty=false".to_string());
        }
        let sha = m
            .source_sha
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "unknown");
        if sha.is_none() {
            return Err("production manifest requires source_sha".to_string());
        }
    }

    let mut third_party = Vec::new();
    for (k, v) in &m.third_party_pins {
        third_party.push((k.clone(), ImageRef::parse(v)?));
    }

    Ok(ValidatedManifest {
        mode,
        pack_version: m.pack_version.clone(),
        gateway,
        sidecar,
        third_party,
        source_sha: m.source_sha.clone(),
        source_dirty: m.source_dirty,
        local_image_ids: m.image_ids.clone(),
    })
}

/// Verify a local docker image **config Id** matches the digest portion of an ImageRef.
/// Valid **only** for `local-dev` mode. Production must use [`repo_digests_match_ref`].
pub fn image_config_id_matches_ref(image_id: &str, image_ref: &ImageRef) -> bool {
    let id_hex = image_id
        .strip_prefix("sha256:")
        .unwrap_or(image_id)
        .to_ascii_lowercase();
    id_hex == image_ref.digest_hex()
}

/// True when any entry in `RepoDigests` (newline or comma separated
/// `name@sha256:hex` lines) matches the expected registry digest reference.
///
/// Production verification must use this (or equivalent manifest digest),
/// **not** config Id equality. A config Id equal to the hex is neither
/// necessary nor sufficient for production.
pub fn repo_digests_match_ref(repo_digests: &str, image_ref: &ImageRef) -> bool {
    let want_hex = image_ref.digest_hex();
    let want_name = image_ref.name();
    for line in repo_digests
        .split(['\n', '\r', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Ok(parsed) = ImageRef::parse(line) {
            if parsed.digest_hex() == want_hex {
                // Name may be a registry path variant; digest match is authoritative.
                // Prefer exact name match when present.
                if parsed.name() == want_name || line.contains(want_hex) {
                    return true;
                }
            }
        } else if let Some((_, dig)) = line.split_once("@sha256:") {
            if dig.to_ascii_lowercase() == want_hex {
                return true;
            }
        }
    }
    false
}

/// Production must refuse comparing a registry digest to a config Id.
/// Returns true only when the probe is valid for the given mode.
#[cfg(test)]
pub fn digest_probe_allowed_for_mode(mode: &ManifestMode, used_config_id: bool) -> bool {
    match mode {
        ManifestMode::LocalDev => true,
        ManifestMode::Production => !used_config_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_tag_only() {
        let err = ImageRef::parse("ghcr.io/org/gw:latest").unwrap_err();
        assert!(err.contains("tag-only"), "{err}");
    }

    #[test]
    fn accepts_digest_ref() {
        let hex = "b".repeat(64);
        let r = ImageRef::parse(&format!("ghcr.io/org/gw@sha256:{hex}")).unwrap();
        assert_eq!(r.digest_hex(), hex);
    }

    #[test]
    fn rejects_short_digest() {
        assert!(ImageRef::parse("n@sha256:abcd").is_err());
    }

    #[test]
    fn validate_watch_off() {
        let mut m = sample_manifest("local-dev");
        m.watch_invariants.watch_producer_enabled = true;
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn validate_placeholder_zero_digest() {
        let mut m = sample_manifest("local-dev");
        m.images.gateway = format!("ghcr.io/x@sha256:{}", "0".repeat(64));
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn production_rejects_local_dev_image_names() {
        let m = sample_manifest("production");
        // sample uses irin-desktop — production must refuse
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn production_requires_source_sha_and_clean() {
        let hex = "c".repeat(64);
        let mut m = ImageManifest {
            schema_version: 1,
            mode: "production".into(),
            pack_version: "0.1.0".into(),
            images: PackImages {
                gateway: format!("ghcr.io/example/gw@sha256:{hex}"),
                sidecar: format!("ghcr.io/example/sc@sha256:{hex}"),
            },
            third_party_pins: Default::default(),
            watch_invariants: WatchInvariants {
                watch_producer_enabled: false,
                watch_dispatcher_enabled: false,
            },
            image_ids: Default::default(),
            local_tags: Default::default(),
            notes: None,
            source_sha: None,
            source_dirty: Some(false),
        };
        assert!(validate_manifest(&m).is_err());
        m.source_sha = Some("abc".into());
        assert!(validate_manifest(&m).is_ok());
        m.source_dirty = Some(true);
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn config_id_match_local_dev_only_helper() {
        let hex = "a".repeat(64);
        let r = ImageRef::parse(&format!("n@sha256:{hex}")).unwrap();
        assert!(image_config_id_matches_ref(&format!("sha256:{hex}"), &r));
        assert!(!image_config_id_matches_ref(
            &format!("sha256:{}", "b".repeat(64)),
            &r
        ));
        assert!(digest_probe_allowed_for_mode(&ManifestMode::LocalDev, true));
        assert!(!digest_probe_allowed_for_mode(
            &ManifestMode::Production,
            true
        ));
        assert!(digest_probe_allowed_for_mode(
            &ManifestMode::Production,
            false
        ));
    }

    #[test]
    fn production_digest_not_satisfied_by_config_id_alone() {
        // Fixture: registry digest D, config Id C (different). Production must
        // use RepoDigests, not C == D.
        let registry_hex = "d".repeat(64);
        let config_hex = "e".repeat(64);
        let image_ref = ImageRef::parse(&format!("ghcr.io/org/gw@sha256:{registry_hex}")).unwrap();
        // Config Id matching registry digest is a false signal for production.
        assert!(image_config_id_matches_ref(
            &format!("sha256:{registry_hex}"),
            &image_ref
        ));
        // Even if config Id matches by coincidence, production probe must not use it.
        assert!(!digest_probe_allowed_for_mode(
            &ManifestMode::Production,
            true
        ));
        // Different config Id is the normal case — config match would fail.
        assert!(!image_config_id_matches_ref(
            &format!("sha256:{config_hex}"),
            &image_ref
        ));
        // RepoDigests containing the registry digest is the correct production check.
        let repos = format!("ghcr.io/org/gw@sha256:{registry_hex}\n");
        assert!(repo_digests_match_ref(&repos, &image_ref));
        // Stale / tag-only style empty RepoDigests fail.
        assert!(!repo_digests_match_ref("", &image_ref));
        assert!(!repo_digests_match_ref(
            &format!("ghcr.io/org/gw@sha256:{config_hex}"),
            &image_ref
        ));
    }

    #[test]
    fn repo_digests_accept_digest_only_match() {
        let hex = "f".repeat(64);
        let r = ImageRef::parse(&format!("ghcr.io/org/gw@sha256:{hex}")).unwrap();
        assert!(repo_digests_match_ref(
            &format!("ghcr.io/org/gw@sha256:{hex}"),
            &r
        ));
        assert!(repo_digests_match_ref(
            &format!("ghcr.io/org/gw:latest@sha256:{hex}"),
            &r
        ));
    }

    fn sample_manifest(mode: &str) -> ImageManifest {
        let hex = "c".repeat(64);
        ImageManifest {
            schema_version: 1,
            mode: mode.into(),
            pack_version: "0.1.0-test".into(),
            images: PackImages {
                gateway: format!("irin-desktop/gateway@sha256:{hex}"),
                sidecar: format!("irin-desktop/sidecar@sha256:{hex}"),
            },
            third_party_pins: Default::default(),
            watch_invariants: WatchInvariants {
                watch_producer_enabled: false,
                watch_dispatcher_enabled: false,
            },
            image_ids: Default::default(),
            local_tags: Default::default(),
            notes: None,
            source_sha: Some("deadbeef".into()),
            source_dirty: Some(false),
        }
    }
}
