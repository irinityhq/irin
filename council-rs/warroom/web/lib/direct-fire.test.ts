import { describe, expect, it } from "vitest";
import {
  DIRECT_FIRE_MODES,
  buildDirectFireStartPayload,
} from "./direct-fire";
import type { DirectFireMode } from "./ws";

describe("DIRECT_FIRE_MODES", () => {
  // Typed table pinned to the DirectFireMode union — a wire-contract change
  // in lib/ws.ts forces this list to be revisited.
  const wireModes: DirectFireMode[] = [
    "contrarian",
    "munger",
    "kiss",
    "specops",
    "premortem",
  ];

  it("covers every DirectFireMode union member exactly once", () => {
    expect(DIRECT_FIRE_MODES.map((m) => m.mode)).toEqual(wireModes);
  });

  it("flags only premortem as experimental (kill-criteria banner)", () => {
    const experimental = DIRECT_FIRE_MODES.filter((m) => m.experimental);
    expect(experimental.map((m) => m.mode)).toEqual(["premortem"]);
  });
});

describe("buildDirectFireStartPayload", () => {
  it("sets the pinned direct_fire wire field and trims the topic", () => {
    const p = buildDirectFireStartPayload({
      topic: "  Tear down this plan  ",
      mode: "contrarian",
    });
    expect(p.direct_fire).toBe("contrarian");
    expect(p.topic).toBe("Tear down this plan");
  });

  it("uses a valid cabinet registry key (parser + smoke shim load it)", () => {
    const p = buildDirectFireStartPayload({ topic: "t", mode: "kiss" });
    expect(p.cabinet_name).toBe("standard");
  });

  it("omits context when blank", () => {
    const p = buildDirectFireStartPayload({
      topic: "t",
      mode: "specops",
      context: "   ",
    });
    expect(p.context).toBeUndefined();
  });

  it("passes trimmed context through", () => {
    const p = buildDirectFireStartPayload({
      topic: "t",
      mode: "premortem",
      context: "  background  ",
    });
    expect(p.context).toBe("background");
  });

  it("sends an explicit direct route by default", () => {
    const p = buildDirectFireStartPayload({ topic: "t", mode: "munger" });
    expect(p.via_gateway).toBe(false);
    expect("sensitivity" in p).toBe(false);
  });

  it("threads via_gateway + lowercase sensitivity when enabled", () => {
    const p = buildDirectFireStartPayload({
      topic: "t",
      mode: "munger",
      viaGateway: true,
      sensitivity: "red",
    });
    expect(p.via_gateway).toBe(true);
    expect(p.sensitivity).toBe("red");
  });

  it("never sets round/council fields (single-shot contract)", () => {
    const p = buildDirectFireStartPayload({ topic: "t", mode: "contrarian" });
    expect(p.max_rounds).toBeUndefined();
    expect(p.mode).toBeUndefined();
    expect(p.smoke_only).toBeUndefined();
  });
});
