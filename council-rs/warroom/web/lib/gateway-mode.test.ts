import { describe, expect, it } from "vitest";
import {
  DEFAULT_SENSITIVITY,
  SENSITIVITY_LEVELS,
  gatewayStartFields,
} from "./gateway-mode";
import type { GatewaySensitivity } from "./ws";

describe("gatewayStartFields", () => {
  it("sends an explicit direct route when the toggle is off", () => {
    expect(gatewayStartFields(false, "red")).toEqual({ via_gateway: false });
    expect("via_gateway" in gatewayStartFields(false)).toBe(true);
    expect("sensitivity" in gatewayStartFields(false)).toBe(false);
  });

  it("sends via_gateway: true plus the lowercase sensitivity when on", () => {
    expect(gatewayStartFields(true, "yellow")).toEqual({
      via_gateway: true,
      sensitivity: "yellow",
    });
  });

  it("defaults sensitivity to green when on", () => {
    expect(gatewayStartFields(true)).toEqual({
      via_gateway: true,
      sensitivity: "green",
    });
    expect(DEFAULT_SENSITIVITY).toBe("green");
  });

  // Typed table pinned to the GatewaySensitivity union — a wire-contract
  // change in lib/ws.ts forces this list to be revisited.
  const levels: GatewaySensitivity[] = ["green", "yellow", "red"];

  it.each(levels)("serializes %s lowercase on the wire", (level) => {
    const fields = gatewayStartFields(true, level);
    expect(fields.sensitivity).toBe(level);
    expect(fields.sensitivity).toBe(level.toLowerCase());
  });

  it("SENSITIVITY_LEVELS covers every union member in escalation order", () => {
    expect([...SENSITIVITY_LEVELS]).toEqual(levels);
  });

  it("round-trips through JSON without casing drift", () => {
    const wire = JSON.parse(JSON.stringify(gatewayStartFields(true, "red")));
    expect(wire).toEqual({ via_gateway: true, sensitivity: "red" });
  });
});
