import type { SessionMode } from "./types";

/** Visual badge for a session's deliberation mode chip. */
export interface SessionModeBadge {
  label: string;
  chipClass: string;
}

/**
 * Badges for every non-default `SessionMode` wire value. "normal" is the
 * legacy/default mode and intentionally renders no chip in the session list
 * (see `sessionModeBadge`). "teardown" is the Rust-era default and renders a
 * danger-tinted chip in the proceeding record header and docket rows.
 */
const MODE_BADGES: Record<Exclude<SessionMode, "normal">, SessionModeBadge> = {
  teardown: { label: "TEARDOWN", chipClass: "chip chip-teardown" },
  pathfind: { label: "PATHFIND", chipClass: "chip chip-success" },
  harden: { label: "HARDEN", chipClass: "chip chip-warning" },
  blind: { label: "BLIND", chipClass: "chip chip-cyan" },
  recall: { label: "RECALL", chipClass: "chip chip-amber" },
  wargame: { label: "WARGAME", chipClass: "chip chip-danger" },
  premortem: { label: "PREMORTEM", chipClass: "chip chip-magenta" },
  contrarian: { label: "CONTRARIAN", chipClass: "chip chip-danger" },
  munger: { label: "MUNGER", chipClass: "chip chip-amber" },
  kiss: { label: "KISS", chipClass: "chip chip-cyan" },
  specops: { label: "SPECOPS", chipClass: "chip chip-magenta" },
  unknown: { label: "UNKNOWN", chipClass: "chip" },
};

/**
 * Map a session mode wire value to its badge. Returns `null` for "normal"
 * (the default — no chip, to keep the session list quiet). Unrecognized
 * strings (future modes, hand-edited indexes) fall back to an uppercase
 * plain chip rather than rendering nothing.
 */
export function sessionModeBadge(mode: string): SessionModeBadge | null {
  if (!mode || mode === "normal") return null;
  const badge = MODE_BADGES[mode as Exclude<SessionMode, "normal">];
  return badge ?? { label: mode.toUpperCase(), chipClass: "chip" };
}
