import { describe, expect, it } from "vitest";
import {
  proceedingTitle,
  proceedingTopicIsLong,
  truncateProceedingTitle,
} from "./proceeding-display";

describe("proceedingTitle", () => {
  it("skips boilerplate first lines and prefers substantive text", () => {
    const topic = `## Constitution

README.md

Should council-rs ship the gateway handoff this week?`;
    expect(proceedingTitle(topic)).toBe(
      "Should council-rs ship the gateway handoff this week?",
    );
  });

  it("uses the first non-empty line and strips markdown headings", () => {
    const topic = "# Slice 6a red-team\n\n## Constitution\n\nLong body…";
    expect(proceedingTitle(topic)).toBe("Slice 6a red-team");
  });

  it("truncates at question boundaries when possible", () => {
    const topic = "Should we ship the gateway handoff this week?";
    expect(proceedingTitle(topic, 120)).toBe(topic);
    const long = `${topic} ${"Extra context ".repeat(8)}`;
    expect(proceedingTitle(long, 120)).toMatch(/\?$/);
  });

  it("falls back for empty topics", () => {
    expect(proceedingTitle("")).toBe("Untitled proceeding");
  });
});

describe("truncateProceedingTitle", () => {
  it("prefers word boundaries over hard cuts", () => {
    const words = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega";
    const out = truncateProceedingTitle(words, 80);
    expect(out.endsWith("…")).toBe(true);
    expect(out).not.toMatch(/as in…$/);
  });
});

describe("proceedingTopicIsLong", () => {
  it("detects multiline prompts", () => {
    expect(proceedingTopicIsLong("line one\nline two")).toBe(true);
  });

  it("does not disclose when a long single line is fully represented", () => {
    const topic = "Should we ship the gateway handoff this week?";
    expect(proceedingTopicIsLong(topic)).toBe(false);
  });

  it("detects single lines with hidden tail beyond the title", () => {
    const topic = `${"Should we ship?".padEnd(200, " x")}`;
    expect(proceedingTopicIsLong(topic)).toBe(true);
  });
});
