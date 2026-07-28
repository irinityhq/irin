//! Pack file install, integrity verification, and image presence checks.

use super::keys::write_atomic_0600;
use super::manifest::{
    image_config_id_matches_ref, load_manifest, repo_digests_match_ref, validate_manifest,
    ManifestMode, ValidatedManifest,
};
use super::paths::{
    bundled_pack_root, ensure_gateway_dir, gateway_data_dir, installed_marker_path,
    is_pack_installed, PACK_DIR_NAME,
};
use crate::docker_cli::{
    docker_command, format_cmd_failure, DockerErrorKind, DESKTOP_COMPOSE_PROJECT,
    DOCKER_CMD_TIMEOUT,
};
use std::fs;
use std::path::{Path, PathBuf};

pub fn install_pack_files() -> Result<PathBuf, String> {
    let src = bundled_pack_root().ok_or_else(|| {
        "Gateway Pack is not bundled in this app. Rebuild the DMG with stage-gateway-pack.sh."
            .to_string()
    })?;
    let gw = ensure_gateway_dir()?;
    let final_dest = gw.join(PACK_DIR_NAME);
    let stage = gw.join(format!(".pack-stage-{}.tmp", std::process::id()));
    let backup = gw.join(format!(".pack-backup-{}.tmp", std::process::id()));

    // Clean leftover stage dirs.
    let _ = fs::remove_dir_all(&stage);
    let _ = fs::remove_dir_all(&backup);

    copy_dir_recursive(&src, &stage).map_err(|e| format!("stage pack files: {e}"))?;

    // Validate complete assets before swap.
    let compose = stage.join("docker-compose.yml");
    let manifest_path = stage.join("image-manifest.json");
    if !compose.is_file() {
        let _ = fs::remove_dir_all(&stage);
        return Err("staged pack missing docker-compose.yml".to_string());
    }
    if !manifest_path.is_file() {
        let _ = fs::remove_dir_all(&stage);
        return Err("staged pack missing image-manifest.json".to_string());
    }
    let validated = {
        let m = load_manifest(&manifest_path).inspect_err(|_| {
            let _ = fs::remove_dir_all(&stage);
        })?;
        validate_manifest(&m).inspect_err(|_| {
            let _ = fs::remove_dir_all(&stage);
        })?
    };
    // Require nginx + conf + lua for a complete pack.
    for rel in ["nginx.conf", "conf", "lua"] {
        let p = stage.join(rel);
        if !p.exists() {
            let _ = fs::remove_dir_all(&stage);
            return Err(format!("staged pack missing {rel}"));
        }
    }

    // Atomic swap: final -> backup, stage -> final, drop backup.
    if final_dest.exists() {
        fs::rename(&final_dest, &backup).map_err(|e| {
            let _ = fs::remove_dir_all(&stage);
            format!("pack swap backup failed: {e}")
        })?;
    }
    if let Err(e) = fs::rename(&stage, &final_dest) {
        // Roll back.
        if backup.exists() {
            let _ = fs::rename(&backup, &final_dest);
        }
        let _ = fs::remove_dir_all(&stage);
        return Err(format!("pack swap failed: {e}"));
    }
    let _ = fs::remove_dir_all(&backup);

    let marker = serde_json::json!({
        "installed": true,
        "pack_version": validated.pack_version,
        "manifest_mode": validated.mode.as_str(),
        "project": DESKTOP_COMPOSE_PROJECT,
        "source_sha": validated.source_sha,
        "asset_hashes": pack_asset_hashes(&final_dest)?,
    });
    write_atomic_0600(&installed_marker_path(), format!("{marker}\n").as_bytes())?;
    Ok(final_dest)
}

/// sha256 (hex) of one file, for pack asset integrity records.
pub(crate) fn sha256_hex_file(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let bytes = fs::read(path).map_err(|e| format!("hash {}: {e}", path.display()))?;
    Ok(format!("{:x}", sha2::Sha256::digest(&bytes)))
}

/// Recursive relpath → sha256 map over the installed pack tree (files only,
/// sorted for determinism). The pack tree is small and app-owned.
pub(crate) fn pack_asset_hashes(root: &Path) -> Result<serde_json::Value, String> {
    let mut out = serde_json::Map::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            fs::read_dir(&dir).map_err(|e| format!("read pack dir {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read pack dir entry: {e}"))?;
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned();
            let meta = entry.metadata().map_err(|e| format!("stat {rel}: {e}"))?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                out.insert(rel, serde_json::Value::String(sha256_hex_file(&path)?));
            }
        }
    }
    Ok(serde_json::Value::Object(out))
}

/// Re-verify the installed pack tree against the install marker's recorded
/// hashes. On any mismatch, re-stage once from the bundled assets (the pack
/// tree is user-writable; the bundle is the code-signed source of truth) and
/// re-verify. Persistent mismatch fails closed: no secret-bearing spawn.
pub(crate) fn verify_pack_asset_integrity(pack_root: &Path) -> Result<(), String> {
    fn current_matches(root: &Path) -> Result<bool, String> {
        let raw = fs::read_to_string(installed_marker_path())
            .map_err(|e| format!("read install marker: {e}"))?;
        let marker: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("parse install marker: {e}"))?;
        let Some(recorded) = marker.get("asset_hashes").cloned() else {
            return Ok(false);
        };
        Ok(pack_asset_hashes(root)? == recorded)
    }
    if current_matches(pack_root)? {
        return Ok(());
    }
    eprintln!("[gateway-pack] asset integrity mismatch; re-staging from bundle");
    let staged = install_pack_files()?;
    if staged != pack_root {
        return Err("re-staged pack root does not match the compose path".to_string());
    }
    if current_matches(pack_root)? {
        Ok(())
    } else {
        Err("pack asset integrity still mismatched after re-stage".to_string())
    }
}

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ty.is_file() {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Installed pack root only — never falls back to bundled Resources as "installed".
pub fn installed_pack_root() -> Option<PathBuf> {
    if !is_pack_installed() {
        return None;
    }
    let p = gateway_data_dir().join(PACK_DIR_NAME);
    if p.join("docker-compose.yml").is_file() {
        Some(p)
    } else {
        None
    }
}

pub(crate) fn load_validated_manifest(pack_root: &Path) -> Result<ValidatedManifest, String> {
    let path = pack_root.join("image-manifest.json");
    if !path.is_file() {
        return Err("image-manifest.json missing from Gateway Pack".to_string());
    }
    let m = load_manifest(&path)?;
    validate_manifest(&m)
}

pub(crate) fn verify_images_present(v: &ValidatedManifest) -> Result<(), String> {
    match v.mode {
        ManifestMode::LocalDev => verify_images_local_dev(v),
        ManifestMode::Production => verify_images_production(v),
    }
}

pub(crate) fn verify_images_local_dev(v: &ValidatedManifest) -> Result<(), String> {
    for (label, image_ref) in [("gateway", &v.gateway), ("sidecar", &v.sidecar)] {
        let out = docker_command(&[
            "image",
            "inspect",
            "--format",
            "{{.Id}}",
            image_ref.as_str(),
        ]);
        match out {
            Ok(o) if o.status.success() => {
                let id = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !image_config_id_matches_ref(&id, image_ref) {
                    // Also accept local_tags / image_ids from manifest.
                    if let Some(expected_id) = v.local_image_ids.get(label) {
                        if image_config_id_matches_ref(&id, image_ref)
                            || id == *expected_id
                            || id.strip_prefix("sha256:") == expected_id.strip_prefix("sha256:")
                        {
                            continue;
                        }
                    }
                    return Err(format!(
                        "{label}: {}",
                        DockerErrorKind::ImageDigestMismatch.operator_message()
                    ));
                }
            }
            Ok(o) => {
                let id_ref = format!("sha256:{}", image_ref.digest_hex());
                let out2 = docker_command(&["image", "inspect", "--format", "{{.Id}}", &id_ref]);
                match out2 {
                    Ok(o2) if o2.status.success() => {
                        let id = String::from_utf8_lossy(&o2.stdout).trim().to_string();
                        if !image_config_id_matches_ref(&id, image_ref) {
                            return Err(format!(
                                "{label}: {}",
                                DockerErrorKind::ImageDigestMismatch.operator_message()
                            ));
                        }
                    }
                    _ => {
                        return Err(format!(
                            "{label} image not present for local-dev. \
                             Run scripts/build-gateway-pack-dev-images.sh. {}",
                            format_cmd_failure("image inspect", &o)
                        ));
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub(crate) fn verify_images_production(v: &ValidatedManifest) -> Result<(), String> {
    for (label, image_ref) in [("gateway", &v.gateway), ("sidecar", &v.sidecar)] {
        // Pull/resolve exact name@sha256 registry digest.
        let pull = docker_command(&["pull", image_ref.digest_ref()]);
        match pull {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                return Err(format!(
                    "{label} production pull failed. {}",
                    format_cmd_failure("image pull", &o)
                ));
            }
            Err(e) => return Err(e),
        }

        let out = docker_command(&[
            "image",
            "inspect",
            "--format",
            "{{json .RepoDigests}}",
            image_ref.digest_ref(),
        ]);
        match out {
            Ok(o) if o.status.success() => {
                let raw = String::from_utf8_lossy(&o.stdout);
                // RepoDigests JSON array → join as lines for matcher.
                let digests = raw
                    .trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .replace('"', "")
                    .replace(',', "\n");
                if !repo_digests_match_ref(&digests, image_ref) {
                    // Also try newline format from Go template alternative.
                    let out2 = docker_command(&[
                        "image",
                        "inspect",
                        "--format",
                        "{{range .RepoDigests}}{{println .}}{{end}}",
                        image_ref.digest_ref(),
                    ]);
                    let digests2 = out2
                        .ok()
                        .map(|o2| String::from_utf8_lossy(&o2.stdout).to_string())
                        .unwrap_or_default();
                    if !repo_digests_match_ref(&digests2, image_ref)
                        && !repo_digests_match_ref(&raw, image_ref)
                    {
                        return Err(format!(
                            "{label}: production RepoDigests do not contain expected registry digest \
                             (config Id matching is not accepted in production mode)"
                        ));
                    }
                }
            }
            Ok(o) => {
                return Err(format!(
                    "{label}: {}",
                    format_cmd_failure("image inspect RepoDigests", &o)
                ));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub(crate) fn compose_file(pack_root: &Path) -> PathBuf {
    pack_root.join("docker-compose.yml")
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
