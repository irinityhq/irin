import { describe, expect, it } from "vitest";
import {
  CABINET_NAME_RE,
  cabinetToYaml,
  isValidCabinetName,
  lintCabinetYaml,
  suggestCabinetKey,
  validateCabinetForSave,
} from "./cabinet-save";
import type { Cabinet } from "./types";

// Pinned server contract (feature contract): POST /api/cabinets/save rejects any name
// not matching ^[a-z0-9][a-z0-9_-]{0,63}$ — no slashes, dots, or traversal.
describe("isValidCabinetName", () => {
  const valid = [
    "a",
    "warroom2",
    "my-cabinet",
    "triad-strategy",
    "snake_case_name",
    "0numeric-start",
    "a".repeat(64), // 1 + 63 tail chars = max length
  ];
  it.each(valid)("accepts %s", (name) => {
    expect(isValidCabinetName(name)).toBe(true);
  });

  const invalid = [
    "",
    "A", // uppercase
    "Warroom",
    "-leading-dash",
    "_leading-underscore",
    "café",
    "a/b", // path separator
    "../traversal",
    "a.yaml", // dot
    "has space",
    "a".repeat(65), // too long
  ];
  it.each(invalid)("rejects %j", (name) => {
    expect(isValidCabinetName(name)).toBe(false);
  });

  it("regex is anchored (no partial matches)", () => {
    expect(CABINET_NAME_RE.test("ok-name\n../etc")).toBe(false);
  });
});

describe("suggestCabinetKey", () => {
  it("slugifies display labels", () => {
    expect(suggestCabinetKey("War Room")).toBe("war-room");
    expect(suggestCabinetKey("My Cabinet (fork of c123)")).toBe(
      "my-cabinet-fork-of-c123",
    );
  });
  it("strips leading non-alphanumerics and trailing separators", () => {
    expect(suggestCabinetKey("--Hello--")).toBe("hello");
    expect(suggestCabinetKey("(parens)")).toBe("parens");
  });
  it("returns empty string when nothing valid can be derived", () => {
    expect(suggestCabinetKey("")).toBe("");
    expect(suggestCabinetKey("---")).toBe("");
    expect(suggestCabinetKey("日本語")).toBe("");
  });
  it("truncates to the 64-char server limit", () => {
    const out = suggestCabinetKey("x".repeat(100));
    expect(out.length).toBe(64);
    expect(isValidCabinetName(out)).toBe(true);
  });
});

function fixtureCabinet(overrides: Partial<Cabinet> = {}): Cabinet {
  return {
    name: "standard",
    label: "Standard Council",
    description: "The default five-seat council",
    rounds: 2,
    is_triad: false,
    seats: [
      {
        name: "The Contrarian",
        provider: "grok",
        model: "grok-4",
        system: "Tear it down.\nLine two: \"quoted\" text.",
      },
    ],
    chair: { name: "Chair", provider: "claude", model: "claude-opus-4" },
    ...overrides,
  };
}

describe("validateCabinetForSave", () => {
  it("accepts a well-formed cabinet", () => {
    expect(validateCabinetForSave(fixtureCabinet())).toBeNull();
  });
  it("rejects empty label / no seats / bad rounds", () => {
    expect(validateCabinetForSave(fixtureCabinet({ label: " " }))).toMatch(/label/);
    expect(validateCabinetForSave(fixtureCabinet({ seats: [] }))).toMatch(/seat/);
    expect(validateCabinetForSave(fixtureCabinet({ rounds: 0 }))).toMatch(/rounds/);
  });
  it("rejects seats missing provider/model", () => {
    const cab = fixtureCabinet();
    cab.seats[0].model = "";
    expect(validateCabinetForSave(cab)).toMatch(/seat/i);
  });
});

describe("cabinetToYaml", () => {
  // Rust Cabinet serde shape: YAML `name` is the DISPLAY label; the registry
  // key is the file stem chosen at save time.
  it("emits the Rust serde shape with JSON-quoted scalars", () => {
    const yaml = cabinetToYaml(fixtureCabinet());
    expect(yaml).toContain('name: "Standard Council"');
    expect(yaml).toContain("rounds: 2");
    expect(yaml).toContain('  - name: "The Contrarian"');
    expect(yaml).toContain('    provider: "grok"');
    // Multiline + embedded quotes survive as YAML double-quoted escapes.
    expect(yaml).toContain(
      '    system: "Tear it down.\\nLine two: \\"quoted\\" text."',
    );
    expect(yaml).toContain("chair:");
    expect(yaml).toContain('  provider: "claude"');
    expect(yaml.endsWith("\n")).toBe(true);
  });
  it("omits client-only fields (label/is_triad) and defaults chair name", () => {
    const cab = fixtureCabinet();
    cab.chair = { provider: "claude", model: "claude-opus-4" };
    const yaml = cabinetToYaml(cab);
    expect(yaml).not.toContain("label:");
    expect(yaml).not.toContain("is_triad");
    expect(yaml).toContain('  name: "Chair"');
  });
  it("emits local_code_only only when true", () => {
    expect(cabinetToYaml(fixtureCabinet())).not.toContain("local_code_only");
    expect(
      cabinetToYaml(fixtureCabinet({ local_code_only: true })),
    ).toContain("local_code_only: true");
  });
  it("omits optional chair fields + synthesis_mode by default", () => {
    const yaml = cabinetToYaml(fixtureCabinet());
    // The seat keeps its own `    system:` (4-space indent); only the chair's
    // optional `  system:` (2-space indent) must be absent.
    expect(yaml).not.toContain("\n  system:");
    expect(yaml).not.toContain("thinking_effort:");
    expect(yaml).not.toContain("synthesis_mode:");
  });
  it("round-trips chair.system / chair.thinking_effort when present", () => {
    const yaml = cabinetToYaml(
      fixtureCabinet({
        chair: {
          name: "Chair",
          provider: "claude",
          model: "claude-opus-4",
          system: 'Synthesize.\n"strictly".',
          thinking_effort: "high",
        },
      }),
    );
    // Indented under chair: (two-space), JSON-quoted scalars.
    expect(yaml).toContain('  system: "Synthesize.\\n\\"strictly\\"."');
    expect(yaml).toContain('  thinking_effort: "high"');
    // Still parses as a valid cabinet document.
    expect(lintCabinetYaml(yaml)).toEqual({ ok: true, missing: [] });
  });
  it("emits synthesis_mode only for the non-default value", () => {
    expect(
      cabinetToYaml(fixtureCabinet({ synthesis_mode: "generic" })),
    ).not.toContain("synthesis_mode:");
    expect(
      cabinetToYaml(fixtureCabinet({ synthesis_mode: "directive_proposal_v1" })),
    ).toContain("synthesis_mode: directive_proposal_v1");
  });
  it("passes its own import lint (save/import flows agree)", () => {
    expect(lintCabinetYaml(cabinetToYaml(fixtureCabinet()))).toEqual({
      ok: true,
      missing: [],
    });
  });
});

describe("lintCabinetYaml", () => {
  it("flags missing required top-level keys", () => {
    const r = lintCabinetYaml("name: Foo\nrounds: 2\n");
    expect(r.ok).toBe(false);
    expect(r.missing).toEqual(["seats", "chair"]);
  });
  it("only matches keys at column 0 (nested keys do not count)", () => {
    const r = lintCabinetYaml("wrapper:\n  name: Foo\n  rounds: 2\n  seats: []\n  chair: {}\n");
    expect(r.ok).toBe(false);
    expect(r.missing).toEqual(["name", "rounds", "seats", "chair"]);
  });
  it("accepts a real cabinet-shaped document", () => {
    const doc = [
      "name: Imported",
      "description: x",
      "rounds: 2",
      "seats:",
      "  - name: A",
      "    provider: grok",
      "    model: grok-4",
      "    system: s",
      "chair:",
      "  name: Chair",
      "  provider: claude",
      "  model: claude-opus-4",
      "",
    ].join("\n");
    expect(lintCabinetYaml(doc)).toEqual({ ok: true, missing: [] });
  });
});
