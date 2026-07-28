// WebSocket deliberate handler + smoke seat events (moved from server.rs).

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::AppState;
use crate::provider;
use crate::stream::deliberate::{self, StreamConfig};
use crate::stream::events::StreamEvent;
use crate::stream::intervention::{Intervention, InterventionQueue};
use crate::warroom;

pub(super) async fn handle_ws(socket: WebSocket, state: AppState) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Wait for the start message (first message must be {type: "start", payload: {...}})
    let start_msg = match ws_rx.next().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => v,
            Err(_) => {
                let err = StreamEvent::error("", "Config not JSON", true);
                let _ = ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&err).unwrap_or_default().into(),
                    ))
                    .await;
                return;
            }
        },
        _ => return,
    };

    if start_msg.get("type").and_then(|v| v.as_str()) != Some("start") {
        let err = StreamEvent::error("", "Expected first message {type:'start'}", true);
        let _ = ws_tx
            .send(Message::Text(
                serde_json::to_string(&err).unwrap_or_default().into(),
            ))
            .await;
        return;
    }

    let payload = start_msg.get("payload").cloned().unwrap_or(json!({}));
    let ws_session_id = uuid::Uuid::new_v4().to_string()[..12].to_string();

    let parsed =
        match super::knobs::parse_ws_start_fields(&payload, super::health::ws_smoke_only_enabled())
        {
            Ok(p) => p,
            Err(e) => {
                let err = StreamEvent::error(&ws_session_id, &e, true);
                let _ = ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&err).unwrap_or_default().into(),
                    ))
                    .await;
                return;
            }
        };
    let mut fields = parsed.fields;
    if parsed.coerce_then_tear_down {
        let _ = ws_tx
            .send(Message::Text(
                serde_json::to_string(&StreamEvent::info(
                    &ws_session_id,
                    "then_tear_down requires pathfind — mode coerced to pathfind",
                ))
                .unwrap_or_default()
                .into(),
            ))
            .await;
    }

    if let Some(map_dir) = payload
        .get("map_dir")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        match warroom::safe_map::gather_map_context_for_deliberation(map_dir) {
            Ok(map_context) => {
                if !fields.context.is_empty() {
                    fields.context.push_str("\n\n---\n\n");
                }
                fields.context.push_str(&map_context);
            }
            Err(e) => {
                let err = StreamEvent::error(&ws_session_id, &format!("map_dir: {e}"), true);
                let _ = ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&err).unwrap_or_default().into(),
                    ))
                    .await;
                return;
            }
        }
    }

    if fields.smoke_only {
        let session_id = "smoke-session";
        let smoke_via_gateway = fields
            .via_gateway
            .unwrap_or_else(provider::default_via_gateway);
        let smoke_sensitivity = fields
            .sensitivity
            .clone()
            .unwrap_or_else(provider::default_sensitivity);

        // Direct-fire synthetic single-shot (feature contract): canned synthesis, zero
        // provider spend, no disk writes. Mirrors the pinned real sequence:
        // session_started → synthesis_started → synthesis_complete →
        // session_saved → done.
        if let Some(ref slug) = fields.direct_fire {
            let Some(spec) = crate::engine::direct_fire::spec(slug) else {
                // Unreachable — parse_ws_start_fields rejected unknown modes.
                let err = StreamEvent::error(
                    session_id,
                    &format!("Unknown direct_fire mode: {slug}"),
                    true,
                );
                let _ = ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&err).unwrap_or_default().into(),
                    ))
                    .await;
                return;
            };
            // Smoke shim: synthetic availability (the required provider is "available"
            // for canned direct-fire; mirrors the "do not filter" logic in the
            // non-direct-fire smoke branch below to avoid starving smoke paths
            // when real CLIs/APIs are absent in the smoke env).
            let available: Vec<(&'static str, bool)> = vec![(spec.provider, true)];
            let canned = format!(
                "[smoke] {} direct-fire synthesis for: {}",
                spec.display, fields.topic
            );
            let events = [
                StreamEvent::session_started(
                    session_id,
                    deliberate::direct_fire_session_started_data(
                        &fields.topic,
                        spec,
                        &available,
                        &fields.tier,
                        smoke_via_gateway,
                        &smoke_sensitivity,
                    ),
                ),
                StreamEvent::synthesis_started(session_id, spec.model),
                StreamEvent::synthesis_complete(session_id, &canned, spec.model, 0, 0.0, None),
                // Canned path — smoke mode never writes a session file.
                StreamEvent::session_saved(session_id, "sessions/smoke-session.json"),
                StreamEvent::done(
                    session_id,
                    0,
                    0.0,
                    0,
                    &canned,
                    1.0,
                    0,
                    Some(json!({ "direct_fire": slug })),
                ),
            ];
            for event in events {
                if ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&event).unwrap_or_default().into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            return;
        }

        // resolve_cabinet_owned (feature contract): smoke session_started must see
        // cabinets saved after startup (disk fallback on a registry miss) so the
        // save→launch path is exercised end-to-end without a paid round.
        match state.config.resolve_cabinet_owned(&fields.cabinet_name) {
            Ok(cabinet) => {
                let available = provider::check_providers_with_gateway(smoke_via_gateway);
                // Synthetic shim: every seat streams regardless of provider
                // reachability (no real calls are made), so all cabinet seats are
                // active and none are dropped. Filtering by availability here would
                // empty the seat loop on a provider-less host and starve the
                // seat_chunk contract (phase9 N01).
                let active_seats = cabinet
                    .seats
                    .iter()
                    .map(|seat| {
                        json!({
                            "name": seat.name,
                            "provider": seat.provider,
                            "model": seat.model,
                        })
                    })
                    .collect::<Vec<_>>();
                let dropped_seats: Vec<serde_json::Value> = Vec::new();
                let rounds_planned =
                    super::knobs::clamp_ws_max_rounds(fields.max_rounds, cabinet.rounds);
                let event = StreamEvent::session_started(
                    session_id,
                    json!({
                        "topic": fields.topic,
                        "cabinet_name": cabinet.name,
                        "rounds_planned": rounds_planned,
                        "mode": if fields.blind { "blind" } else { "normal" },
                        "active_seats": active_seats,
                        "dropped_seats": dropped_seats,
                        "chair": {
                            "provider": &cabinet.chair.provider,
                            "model": &cabinet.chair.model,
                        },
                        "available_providers": available
                            .iter()
                            .filter(|(_, ok)| *ok)
                            .map(|(name, _)| name)
                            .collect::<Vec<_>>(),
                        "council_version": env!("CARGO_PKG_VERSION"),
                        "stream_version": "rs-1.0.0",
                        "tier": fields.tier,
                        "then_tear_down": fields.then_tear_down,
                        "budget_max_usd": fields.budget_max_usd,
                        "auto_specops_threshold": fields.auto_specops_threshold,
                        // feature contract: also emitted by the real path
                        // (src/stream/deliberate.rs session_started) — keep in sync.
                        "via_gateway": smoke_via_gateway,
                        "execution_route": if smoke_via_gateway { "governed" } else { "direct" },
                        "sensitivity": smoke_sensitivity.to_lowercase(),
                    }),
                );
                if ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&event).unwrap_or_default().into(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }

                // N01 smoke: synthetic seat/round/seat_complete/done loop with
                // THREE seat_chunk frames per seat (zero provider spend, no disk
                // writes). The real path lives in stream/deliberate.rs; this
                // mirrors its event ordering so the UI exercises chunk handling
                // without a paid round. Streaming-capable detection is irrelevant
                // here — smoke always emits synthetic chunks.
                let smoke_seats: Vec<(String, String, String)> = cabinet
                    .seats
                    .iter()
                    .map(|seat| (seat.name.clone(), seat.provider.clone(), seat.model.clone()))
                    .collect();
                let smoke_events = build_smoke_seat_events(
                    session_id,
                    rounds_planned.max(1),
                    &smoke_seats,
                    &cabinet.chair.provider,
                    &cabinet.chair.model,
                    &fields.topic,
                );
                for event in smoke_events {
                    if ws_tx
                        .send(Message::Text(
                            serde_json::to_string(&event).unwrap_or_default().into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Err(e) => {
                let err =
                    StreamEvent::error(session_id, &format!("Cabinet load failed: {}", e), true);
                let _ = ws_tx
                    .send(Message::Text(
                        serde_json::to_string(&err).unwrap_or_default().into(),
                    ))
                    .await;
            }
        }
        return;
    }

    // resolve_cabinet_owned (feature contract): clamp against the real cabinet's round
    // count even for cabinets saved after startup (disk fallback on miss).
    let cabinet_rounds = state
        .config
        .resolve_cabinet_owned(&fields.cabinet_name)
        .map(|c| c.rounds)
        .unwrap_or(super::knobs::WS_MAX_ROUNDS_CAP);
    let max_rounds = fields
        .max_rounds
        .map(|r| super::knobs::clamp_ws_max_rounds(Some(r), cabinet_rounds));

    let stream_config = StreamConfig {
        topic: fields.topic,
        cabinet_name: fields.cabinet_name,
        custom_cabinet: fields.custom_cabinet,
        context: fields.context,
        mode: fields.mode,
        blind: fields.blind,
        frame_check: fields.frame_check,
        scope_auditor: fields.scope_auditor,
        max_rounds,
        pause_after_each_round: fields.pause_after_each_round,
        auto_specops_threshold: fields.auto_specops_threshold,
        parent_session_id: fields.parent_session_id,
        swaps: fields.swaps,
        validate: fields.validate,
        validate_provider: fields.validate_provider,
        validate_gate: fields.validate_gate,
        worker_provenance: fields.worker_provenance,
        budget_max_usd: fields.budget_max_usd,
        tier: fields.tier,
        then_tear_down: fields.then_tear_down,
        via_gateway: fields.via_gateway,
        sensitivity: fields.sensitivity,
        direct_fire: fields.direct_fire,
    };

    // Channels
    let interventions = InterventionQueue::new();
    let intervention_sender = interventions.sender();
    let (event_tx, mut event_rx) = mpsc::channel::<StreamEvent>(64);
    let cancel = CancellationToken::new();

    // Spawn the deliberation loop, but retain ownership so a disconnected
    // browser cannot leave provider work detached in the background.
    let config = state.config.clone();
    let run_cancel = cancel.clone();
    let run_handle = tokio::spawn(async move {
        deliberate::run(config, stream_config, event_tx, interventions, run_cancel).await;
    });

    // Spawn the intake loop (client → server interventions)
    let intake_cancel = cancel.clone();
    let intake_handle = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            if let Message::Text(text) = msg
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                && v.get("type").and_then(|t| t.as_str()) == Some("intervention")
                && let Some(payload) = v.get("payload")
                && let Some(action) = Intervention::from_value(payload)
            {
                let _ = intervention_sender.send(action).await;
            }
        }
        // Close frame, receive error, or EOF: stop the run promptly.
        intake_cancel.cancel();
    });

    // Forward events from deliberation → WebSocket
    loop {
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            event = event_rx.recv() => match event {
                Some(event) => event,
                None => break,
            },
        };
        let json = match serde_json::to_string(&event) {
            Ok(j) => j,
            Err(_) => continue,
        };
        if ws_tx.send(Message::Text(json.into())).await.is_err() {
            cancel.cancel();
            break; // Client disconnected
        }
    }

    cancel.cancel();
    intake_handle.abort();
    let _ = intake_handle.await;
    cancel_and_join_ws_run(cancel, run_handle, std::time::Duration::from_millis(750)).await;
}

/// Cancel the owned streaming run and give cooperative cleanup a short grace.
/// Returns `true` when the run stopped cooperatively; `false` when it required
/// an abort. Aborting drops in-flight local futures, but cannot retract a
/// request that an upstream provider already accepted.
pub(super) async fn cancel_and_join_ws_run(
    cancel: CancellationToken,
    mut run_handle: tokio::task::JoinHandle<()>,
    grace: std::time::Duration,
) -> bool {
    cancel.cancel();
    if tokio::time::timeout(grace, &mut run_handle).await.is_ok() {
        return true;
    }
    run_handle.abort();
    let _ = run_handle.await;
    false
}

/// Build the synthetic seat/round/seat_complete/done event sequence for the
/// non-direct-fire smoke shim (N01). Emits, per round:
///   round_started → for each seat: seat_started, 3×seat_chunk, seat_complete
///   → convergence_scored → round_complete
/// then synthesis_started → synthesis_complete → session_saved → done.
///
/// THREE `seat_chunk` frames precede every `seat_complete`; `seat_complete.text`
/// is the authoritative full text (the three chunk deltas concatenated). Zero
/// provider spend, no disk writes — `session_saved` points at a canned path.
/// Synthetic N02 divergence points for the smoke shim — seats placed on a unit
/// circle so the UI scatter has plausible, deterministic geometry without any
/// embeddings call or provider spend.
pub(crate) fn smoke_divergence_points(
    seats: &[(String, String, String)],
) -> Vec<crate::warroom::divergence::DivergencePoint> {
    let n = seats.len();
    if n == 0 {
        return vec![];
    }
    seats
        .iter()
        .enumerate()
        .map(|(i, (name, _, _))| {
            let theta = (i as f64) * std::f64::consts::TAU / (n as f64);
            crate::warroom::divergence::DivergencePoint {
                seat: name.clone(),
                x: (theta.cos() * 1e6).round() / 1e6,
                y: (theta.sin() * 1e6).round() / 1e6,
            }
        })
        .collect()
}

pub(crate) fn build_smoke_seat_events(
    session_id: &str,
    rounds_planned: u32,
    seats: &[(String, String, String)],
    chair_provider: &str,
    chair_model: &str,
    topic: &str,
) -> Vec<StreamEvent> {
    const CHUNK_PARTS: [&str; 3] = ["[smoke] ", "synthetic ", "stream"];
    let mut events: Vec<StreamEvent> = Vec::new();
    let total_rounds = rounds_planned.max(1);

    for round_num in 1..=total_rounds {
        events.push(StreamEvent::round_started(
            session_id,
            round_num,
            total_rounds,
        ));
        for (name, provider, model) in seats {
            events.push(StreamEvent::seat_started(
                session_id, round_num, name, provider, model,
            ));
            for (seq, part) in CHUNK_PARTS.iter().enumerate() {
                events.push(StreamEvent::seat_chunk(
                    session_id, round_num, name, part, seq as u32,
                ));
            }
            // Authoritative full text = concatenated chunk deltas (the UI
            // replaces the accumulated chunks with this).
            let full_text = CHUNK_PARTS.concat();
            let resp = crate::types::SeatResponse {
                seat_name: name.clone(),
                provider: provider.clone(),
                model: model.clone(),
                text: full_text,
                round_num,
                latency_ms: 0,
                tokens_in: 0,
                tokens_out: 0,
                cached_in: 0,
                cost_usd: 0.0,
                error: None,
                gateway: None,
                provider_provenance: None,
            };
            events.push(StreamEvent::seat_complete(
                session_id,
                serde_json::to_value(&resp).unwrap_or_default(),
            ));
        }
        events.push(StreamEvent::convergence_scored(
            session_id, round_num, 1.0, true,
        ));
        // N02 smoke: synthetic round_divergence with plausible per-seat points
        // arranged on a circle (deterministic, no embeddings call). Only when
        // there are >= 2 seats — mirrors the real path's omit-when-<2 rule.
        if smoke_divergence_points(seats).len() >= 2 {
            events.push(StreamEvent::round_divergence(
                session_id,
                round_num,
                smoke_divergence_points(seats),
            ));
        }
        events.push(StreamEvent::round_complete(
            session_id, round_num, 1.0, true, false,
        ));
    }

    let canned = format!("[smoke] synthesis for: {topic}");
    let _ = chair_provider; // chair provider not surfaced in synthesis events
    events.push(StreamEvent::synthesis_started(session_id, chair_model));
    events.push(StreamEvent::synthesis_complete(
        session_id,
        &canned,
        chair_model,
        0,
        0.0,
        None,
    ));
    events.push(StreamEvent::session_saved(
        session_id,
        "sessions/smoke-session.json",
    ));
    events.push(StreamEvent::done(
        session_id,
        0,
        0.0,
        0,
        &canned,
        1.0,
        total_rounds,
        None,
    ));
    events
}

#[cfg(test)]
#[path = "smoke_seat_events_tests.rs"]
mod smoke_seat_events_tests;
#[cfg(test)]
#[path = "ws_cancel_tests.rs"]
mod ws_cancel_tests;
