//! Live health / auth probes used by status and enable paths.

use super::install::{compose_file, installed_pack_root, load_validated_manifest};
use super::manifest::ValidatedManifest;
use crate::docker_cli::{
    docker_command, format_cmd_failure, DESKTOP_COMPOSE_PROJECT, DESKTOP_GATEWAY_URL,
    DOCKER_CMD_TIMEOUT,
};
use crate::keychain::{is_valid_gw_raw_key, store_gw_api_key, SecretStore};
use crate::private_config::{load_or_create_private_config, write_private_config_at};
use std::path::Path;
use std::time::Duration;

pub(crate) fn http_get_status(url: &str, bearer: Option<&str>) -> Result<(u16, String), String> {
    let mut req = ureq::get(url).timeout(Duration::from_secs(10));
    if let Some(token) = bearer {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    match req.call() {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.into_string().unwrap_or_default();
            Ok((status, body))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Ok((code, body))
        }
        Err(e) => Err(format!("request failed: {e}")),
    }
}

pub(crate) fn gateway_health_ok() -> bool {
    matches!(
        http_get_status(&format!("{DESKTOP_GATEWAY_URL}/health"), None),
        Ok((200, _))
    )
}

pub(crate) fn admin_surface_ready() -> bool {
    match ureq::post(&format!("{DESKTOP_GATEWAY_URL}/admin/keys"))
        .timeout(Duration::from_secs(3))
        .set("Content-Type", "application/json")
        .send_string("{}")
    {
        Ok(resp) => {
            let s = resp.status();
            s != 502 && s != 503 && s != 504
        }
        Err(ureq::Error::Status(code, _)) => code != 502 && code != 503 && code != 504,
        Err(_) => false,
    }
}

/// True only when the running desktop project's containers were created from
/// the validated manifest's pinned image refs. A lookalike project — even
/// with our name and compose path — running a different image never counts
/// as ours, so the Keychain-held client key can only reach our own images.
pub(crate) fn desktop_project_images_match(validated: &ValidatedManifest) -> bool {
    let out = docker_command(&[
        "ps",
        "--filter",
        &format!("label=com.docker.compose.project={DESKTOP_COMPOSE_PROJECT}"),
        "-q",
    ]);
    let Ok(out) = out else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let ids: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if ids.is_empty() {
        return false;
    }
    // Compare resolved image IDs, not Config.Image strings: a retagged local
    // name can string-match while pointing at different bytes. The pinned
    // refs must resolve locally, or nothing is proven.
    let mut expected_ids: Vec<String> = Vec::new();
    for reference in [validated.gateway.as_str(), validated.sidecar.as_str()] {
        match docker_command(&["image", "inspect", "-f", "{{.Id}}", reference]) {
            Ok(o) if o.status.success() => {
                let id = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if id.is_empty() {
                    return false;
                }
                expected_ids.push(id);
            }
            _ => return false,
        }
    }
    ids.iter().all(|id| {
        let img = docker_command(&["inspect", "-f", "{{.Image}}", id]);
        match img {
            Ok(o) if o.status.success() => {
                let got = String::from_utf8_lossy(&o.stdout).trim().to_string();
                expected_ids.contains(&got)
            }
            _ => false,
        }
    })
}

pub(crate) fn models_authenticated(raw_key: &str) -> bool {
    // The Keychain-held client key is sent only to the app-owned Gateway:
    // owned project (name + our compose file) AND our validated images,
    // failing closed on any unproven layer.
    if !desktop_project_running() {
        return false;
    }
    let Some(pack_root) = installed_pack_root() else {
        return false;
    };
    let Ok(validated) = load_validated_manifest(&pack_root) else {
        return false;
    };
    if !desktop_project_images_match(&validated) {
        return false;
    }
    matches!(
        http_get_status(&format!("{DESKTOP_GATEWAY_URL}/v1/models"), Some(raw_key)),
        Ok((200, _))
    )
}

fn models_status_is_fail_closed_without_key(result: Result<(u16, String), String>) -> bool {
    matches!(result, Ok((401 | 403, _)))
}

pub(crate) fn models_fail_closed_without_key() -> bool {
    models_status_is_fail_closed_without_key(http_get_status(
        &format!("{DESKTOP_GATEWAY_URL}/v1/models"),
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::models_status_is_fail_closed_without_key;

    #[test]
    fn models_fail_closed_without_key_maps_transport_error_to_false() {
        assert!(!models_status_is_fail_closed_without_key(Err(
            "unreachable".to_string()
        )));
        assert!(models_status_is_fail_closed_without_key(Ok((
            401,
            String::new()
        ))));
        assert!(models_status_is_fail_closed_without_key(Ok((
            403,
            String::new()
        ))));
        assert!(!models_status_is_fail_closed_without_key(Ok((
            200,
            String::new()
        ))));
    }
}

/// Provision Council service-role client via real admin API. Raw key → Keychain only.
/// `bootstrap` is held only in memory / compose process env for this call.
pub(crate) fn provision_council_client(
    store: &dyn SecretStore,
    bootstrap: &str,
) -> Result<String, String> {
    let body = serde_json::json!({
        "budget_key": "desktop-council",
        "tier": "default",
        "rpm": 600,
        "service_role": "council",
        "admin_key": bootstrap,
    });
    let resp = ureq::post(&format!("{DESKTOP_GATEWAY_URL}/admin/keys"))
        .timeout(Duration::from_secs(15))
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| format!("provision request failed: {e}"))?;
    if resp.status() != 200 {
        return Err(format!("provision rejected with HTTP {}", resp.status()));
    }
    let value: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("provision response json: {e}"))?;
    let raw_key = value
        .get("raw_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "provision response missing raw_key".to_string())?;
    if !is_valid_gw_raw_key(raw_key) {
        return Err("provision response raw_key has invalid shape".to_string());
    }
    let key_id = value
        .get("key_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "provision response missing key_id".to_string())?;
    if !key_id.starts_with("k_") || key_id.len() != 10 {
        return Err("provision response key_id has invalid shape".to_string());
    }

    store_gw_api_key(store, raw_key)?;
    let mut cfg = load_or_create_private_config()?;
    cfg.gateway_key_id = Some(key_id.to_string());
    write_private_config_at(&crate::private_config::private_config_path(), &cfg)?;

    let _ = raw_key;
    Ok(key_id.to_string())
}

/// True only when Docker reports the fixed desktop compose project running.
///
/// Uses daemon-global `docker compose ls` with an exact project-name match.
/// The old `docker compose -p <project> ps` ran without `-f`, so compose
/// resolved its configuration from the process cwd — a Finder-launched app
/// has no compose file there, misread its own running project as absent
/// (degraded status), and the port check then misclassified the app's own
/// Gateway as foreign. `compose ls` needs no compose configuration at all.
pub(crate) fn desktop_project_running() -> bool {
    // Ownership must be proven against OUR installed compose file; without an
    // installed pack root there is nothing to prove against, so fail closed.
    let expected =
        installed_pack_root().map(|root| compose_file(&root).to_string_lossy().into_owned());
    let Some(expected) = expected else {
        return false;
    };
    let out = docker_command(&[
        "compose",
        "ls",
        "--filter",
        &format!("name={DESKTOP_COMPOSE_PROJECT}"),
        "--format",
        "json",
    ]);
    match out {
        Ok(o) if o.status.success() => {
            compose_ls_reports_running(&String::from_utf8_lossy(&o.stdout), &expected)
        }
        _ => false,
    }
}

/// Parse `docker compose ls --format json` output: true only when an entry's
/// `Name` is exactly the fixed desktop project, its `Status` is running, and
/// its `ConfigFiles` points at OUR installed pack compose file. A lookalike
/// project — even one with the same name — never counts as ours, so the
/// Keychain-held client key can only go to the app-owned listener.
pub(crate) fn compose_ls_reports_running(json: &str, expected_config: &str) -> bool {
    let parsed: serde_json::Value = match serde_json::from_str(json.trim()) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let Some(projects) = parsed.as_array() else {
        return false;
    };
    // Docker may canonicalize ConfigFiles differently than our path (e.g.
    // /var vs /private/var); compare canonical forms with a raw fallback.
    let expected_canon = std::fs::canonicalize(expected_config)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| expected_config.to_string());
    projects.iter().any(|p| {
        p.get("Name").and_then(|n| n.as_str()) == Some(DESKTOP_COMPOSE_PROJECT)
            && p.get("Status")
                .and_then(|s| s.as_str())
                .map(|s| s.starts_with("running"))
                .unwrap_or(false)
            && p.get("ConfigFiles")
                .and_then(|s| s.as_str())
                .map(|s| {
                    s.split(',').any(|f| {
                        let f = f.trim();
                        f == expected_config
                            || std::fs::canonicalize(f)
                                .map(|c| c.to_string_lossy().into_owned())
                                .unwrap_or_else(|_| f.to_string())
                                == expected_canon
                    })
                })
                .unwrap_or(false)
    })
}
