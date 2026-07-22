import { describe, expect, it } from "vitest";
import { fmtCost } from "./cn";

describe("fmtCost", () => {
  it("renders em dash for zero and non-finite values", () => {
    expect(fmtCost(0)).toBe("—");
    expect(fmtCost(-0)).toBe("—");
    expect(fmtCost(Number.NaN)).toBe("—");
    expect(fmtCost(Number.POSITIVE_INFINITY)).toBe("—");
  });

  it("uses five decimals for sub-mill costs", () => {
    expect(fmtCost(0.00042)).toBe("$0.00042");
  });

  it("uses coarser precision as cost grows", () => {
    expect(fmtCost(0.0088)).toBe("$0.0088");
    expect(fmtCost(0.889)).toBe("$0.889");
  });
});
