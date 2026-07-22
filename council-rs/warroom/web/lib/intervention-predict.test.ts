import { describe, expect, it } from "vitest";
import { PREDICT_HINT_THRESHOLD, predictHint } from "./intervention-predict";

describe("predictHint", () => {
  it("shows the hint above the threshold and labels it a heuristic with sample basis", () => {
    const h = predictHint({ probability: 0.72, method: "logreg", n_samples: 41 });
    expect(h.show).toBe(true);
    expect(h.label).toContain("Heuristic");
    expect(h.label).toContain("72%");
    expect(h.label).toContain("41 prior interventions");
    expect(h.label).toContain("SpecOps");
  });

  it("hides the hint at or below the threshold", () => {
    expect(predictHint({ probability: PREDICT_HINT_THRESHOLD, method: "logreg", n_samples: 50 }).show).toBe(false);
    expect(predictHint({ probability: 0.3, method: "frequency", n_samples: 12 }).show).toBe(false);
  });

  it("labels the frequency fallback method honestly", () => {
    const h = predictHint({ probability: 0.9, method: "frequency", n_samples: 12 });
    expect(h.show).toBe(true);
    expect(h.label).toContain("overall escalation frequency");
    expect(h.label).toContain("12 prior interventions");
  });

  it("returns no hint when the fetch failed (null/undefined)", () => {
    expect(predictHint(null)).toEqual({ show: false, label: "" });
    expect(predictHint(undefined)).toEqual({ show: false, label: "" });
  });

  it("returns no hint for a NaN/garbage probability", () => {
    // @ts-expect-error — exercising defensive runtime path
    expect(predictHint({ probability: "high", method: "logreg", n_samples: 40 }).show).toBe(false);
    expect(predictHint({ probability: NaN, method: "logreg", n_samples: 40 }).show).toBe(false);
  });
});
