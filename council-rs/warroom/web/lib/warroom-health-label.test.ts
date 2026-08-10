import { describe, expect, it } from "vitest";
import {
  COUNCIL_LOADING_LABEL,
  warroomHealthLabel,
} from "./warroom-health-label";

describe("warroomHealthLabel — Council loading during boot retries", () => {
  it("shows Council loading while apiStatus is loading", () => {
    expect(warroomHealthLabel(null, null, "loading", false)).toBe(
      COUNCIL_LOADING_LABEL,
    );
  });

  it("shows Council loading while mount-time health retries are in progress", () => {
    // First health poll failed (error) but boot retries still run.
    expect(warroomHealthLabel(null, null, "error", true)).toBe(
      COUNCIL_LOADING_LABEL,
    );
    expect(warroomHealthLabel(null, null, "online", true)).toBe(
      COUNCIL_LOADING_LABEL,
    );
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
