import { describe, expect, it } from "vitest";
import { parseOutboxList } from "./governance";

describe("parseOutboxList", () => {
  it("accepts the directives contract", () => {
    const parsed = parseOutboxList({
      canary_tenant: "sovereign",
      directives: [],
      next_cursor: null,
    });
    expect(parsed.directives).toEqual([]);
  });

  it("rejects the removed records projection", () => {
    expect(() =>
      parseOutboxList({ canary_tenant: "sovereign", records: [] }),
    ).toThrow(/directives/);
  });

  it("requires configured canary truth from the BFF", () => {
    expect(() => parseOutboxList({ directives: [] })).toThrow(/canary tenant/);
  });
});
