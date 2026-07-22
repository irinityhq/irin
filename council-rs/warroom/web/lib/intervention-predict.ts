import type { InterventionPrediction } from "./types";

/** Probability above which the proactive SpecOps hint is shown (N04). */
export const PREDICT_HINT_THRESHOLD = 0.6;

export interface PredictHint {
  show: boolean;
  /** Human copy — always labels itself a heuristic with the sample basis. */
  label: string;
}

/**
 * Pure gate for the N04 proactive intervention hint. The hint only appears
 * when the predicted escalation probability strictly exceeds the threshold.
 * Copy is always explicit that this is a heuristic and on what it is based.
 *
 * Returns `{ show: false }` for null/undefined (fetch failed) or out-of-range
 * probabilities — the panel shows no hint in those cases.
 */
export function predictHint(
  prediction: InterventionPrediction | null | undefined,
): PredictHint {
  if (
    !prediction ||
    typeof prediction.probability !== "number" ||
    Number.isNaN(prediction.probability)
  ) {
    return { show: false, label: "" };
  }
  const pct = Math.round(prediction.probability * 100);
  const basis =
    prediction.method === "logreg"
      ? `based on ${prediction.n_samples} prior interventions`
      : `based on overall escalation frequency (${prediction.n_samples} prior interventions)`;
  return {
    show: prediction.probability > PREDICT_HINT_THRESHOLD,
    label: `Heuristic: ~${pct}% chance this stalls — consider SpecOps (${basis}).`,
  };
}
