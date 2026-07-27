import { describe, expect, it } from "vitest";
import { warroomHealthLabel } from "./warroom-health-label";

describe("warroomHealthLabel — CONNECTING during boot retries", () => {
  it("shows connecting while apiStatus is loading", () => {
    expect(warroomHealthLabel(null, null, "loading", false)).toBe("connecting");
  });

  it("shows connecting while mount-time health retries are in progress", () => {
    // First health poll failed (error) but 1.5/3/6s retries still run.
    expect(warroomHealthLabel(null, null, "error", true)).toBe("connecting");
    expect(warroomHealthLabel(null, null, "online", true)).toBe("connecting");
  });

  it("shows offline only after retries exhausted with no health", () => {
    expect(warroomHealthLabel(null, null, "error", false)).toBe("offline");
    expect(warroomHealthLabel(null, null, "online", false)).toBe("offline");
  });

  it("shows version line when health is present", () => {
    expect(warroomHealthLabel("1.2.3", "4", "online", false)).toBe(
      "gen 1.2.3 · stream 4",
    );
    // Health wins over boot-retry flag.
    expect(warroomHealthLabel("1.2.3", "4", "loading", true)).toBe(
      "gen 1.2.3 · stream 4",
    );
  });
});
