import type { DeliberationState, SessionDetail } from "./types";

export type PhaseState = "" | "done" | "active";

export interface ProceedingPhase {
  label: string;
  sub: string;
  state: PhaseState;
}

export function buildPhasesForSession(detail: SessionDetail): ProceedingPhase[] {
  const rounds = detail.rounds.length;
  const lastRound = detail.rounds[rounds - 1];
  const seatCount = lastRound?.responses.length ?? 0;
  const validationFacts = detail.rounds.reduce(
    (n, r) => n + (r.validation_report?.length ?? 0),
    0,
  );
  const hasSynthesis = Boolean(detail.synthesis?.trim());

  return [
    { label: "Convene", sub: "locked", state: rounds > 0 ? "done" : "active" },
    {
      label: rounds > 1 ? `Round ${rounds}` : "Round 1",
      sub: seatCount ? `${seatCount} seats` : "—",
      state: rounds > 0 ? (hasSynthesis ? "done" : "active") : "",
    },
    {
      label: "Cross-poll",
      sub: rounds > 1 ? `${rounds - 1} pass` : "—",
      state: rounds > 1 ? "done" : "",
    },
    {
      label: "Validate",
      sub: validationFacts ? `${validationFacts} facts` : "—",
      state: validationFacts > 0 ? "done" : "",
    },
    {
      label: "Ruled",
      sub: hasSynthesis ? "filed" : "—",
      state: hasSynthesis ? "active" : "",
    },
    {
      label: "Precedent",
      sub: hasSynthesis ? "indexed" : "—",
      state: hasSynthesis ? "done" : "",
    },
  ];
}

/** Live phase rail — stable labels; only `state` fields advance to avoid flicker. */
export function buildPhasesForLive(state: DeliberationState): ProceedingPhase[] {
  const { phase, current_round, rounds, active_seats, synthesis, saved_path } = state;
  const seatCount = active_seats.length;
  const validationFacts = rounds.reduce(
    (n, r) => n + (r.validation?.verdicts?.length ?? 0),
    0,
  );
  const hasValidation = validationFacts > 0;
  const hasSynthesis = Boolean(synthesis?.text);
  const isSynthesizing = phase === "synthesizing";
  const isDone = phase === "done";
  const conveneDone = phase !== "idle" && phase !== "connecting" && phase !== "error";
  const roundLabel =
    current_round > 1 ? `Round ${current_round}` : "Round 1";
  const crossPollPasses = Math.max(0, rounds.length - 1);
  const roundInFlight =
    phase === "streaming" || phase === "paused" || phase === "specops";

  let roundState: PhaseState = "";
  if (conveneDone) {
    if (hasSynthesis || isSynthesizing || isDone) roundState = "done";
    else if (roundInFlight) roundState = "active";
    else if (current_round > 0) roundState = "done";
  }

  let validateState: PhaseState = "";
  if (hasValidation) {
    validateState = hasSynthesis || isDone ? "done" : "active";
  }

  // Filed ruling keeps the amber authority treatment — matches buildPhasesForSession.
  let ruledState: PhaseState = "";
  if (isSynthesizing || hasSynthesis || isDone) ruledState = "active";

  return [
    {
      label: "Convene",
      sub: conveneDone ? "locked" : "—",
      state: conveneDone ? "done" : phase === "connecting" ? "active" : "",
    },
    {
      label: roundLabel,
      sub: seatCount ? `${seatCount} seats` : "—",
      state: roundState,
    },
    {
      label: "Cross-poll",
      sub: crossPollPasses > 0 ? `${crossPollPasses} pass` : "—",
      state: crossPollPasses > 0 ? "done" : "",
    },
    {
      label: "Validate",
      sub: validationFacts ? `${validationFacts} facts` : "—",
      state: validateState,
    },
    {
      label: "Ruled",
      sub: isSynthesizing
        ? "writing…"
        : hasSynthesis || isDone
          ? "filed"
          : "—",
      state: ruledState,
    },
    {
      label: "Precedent",
      sub: saved_path ? "indexed" : "—",
      state: saved_path ? "done" : "",
    },
  ];
}
