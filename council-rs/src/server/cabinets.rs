// Cabinet listing + save handlers (moved from server.rs).

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::warroom;

/// GET /api/cabinets — full shape matching frontend CabinetEditor expectations.
///
/// feature contract: re-scans `<base_dir>/cabinets/` per request so cabinets saved via
/// POST /api/cabinets/save appear without a restart (the startup `Arc<Config>`
/// stays immutable). Saved cabinets launch by registry name: every launch path
/// resolves through `Config::resolve_cabinet_owned`, which falls back to the
/// saved YAML on a registry miss and runs the same per-run validation gate.
/// Falls back to the startup snapshot when the scan comes back empty (e.g. dir
/// unreadable).
pub(super) async fn cabinets(State(state): State<AppState>) -> impl IntoResponse {
    let scanned = crate::config::scan_cabinets_dir(&state.config.base_dir);
    let live = if scanned.is_empty() {
        &state.config.cabinets
    } else {
        &scanned
    };
    let cabs: Vec<serde_json::Value> = live
        .iter()
        .map(|(key, cab)| {
            // Chair: always name/provider/model; include the optional system and
            // thinking_effort only when set so the GET shape stays wire-minimal
            // and round-trips Chair's `#[serde(default)] Option<String>` fields.
            let mut chair = serde_json::Map::new();
            chair.insert("name".into(), json!(cab.chair.name));
            chair.insert("provider".into(), json!(cab.chair.provider));
            chair.insert("model".into(), json!(cab.chair.model));
            if let Some(system) = &cab.chair.system {
                chair.insert("system".into(), json!(system));
                let p = state
                    .config
                    .base_dir
                    .join("prompts")
                    .join(format!("{}.tera", system));
                if let Ok(src) = std::fs::read_to_string(&p) {
                    chair.insert("system_source".into(), json!(src));
                }
            }
            if let Some(effort) = &cab.chair.thinking_effort {
                chair.insert("thinking_effort".into(), json!(effort));
            }

            let mut obj = serde_json::Map::new();
            obj.insert("name".into(), json!(key));
            obj.insert("label".into(), json!(cab.name));
            // Truncate to first line for picker/UI display (full spec is long for triage etc.)
            let short_desc = cab
                .description
                .lines()
                .next()
                .unwrap_or(&cab.description)
                .trim()
                .to_string();
            obj.insert("description".into(), json!(short_desc));
            obj.insert("rounds".into(), json!(cab.rounds));
            obj.insert(
                "seats".into(),
                json!(
                    cab.seats
                        .iter()
                        .map(|s| {
                            let mut seat = serde_json::Map::new();
                            seat.insert("name".into(), json!(s.name));
                            seat.insert("provider".into(), json!(s.provider));
                            seat.insert("model".into(), json!(s.model));
                            let sys = &s.system;
                            seat.insert("system".into(), json!(sys));
                            if !sys.trim().is_empty() {
                                let p = state
                                    .config
                                    .base_dir
                                    .join("prompts")
                                    .join(format!("{}.tera", sys));
                                if let Ok(src) = std::fs::read_to_string(&p) {
                                    seat.insert("system_source".into(), json!(src));
                                }
                            }
                            serde_json::Value::Object(seat)
                        })
                        .collect::<Vec<_>>()
                ),
            );
            obj.insert("chair".into(), serde_json::Value::Object(chair));
            obj.insert(
                "is_triad".into(),
                json!(warroom::fork::is_triad_registry_key(key)),
            );
            obj.insert("local_code_only".into(), json!(cab.local_code_only));
            // Skip the serde default (Generic) so the wire stays back-compatible
            // with older clients that never saw the field.
            if cab.synthesis_mode != crate::types::SynthesisMode::default() {
                obj.insert(
                    "synthesis_mode".into(),
                    serde_json::to_value(&cab.synthesis_mode).unwrap_or(json!("generic")),
                );
            }
            serde_json::Value::Object(obj)
        })
        .collect();

    axum::Json(json!({ "cabinets": cabs }))
}

/// POST /api/cabinets/save — persist a War Room cabinet draft to
/// `<base_dir>/cabinets/<name>.yaml` (feature contract).
///
/// Body: `{"name": string, "yaml": string}`. Auth: covered by the router-wide
/// `auth_middleware` layer — same posture as the other mutating routes
/// (embeddings/rebuild, drift/run, precedent/reindex).
/// Responses: 200 `{"ok": true, "name", "path"}` | 4xx/5xx `{"error": ...}`.
pub(super) async fn cabinets_save_handler(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<CabinetSaveRequest>,
) -> Response {
    use warroom::cabinets_save::{self, SaveError};

    let cabinet = match cabinets_save::validate_save_request(&req.name, &req.yaml) {
        Ok(c) => c,
        Err(e) => {
            let status = match e {
                SaveError::EmbeddedKey(_) => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            return (status, axum::Json(json!({ "error": e.to_string() }))).into_response();
        }
    };

    // Full execution validation (structural + xmcp vault) before the write —
    // the same gate the WS custom_cabinet path runs per-run.
    // `model_check_blocking` is a synchronous network call, so offload per the
    // spawn_blocking convention used by precedent_reindex / embeddings_rebuild.
    let config = state.config.clone();
    let name_for_validation = req.name.clone();
    let validated = tokio::task::spawn_blocking(move || {
        config.validate_cabinet_for_save(&name_for_validation, &cabinet)
    })
    .await;
    match validated {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": format!("{e:#}") })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": format!("validation join failed: {e}") })),
            )
                .into_response();
        }
    }

    match cabinets_save::write_cabinet_yaml(&state.config.base_dir, &req.name, &req.yaml) {
        Ok(path) => axum::Json(json!({
            "ok": true,
            "name": req.name,
            "path": path.display().to_string(),
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": format!("write failed: {e}") })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct CabinetSaveRequest {
    name: String,
    yaml: String,
}
