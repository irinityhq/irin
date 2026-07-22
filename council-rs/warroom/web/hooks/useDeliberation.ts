"use client";

import { useCallback, useReducer, useRef } from "react";
import {
  AwaitingInputData,
  ConvergenceScoredData,
  DeliberationState,
  DoneData,
  InterventionAction,
  InterventionPayload,
  PrecedentMatch,
  RoundRuntimeState,
  BudgetPausedData,
  PhaseStartedData,
  RoundValidationData,
  RoundDivergenceData,
  SeatChunkData,
  SeatCompleteData,
  SeatRef,
  SessionStartedData,
  SpecopsSignalData,
  StreamEvent,
  SynthesisCompleteData,
  initialState,
} from "@/lib/types";
import { configReady } from "@/lib/runtime-config";
import { DeliberationSocket, StartPayload, openDeliberation } from "@/lib/ws";

type Action =
  | { kind: "reset" }
  | { kind: "aborted" }
  | { kind: "connecting" }
  | { kind: "intervening"; action: InterventionAction }
  | { kind: "event"; ev: StreamEvent }
  | { kind: "fatal"; message: string }
  | { kind: "closed" };

export const ABORT_NOTICE =
  "Local Council dispatch stopped. Requests already accepted by a provider may still finish or incur charges.";

function emptyRound(num: number, seats: SeatRef[]): RoundRuntimeState {
  const map: Record<string, RoundRuntimeState["seats"][string]> = {};
  for (const s of seats) {
    map[s.name] = {
      seat: s,
      text: "",
      status: "pending",
      latency_ms: 0,
      tokens_in: 0,
      tokens_out: 0,
      cached_in: 0,
      cost_usd: 0,
      error: null,
      round_num: num,
      provider_provenance: null,
    };
  }
  return { round_num: num, seats: map, complete: false };
}

export function reduceDeliberationState(
  state: DeliberationState,
  a: Action,
): DeliberationState {
  switch (a.kind) {
    case "reset":
      return { ...initialState };
    case "aborted":
      if (state.phase === "done" || state.phase === "idle") return state;
      return {
        ...state,
        phase: "error",
        pendingIntervention: null,
        errors: [
          ...state.errors,
          {
            message: ABORT_NOTICE,
            ts: new Date().toISOString(),
            fatal: true,
          },
        ],
      };
    case "connecting":
      return { ...initialState, phase: "connecting" };
    case "intervening":
      return {
        ...state,
        pendingIntervention: a.action === "end_early" ? "end_early" : null,
      };
    case "fatal":
      if (state.phase === "done" || state.phase === "idle") {
        return state;
      }
      return {
        ...state,
        phase: "error",
        errors: [
          ...state.errors,
          { message: a.message, ts: new Date().toISOString(), fatal: true },
        ],
      };
    case "closed": {
      // If we're in a terminal or pre-start phase, leave state alone.
      if (
        state.phase === "done" ||
        state.phase === "idle" ||
        state.phase === "error"
      ) {
        return state;
      }
      // Otherwise the WS dropped mid-deliberation — surface as an error
      // so the UI exits the frozen streaming/paused/specops/synthesizing/
      // connecting state and can offer reconnect.
      const message = "Connection to council bridge lost";
      return {
        ...state,
        phase: "error",
        errors: [
          ...state.errors,
          { message, ts: new Date().toISOString(), fatal: true },
        ],
      };
    }
    case "event":
      return applyEvent(state, a.ev);
    default:
      return state;
  }
}

/** Pure event reducer — exported for unit tests. */
export function applyEvent(s: DeliberationState, ev: StreamEvent): DeliberationState {
  switch (ev.type) {
    case "session_started": {
      const d = ev.data as SessionStartedData;
      const isLaterPhase = (d.phase ?? 1) > 1;
      return {
        ...(isLaterPhase ? s : { ...initialState }),
        phase: "streaming",
        session_id: ev.session_id,
        topic: d.topic,
        cabinet_label: d.cabinet_name,
        cabinet_name: d.cabinet_name,
        rounds_planned: d.rounds_planned,
        mode: d.mode,
        active_seats: d.active_seats,
        dropped_seats: d.dropped_seats,
        chair: d.chair,
        execution_route:
          d.execution_route ?? (d.via_gateway === true ? "governed" : d.via_gateway === false ? "direct" : "unknown"),
        gateway_sensitivity: d.sensitivity,
        rounds: isLaterPhase ? [] : s.rounds,
        current_round: isLaterPhase ? 0 : s.current_round,
        synthesis: isLaterPhase ? undefined : s.synthesis,
        specops: isLaterPhase ? undefined : s.specops,
        awaiting: undefined,
        stream_phase: d.phase,
        stream_phases_total: d.phases_total,
        deliberation_mode: d.deliberation_mode,
        tier: d.tier,
        totals: isLaterPhase ? { tokens: 0, cost_usd: 0, latency_ms: 0 } : s.totals,
      };
    }
    case "precedent_loaded":
      return { ...s, precedent: (ev.data as { matches: PrecedentMatch[] }).matches };
    case "round_started": {
      const num = (ev.data as { round_num: number }).round_num;
      const existing = s.rounds.find((r) => r.round_num === num);
      if (existing) return { ...s, current_round: num };
      const newRound = emptyRound(num, s.active_seats);
      return { ...s, rounds: [...s.rounds, newRound], current_round: num };
    }
    case "seat_started": {
      const d = ev.data as { round_num: number; seat_name: string };
      return mapRound(s, d.round_num, (r) => {
        const seat = r.seats[d.seat_name];
        if (!seat) return r;
        return {
          ...r,
          seats: { ...r.seats, [d.seat_name]: { ...seat, status: "thinking" } },
        };
      });
    }
    case "seat_chunk": {
      const d = ev.data as SeatChunkData;
      return mapRound(s, d.round_num, (r) => {
        const seat = r.seats[d.seat_name];
        if (!seat) return r;
        // mpsc preserves order, but guard duplicates/replays defensively: a
        // chunk whose seq we've already applied (<= last_seq) is dropped.
        const lastSeq = seat.last_seq ?? -1;
        if (typeof d.seq === "number" && d.seq <= lastSeq) return r;
        return {
          ...r,
          seats: {
            ...r.seats,
            [d.seat_name]: {
              ...seat,
              status: seat.status === "complete" ? seat.status : "thinking",
              streaming: true,
              text: seat.text + (d.text_delta ?? ""),
              last_seq: typeof d.seq === "number" ? d.seq : lastSeq,
            },
          },
        };
      });
    }
    case "seat_complete": {
      const d = ev.data as SeatCompleteData;
      return mapRound(s, d.round_num, (r) => {
        const seat = r.seats[d.seat_name];
        if (!seat) return r;
        return {
          ...r,
          seats: {
            ...r.seats,
            [d.seat_name]: {
              ...seat,
              status: d.error ? "error" : "complete",
              streaming: false,
              text: d.text,
              latency_ms: d.latency_ms,
              tokens_in: d.tokens_in,
              tokens_out: d.tokens_out,
              cached_in: d.cached_in,
              cost_usd: d.cost_usd,
              error: d.error ?? null,
              provider_provenance: d.provider_provenance ?? null,
              gateway_provenance: d.gateway ?? d.gateway_provenance ?? null,
            },
          },
        };
      }, {
        totals: {
          tokens: s.totals.tokens + d.tokens_in + d.tokens_out,
          cost_usd: s.totals.cost_usd + d.cost_usd,
          latency_ms: s.totals.latency_ms + d.latency_ms,
        },
      });
    }
    case "convergence_scored": {
      const d = ev.data as ConvergenceScoredData;
      return mapRound(s, d.round_num, (r) => ({
        ...r,
        convergence: d.score,
        converged: d.converged,
      }));
    }
    case "round_divergence": {
      const d = ev.data as RoundDivergenceData;
      const points = Array.isArray(d.points) ? d.points : [];
      return mapRound(s, d.round_num, (r) => ({
        ...r,
        divergence: points,
      }));
    }
    case "round_complete": {
      const d = ev.data as { round_num: number; early_convergence?: boolean };
      return mapRound(s, d.round_num, (r) => ({
        ...r,
        complete: true,
        early_convergence: d.early_convergence,
      }));
    }
    case "awaiting_input":
      return { ...s, phase: "paused", awaiting: ev.data as AwaitingInputData };
    case "intervention_received":
      return { ...s, phase: "streaming", awaiting: undefined };
    case "specops_started":
      return { ...s, phase: "specops", pendingIntervention: null };
    case "specops_signal": {
      const d = ev.data as SpecopsSignalData;
      return {
        ...s,
        phase: "streaming",
        specops: d,
        totals: {
          ...s.totals,
          cost_usd: s.totals.cost_usd + (d.cost_usd || 0),
          tokens:
            s.totals.tokens + (d.tokens_in || 0) + (d.tokens_out || 0),
          latency_ms: s.totals.latency_ms + (d.latency_ms || 0),
        },
      };
    }
    case "synthesis_started":
      return { ...s, phase: "synthesizing", pendingIntervention: null };
    case "synthesis_complete": {
      const d = ev.data as SynthesisCompleteData;
      return {
        ...s,
        synthesis: d,
        totals: {
          ...s.totals,
          cost_usd: s.totals.cost_usd + d.cost_usd,
          latency_ms: s.totals.latency_ms + d.latency_ms,
        },
      };
    }
    case "session_saved":
      return { ...s, saved_path: (ev.data as { path: string }).path };
    case "done": {
      const d = ev.data as DoneData;
      return {
        ...s,
        phase: "done",
        pendingIntervention: null,
        totals: {
          tokens: d.total_tokens,
          cost_usd: d.total_cost_usd,
          latency_ms: d.total_latency_ms,
        },
      };
    }
    case "budget_paused": {
      const d = ev.data as BudgetPausedData;
      return {
        ...s,
        budget_paused: d,
        info_messages: [
          ...s.info_messages,
          {
            message: `Budget pause: $${d.total_cost_usd.toFixed(4)} / $${d.max_usd.toFixed(2)} (round ${d.round_num})`,
            ts: ev.ts,
          },
        ],
      };
    }
    case "phase_started": {
      const d = ev.data as PhaseStartedData;
      return {
        ...s,
        phase_label: d.label,
        info_messages: [
          ...s.info_messages,
          { message: `Phase ${d.phase}: ${d.label}`, ts: ev.ts },
        ],
      };
    }
    case "info": {
      const d = ev.data as { message: string };
      return {
        ...s,
        info_messages: [...s.info_messages, { message: d.message, ts: ev.ts }],
      };
    }
    case "round_validation": {
      const d = ev.data as RoundValidationData;
      return mapRound(s, d.round_num, (r) => ({
        ...r,
        validation: d,
      }));
    }
    case "error": {
      const d = ev.data as { message: string; fatal: boolean };
      if (s.phase === "done") {
        return s;
      }
      return {
        ...s,
        phase: d.fatal ? "error" : s.phase,
        pendingIntervention: null,
        errors: [
          ...s.errors,
          { message: d.message, ts: ev.ts, fatal: d.fatal },
        ],
      };
    }
    default:
      return s;
  }
}

function mapRound(
  s: DeliberationState,
  num: number,
  fn: (r: RoundRuntimeState) => RoundRuntimeState,
  rest: Partial<DeliberationState> = {},
): DeliberationState {
  return {
    ...s,
    ...rest,
    rounds: s.rounds.map((r) => (r.round_num === num ? fn(r) : r)),
  };
}

export function useDeliberation() {
  const [state, dispatch] = useReducer(reduceDeliberationState, initialState);
  const sockRef = useRef<DeliberationSocket | null>(null);

  const start = useCallback((p: StartPayload) => {
    sockRef.current?.close();
    dispatch({ kind: "connecting" });
    void configReady.then(() => {
      sockRef.current = openDeliberation(
        p,
        (ev) => dispatch({ kind: "event", ev }),
        (msg) => dispatch({ kind: "fatal", message: msg }),
        () => dispatch({ kind: "closed" }),
      );
    });
  }, []);

  const intervene = useCallback((p: InterventionPayload) => {
    dispatch({ kind: "intervening", action: p.action });
    sockRef.current?.send_intervention(p);
  }, []);

  const reset = useCallback(() => {
    sockRef.current?.close();
    sockRef.current = null;
    dispatch({ kind: "reset" });
  }, []);

  const abort = useCallback(() => {
    sockRef.current?.close();
    sockRef.current = null;
    dispatch({ kind: "aborted" });
  }, []);

  return { state, start, intervene, reset, abort };
}
