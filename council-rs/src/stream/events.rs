//! Stream event types — exact wire-compatible shapes with council_stream.py.
//!
//! EVENT TYPES (StreamEvent.type):
//!     session_started       — initial config + active/dropped seats
//!     precedent_loaded      — prior rulings matched
//!     round_started         — round_num, total_rounds
//!     seat_started          — seat is dispatched
//!     seat_chunk            — streaming text delta from a streaming-capable provider
//!     seat_complete         — seat returned (full text + cost + latency)
//!     convergence_scored    — judge ran, score returned
//!     round_divergence      — per-seat 2D PCA projection of seat embeddings (N02)
//!     round_complete        — round wrapped, includes early-convergence flag
//!     awaiting_input        — pause point
//!     intervention_received — client sent an action
//!     specops_started       — Grok swarm dispatched
//!     specops_signal        — swarm verdict
//!     synthesis_started     — Chair is thinking
//!     synthesis_complete    — final ruling text
//!     session_saved         — transcript written
//!     budget_paused         — cost cap hit; deliberation ending early
//!     phase_started         — multi-phase WS run (e.g. Pathfind → Tear-down)
//!     done                  — totals + final synthesis
//!     error                 — non-fatal warning OR fatal abort

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::ProviderProvenance;

/// A single stream event — serialized to JSON and sent over WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub session_id: String,
    pub ts: String,
    pub data: Value,
}

impl StreamEvent {
    pub fn new(event_type: &str, session_id: &str, data: Value) -> Self {
        Self {
            event_type: event_type.to_string(),
            session_id: session_id.to_string(),
            ts: Utc::now().to_rfc3339(),
            data,
        }
    }

    /// Convenience constructors for each event type
    pub fn session_started(session_id: &str, data: Value) -> Self {
        Self::new("session_started", session_id, data)
    }

    pub fn precedent_loaded(session_id: &str, matches: Vec<Value>) -> Self {
        Self::new(
            "precedent_loaded",
            session_id,
            serde_json::json!({ "matches": matches }),
        )
    }

    pub fn round_started(session_id: &str, round_num: u32, total_rounds: u32) -> Self {
        Self::new(
            "round_started",
            session_id,
            serde_json::json!({
                "round_num": round_num,
                "total_rounds": total_rounds,
            }),
        )
    }

    pub fn seat_started(
        session_id: &str,
        round_num: u32,
        seat_name: &str,
        provider: &str,
        model: &str,
    ) -> Self {
        Self::new(
            "seat_started",
            session_id,
            serde_json::json!({
                "round_num": round_num,
                "seat_name": seat_name,
                "provider": provider,
                "model": model,
            }),
        )
    }

    /// Token-streaming delta for one seat (N01). Emitted between `seat_started`
    /// and `seat_complete` for streaming-capable providers. `seat_complete.text`
    /// stays authoritative — the UI replaces accumulated chunks with it. `seq`
    /// is a monotonic per-seat counter (mpsc preserves order; seq is defensive).
    pub fn seat_chunk(
        session_id: &str,
        round_num: u32,
        seat_name: &str,
        text_delta: &str,
        seq: u32,
    ) -> Self {
        Self::new(
            "seat_chunk",
            session_id,
            serde_json::json!({
                "round_num": round_num,
                "seat_name": seat_name,
                "text_delta": text_delta,
                "seq": seq,
            }),
        )
    }

    pub fn seat_complete(session_id: &str, data: Value) -> Self {
        Self::new("seat_complete", session_id, data)
    }

    pub fn convergence_scored(
        session_id: &str,
        round_num: u32,
        score: f64,
        converged: bool,
    ) -> Self {
        Self::new(
            "convergence_scored",
            session_id,
            serde_json::json!({
                "round_num": round_num,
                "score": score,
                "converged": converged,
            }),
        )
    }

    /// Per-seat divergence projection for one round (N02). Emitted after
    /// `convergence_scored` when seat embeddings are available. `method` is
    /// always `"pca"` (hand-rolled 2-component PCA — UMAP has no mature Rust
    /// crate, so we label the method truthfully). `points` carries one
    /// `{seat, x, y}` per active seat. The event is omitted entirely when
    /// embeddings are unavailable — the UI tolerates absence.
    pub fn round_divergence(
        session_id: &str,
        round_num: u32,
        points: Vec<crate::warroom::divergence::DivergencePoint>,
    ) -> Self {
        Self::new(
            "round_divergence",
            session_id,
            serde_json::json!({
                "round_num": round_num,
                "method": "pca",
                "points": points,
            }),
        )
    }

    pub fn round_complete(
        session_id: &str,
        round_num: u32,
        score: f64,
        converged: bool,
        early: bool,
    ) -> Self {
        Self::new(
            "round_complete",
            session_id,
            serde_json::json!({
                "round_num": round_num,
                "score": score,
                "converged": converged,
                "early_convergence": early,
            }),
        )
    }

    pub fn awaiting_input(
        session_id: &str,
        round_num: u32,
        score: f64,
        converged: bool,
        options: &[&str],
        specops_signal: Option<&str>,
    ) -> Self {
        let mut data = serde_json::json!({
            "round_num": round_num,
            "convergence": score,
            "converged": converged,
            "options": options,
        });
        if let Some(sig) = specops_signal {
            data["specops_signal"] = serde_json::json!(sig);
        }
        Self::new("awaiting_input", session_id, data)
    }

    pub fn intervention_received(session_id: &str, action: Value) -> Self {
        Self::new("intervention_received", session_id, action)
    }

    pub fn specops_started(session_id: &str, trigger: &str, mode: &str) -> Self {
        Self::new(
            "specops_started",
            session_id,
            serde_json::json!({
                "trigger": trigger,
                "mode": mode,
            }),
        )
    }

    pub fn specops_signal(session_id: &str, data: Value) -> Self {
        Self::new("specops_signal", session_id, data)
    }

    pub fn synthesis_started(session_id: &str, model: &str) -> Self {
        Self::new(
            "synthesis_started",
            session_id,
            serde_json::json!({
                "model": model,
            }),
        )
    }

    pub fn synthesis_complete(
        session_id: &str,
        text: &str,
        model: &str,
        latency_ms: u64,
        cost_usd: f64,
        provider_provenance: Option<ProviderProvenance>,
    ) -> Self {
        Self::new(
            "synthesis_complete",
            session_id,
            serde_json::json!({
                "text": text,
                "model": model,
                "latency_ms": latency_ms,
                "cost_usd": cost_usd,
                "provider_provenance": provider_provenance,
            }),
        )
    }

    pub fn session_saved(session_id: &str, path: &str) -> Self {
        Self::new(
            "session_saved",
            session_id,
            serde_json::json!({
                "path": path,
                "session_id": session_id,
            }),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn done(
        session_id: &str,
        tokens: u32,
        cost: f64,
        latency: u64,
        synthesis: &str,
        convergence: f64,
        rounds: u32,
        extra: Option<serde_json::Value>,
    ) -> Self {
        let mut data = serde_json::json!({
            "total_tokens": tokens,
            "total_cost_usd": cost,
            "total_latency_ms": latency,
            "synthesis": synthesis,
            "session_id": session_id,
            "convergence_final": convergence,
            "rounds_run": rounds,
        });
        if let Some(extra) = extra
            && let Some(obj) = data.as_object_mut()
            && let Some(extra_obj) = extra.as_object()
        {
            for (k, v) in extra_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
        Self::new("done", session_id, data)
    }

    pub fn error(session_id: &str, message: &str, fatal: bool) -> Self {
        Self::new(
            "error",
            session_id,
            serde_json::json!({
                "message": message,
                "fatal": fatal,
            }),
        )
    }

    pub fn info(session_id: &str, message: &str) -> Self {
        Self::new(
            "info",
            session_id,
            serde_json::json!({
                "message": message,
            }),
        )
    }

    pub fn budget_paused(
        session_id: &str,
        round_num: u32,
        total_cost_usd: f64,
        max_usd: f64,
    ) -> Self {
        Self::new(
            "budget_paused",
            session_id,
            serde_json::json!({
                "round_num": round_num,
                "total_cost_usd": total_cost_usd,
                "max_usd": max_usd,
                "action": "end_early",
            }),
        )
    }

    pub fn phase_started(
        session_id: &str,
        phase: u32,
        label: &str,
        parent_session_id: &str,
    ) -> Self {
        Self::new(
            "phase_started",
            session_id,
            serde_json::json!({
                "phase": phase,
                "label": label,
                "parent_session_id": parent_session_id,
            }),
        )
    }

    pub fn round_validation(
        session_id: &str,
        round_num: u32,
        gate_applied: bool,
        verdicts: &serde_json::Value,
    ) -> Self {
        Self::new(
            "round_validation",
            session_id,
            serde_json::json!({
                "round_num": round_num,
                "gate_applied": gate_applied,
                "verdicts": verdicts,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedent_loaded_wraps_matches_array() {
        let event = StreamEvent::precedent_loaded(
            "sess-1",
            vec![serde_json::json!({
                "id": "council_20260101_abc",
                "ts": "2026-01-01T00:00:00Z",
                "topic": "Auth architecture",
                "cabinet": "standard",
                "keywords": ["auth"],
                "ruling_digest": "digest",
                "confidence": "high",
                "convergence": 0.9,
                "mode": "teardown",
            })],
        );
        assert_eq!(event.event_type, "precedent_loaded");
        assert_eq!(event.data["matches"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(event.data["matches"][0]["id"], "council_20260101_abc");
    }

    #[test]
    fn seat_chunk_has_pinned_wire_shape() {
        let event = StreamEvent::seat_chunk("sess-1", 2, "Hawk", "delta text", 3);
        assert_eq!(event.event_type, "seat_chunk");
        assert_eq!(event.session_id, "sess-1");
        assert_eq!(event.data["round_num"], 2);
        assert_eq!(event.data["seat_name"], "Hawk");
        assert_eq!(event.data["text_delta"], "delta text");
        assert_eq!(event.data["seq"], 3);
        // Exactly the four pinned keys — no extras.
        assert_eq!(event.data.as_object().map(|o| o.len()), Some(4));
    }

    #[test]
    fn round_divergence_has_pinned_wire_shape() {
        use crate::warroom::divergence::DivergencePoint;
        let event = StreamEvent::round_divergence(
            "sess-1",
            2,
            vec![
                DivergencePoint {
                    seat: "Hawk".into(),
                    x: 0.5,
                    y: -0.25,
                },
                DivergencePoint {
                    seat: "Dove".into(),
                    x: -0.5,
                    y: 0.25,
                },
            ],
        );
        assert_eq!(event.event_type, "round_divergence");
        assert_eq!(event.session_id, "sess-1");
        assert_eq!(event.data["round_num"], 2);
        assert_eq!(event.data["method"], "pca");
        let points = event.data["points"].as_array().unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0]["seat"], "Hawk");
        assert_eq!(points[0]["x"], 0.5);
        assert_eq!(points[0]["y"], -0.25);
        // Exactly the three pinned data keys.
        assert_eq!(event.data.as_object().map(|o| o.len()), Some(3));
    }

    #[test]
    fn synthesis_complete_includes_provider_provenance() {
        let event = StreamEvent::synthesis_complete(
            "session-1",
            "done",
            "codex-cli-gpt-5.6-sol",
            42,
            0.0,
            Some(ProviderProvenance::cli_readonly(
                "codex_cli",
                "usage_unavailable",
            )),
        );

        assert_eq!(event.event_type, "synthesis_complete");
        assert_eq!(
            event.data["provider_provenance"]["access_mode"],
            "cli_agent_readonly"
        );
        assert_eq!(event.data["provider_provenance"]["filesystem"], "read_only");
    }
}
