import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { deriveCooldownState, parseWatchSnapshot } from "./watch-gateway";

const snapshot = {
  tenant: "configured-canary",
  canary_tenant: "configured-canary",
  action_production_armed: false,
  sentinels: [],
  temperature: { value: 0, level: "cold", fires_last_hour: 0, fires_last_24h: 0 },
  recent_fires: [],
  recent_execute_receipts: [],
  budget: { spend_today_usd: 0, spend_cap_usd: 25 },
  degradation: {},
};

/** Shared execute-receipt corpus (seam fixture). Path pinned via import.meta.url. */
type CorpusExpect = "accept" | "reject";
type CorpusCase = {
  name: string;
  fault: string;
  expect: CorpusExpect;
  reason_substring: string | null;
  receipt: Record<string, unknown>;
};

const corpusPath = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../../docs/seams/fixtures/execute_receipt_cases.json",
);
const executeReceiptCases = JSON.parse(readFileSync(corpusPath, "utf8")) as CorpusCase[];

describe("parseWatchSnapshot", () => {
  it("uses configured canary truth from the BFF", () => {
    expect(parseWatchSnapshot(snapshot).canary_tenant).toBe("configured-canary");
  });

  it("rejects a missing or mismatched canary instead of inventing system", () => {
    expect(() => parseWatchSnapshot({ ...snapshot, canary_tenant: undefined })).toThrow(/canary/);
    expect(() => parseWatchSnapshot({ ...snapshot, tenant: "system" })).toThrow(/match/);
  });

  it("requires an explicit action-production state", () => {
    expect(() => parseWatchSnapshot({ ...snapshot, action_production_armed: undefined })).toThrow(
      /action-production/,
    );
  });

  it("requires the redacted execute-receipt collection", () => {
    const rest = { ...snapshot } as Record<string, unknown>;
    delete rest.recent_execute_receipts;
    expect(() => parseWatchSnapshot(rest)).toThrow(/collection/);
  });

  describe("execute receipt corpus (shared with gateway gate4)", () => {
    it("corpus is non-empty and reachable from this package", () => {
      expect(executeReceiptCases.length).toBeGreaterThan(0);
    });

    for (const c of executeReceiptCases) {
      it(`${c.expect}: ${c.name}`, () => {
        const body = {
          ...snapshot,
          recent_execute_receipts: [c.receipt],
        };
        if (c.expect === "accept") {
          const parsed = parseWatchSnapshot(body);
          expect(parsed.recent_execute_receipts).toHaveLength(1);
          expect(parsed.recent_execute_receipts[0]).toMatchObject(c.receipt);
        } else {
          expect(() => parseWatchSnapshot(body)).toThrow(
            c.reason_substring ? new RegExp(c.reason_substring) : /./,
          );
        }
      });
    }
  });
});

describe("deriveCooldownState", () => {
  const now = 1_000_000;

  it("returns hard-killed when hard_killed_at is set", () => {
    expect(
      deriveCooldownState(
        { enabled: true, hard_killed_at: now - 1000, last_fire_at: null, cooldown_ms: 5000 },
        now,
      ),
    ).toBe("hard-killed");
  });

  it("returns disabled when not enabled", () => {
    expect(
      deriveCooldownState(
        { enabled: false, hard_killed_at: null, last_fire_at: null, cooldown_ms: 5000 },
        now,
      ),
    ).toBe("disabled");
  });

  it("returns cooldown inside the configured window", () => {
    expect(
      deriveCooldownState(
        { enabled: true, hard_killed_at: null, last_fire_at: now - 2000, cooldown_ms: 5000 },
        now,
      ),
    ).toBe("cooldown");
  });

  it("returns ready after cooldown elapsed", () => {
    expect(
      deriveCooldownState(
        { enabled: true, hard_killed_at: null, last_fire_at: now - 10_000, cooldown_ms: 5000 },
        now,
      ),
    ).toBe("ready");
  });
});
