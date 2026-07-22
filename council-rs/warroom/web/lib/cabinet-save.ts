import type { Cabinet } from "./types";

/**
 * Cabinet save helpers (feature contract / feature contract).
 *
 * Pinned contract: POST /api/cabinets/save takes {"name", "yaml"} where
 * `name` is the registry key (file stem under <base_dir>/cabinets/) and
 * `yaml` must parse as a Rust `Cabinet` (serde_yaml) server-side. The server
 * is the authority on YAML validity and built-in overwrite protection —
 * everything here is client-side pre-flight only.
 */

/** Server-pinned registry-key rule — keep in sync with /api/cabinets/save. */
export const CABINET_NAME_RE = /^[a-z0-9][a-z0-9_-]{0,63}$/;

export function isValidCabinetName(name: string): boolean {
  return CABINET_NAME_RE.test(name);
}

/**
 * Slugify a display label into a registry-key candidate. Returns "" when no
 * valid key can be derived (caller should leave the field for the user).
 */
export function suggestCabinetKey(label: string): string {
  const slug = label
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^[^a-z0-9]+/, "")
    .replace(/[-_]+$/, "")
    .slice(0, 64);
  return isValidCabinetName(slug) ? slug : "";
}

/**
 * Pre-flight a draft Cabinet before serializing. Returns an error message or
 * null. Light checks only — validate_cabinet_for_execution runs server-side.
 */
export function validateCabinetForSave(cab: Cabinet): string | null {
  if (!cab.label.trim()) return "Cabinet label must not be empty";
  if (!Number.isFinite(cab.rounds) || cab.rounds < 1) {
    return "Cabinet rounds must be >= 1";
  }
  if (cab.seats.length === 0) return "Cabinet needs at least one seat";
  for (const s of cab.seats) {
    if (!s.name.trim() || !s.provider.trim() || !s.model.trim()) {
      return "Every seat needs name, provider, and model";
    }
  }
  if (!cab.chair.provider.trim() || !cab.chair.model.trim()) {
    return "Chair needs provider and model";
  }
  return null;
}

/**
 * Serialize a client Cabinet into YAML matching the Rust `Cabinet` serde
 * shape (src/types.rs): YAML `name` is the DISPLAY label (client `label`) —
 * the registry key is the file stem chosen at save time. All string scalars
 * are emitted via JSON.stringify, which produces valid YAML double-quoted
 * flow scalars (\n, \", \\, \uXXXX escapes) — no YAML serializer dependency
 * needed (js-yaml is not a runtime dep). serde_yaml ignores unknown keys, so
 * we emit only the Rust-required fields.
 */
export function cabinetToYaml(cab: Cabinet): string {
  const q = (s: string) => JSON.stringify(s);
  const lines: string[] = [
    `name: ${q(cab.label)}`,
    `description: ${q(cab.description ?? "")}`,
    `rounds: ${Math.max(1, Math.trunc(cab.rounds))}`,
    "seats:",
  ];
  for (const s of cab.seats) {
    lines.push(`  - name: ${q(s.name)}`);
    lines.push(`    provider: ${q(s.provider)}`);
    lines.push(`    model: ${q(s.model)}`);
    lines.push(`    system: ${q(s.system)}`);
  }
  lines.push("chair:");
  lines.push(`  name: ${q(cab.chair.name ?? "Chair")}`);
  lines.push(`  provider: ${q(cab.chair.provider)}`);
  lines.push(`  model: ${q(cab.chair.model)}`);
  // Optional chair knobs — emit only when present so the YAML round-trips the
  // Rust `Chair` shape (both are `#[serde(default)] Option<String>`).
  if (cab.chair.system) lines.push(`  system: ${q(cab.chair.system)}`);
  if (cab.chair.thinking_effort) {
    lines.push(`  thinking_effort: ${q(cab.chair.thinking_effort)}`);
  }
  if (cab.local_code_only) lines.push("local_code_only: true");
  // Skip the serde default ("generic") so back-compat wire stays minimal.
  if (cab.synthesis_mode && cab.synthesis_mode !== "generic") {
    lines.push(`synthesis_mode: ${cab.synthesis_mode}`);
  }
  return lines.join("\n") + "\n";
}

/** Rust `Cabinet` required top-level keys (serde) — used for import lint. */
export const REQUIRED_CABINET_KEYS = ["name", "rounds", "seats", "chair"] as const;

export interface CabinetYamlLint {
  ok: boolean;
  missing: string[];
}

/**
 * Light client-side lint of imported cabinet YAML (feature contract). Checks only that
 * the Rust-required top-level keys appear at column 0 — the raw text is
 * POSTed untouched and serde_yaml on the server is the real validator.
 */
export function lintCabinetYaml(text: string): CabinetYamlLint {
  const missing = REQUIRED_CABINET_KEYS.filter(
    (k) => !new RegExp(`^${k}\\s*:`, "m").test(text),
  );
  return { ok: missing.length === 0, missing: [...missing] };
}

/** Response shape of POST /api/cabinets/save (pinned contract). */
export interface CabinetSaveResponse {
  ok: boolean;
  name: string;
  path: string;
}
