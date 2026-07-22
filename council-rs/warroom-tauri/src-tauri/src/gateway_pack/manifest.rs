//! Image manifest validation for the Gateway Pack.
//!
//! Production and local-dev modes both require `name@sha256:<64-hex>` for
//! gateway and sidecar. Tag-only refs are refused. Local-dev is explicitly
//! labeled so it cannot be mistaken for releasable GHCR refs.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Digested image reference: `name@sha256:hex64`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef(String);

impl ImageRef {
    pub fn as_str(&self) -> &str {
        &self.0
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
}

#[derive(Debug, Clone)]
pub struct ValidatedManifest {
    pub mode: ManifestMode,
    pub pack_version: String,
    pub gateway: ImageRef,
    pub sidecar: ImageRef,
    #[allow(dead_code)]
    pub third_party: Vec<(String, ImageRef)>,
    #[allow(dead_code)]
    pub source_sha: Option<String>,
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
    })
}

/// Verify a local docker image id matches the digest portion of an ImageRef.
/// `image_id` is `sha256:hex` from `docker image inspect`.
pub fn image_id_matches_ref(image_id: &str, image_ref: &ImageRef) -> bool {
    let id_hex = image_id
        .strip_prefix("sha256:")
        .unwrap_or(image_id)
        .to_ascii_lowercase();
    id_hex == image_ref.digest_hex()
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
        let mut m = sample_manifest();
        m.watch_invariants.watch_producer_enabled = true;
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn validate_placeholder_zero_digest() {
        let mut m = sample_manifest();
        m.images.gateway = format!("ghcr.io/x@sha256:{}", "0".repeat(64));
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn id_match() {
        let hex = "a".repeat(64);
        let r = ImageRef::parse(&format!("n@sha256:{hex}")).unwrap();
        assert!(image_id_matches_ref(&format!("sha256:{hex}"), &r));
        assert!(!image_id_matches_ref(&format!("sha256:{}", "b".repeat(64)), &r));
    }

    fn sample_manifest() -> ImageManifest {
        let hex = "c".repeat(64);
        ImageManifest {
            schema_version: 1,
            mode: "local-dev".into(),
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
            source_sha: None,
        }
    }
}
