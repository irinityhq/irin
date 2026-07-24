import type { Cabinet } from "./types";

/** Documented War Room default when no explicit cabinet is supplied. */
export const DEFAULT_CABINET_NAME = "standard";

/**
 * A cabinet is runnable when every seat + chair transport is available in the
 * normalized Discover inventory (`GET /api/discover` → providers with
 * `available: true`). `/api/health` is a liveness probe that deliberately
 * does not probe host CLI transports, so it must not feed runnability.
 */
export function cabinetRequiredProviders(cabinet: Cabinet): string[] {
  return [
    ...cabinet.seats.map((seat) => seat.provider),
    cabinet.chair.provider,
  ].filter(Boolean);
}

export function isCabinetRunnable(
  cabinet: Cabinet,
  providersAvailable: readonly string[] | null | undefined,
): boolean {
  if (!providersAvailable) return false;
  const have = new Set(providersAvailable);
  return cabinetRequiredProviders(cabinet).every((p) => have.has(p));
}

export function cabinetMissingProviders(
  cabinet: Cabinet,
  providersAvailable: readonly string[] | null | undefined,
): string[] {
  const have = new Set(providersAvailable ?? []);
  return [...new Set(cabinetRequiredProviders(cabinet).filter((p) => !have.has(p)))];
}

/**
 * Stable first-load default when the preferred cabinet is not runnable.
 *
 * Rule (documented in docs/cabinets.md):
 * 1. Keep `preferred` when it exists and is runnable.
 * 2. Otherwise pick the first runnable cabinet in API list order
 *    (embedded cabinets before triads when both groups appear).
 * 3. Return null when none are runnable — caller keeps selection and explains.
 *
 * Does not inspect operator credentials or hard-code host-specific cabinets.
 */
export function pickRunnableCabinetName(
  cabinets: readonly Cabinet[],
  providersAvailable: readonly string[],
  preferred: string = DEFAULT_CABINET_NAME,
): string | null {
  if (cabinets.length === 0) return null;

  const preferredCab = cabinets.find((c) => c.name === preferred);
  if (preferredCab && isCabinetRunnable(preferredCab, providersAvailable)) {
    return preferredCab.name;
  }

  // Prefer non-triad cabinets first (stable product default), then triads,
  // preserving relative order within each group (API / registry order).
  const ordered = [
    ...cabinets.filter((c) => !c.is_triad),
    ...cabinets.filter((c) => c.is_triad),
  ];
  const runnable = ordered.find((c) => isCabinetRunnable(c, providersAvailable));
  return runnable?.name ?? null;
}

/**
 * One-shot auto-select for an untouched Deliberate form.
 *
 * Returns the cabinet name to apply, or null when the caller should keep the
 * current selection (explicit preference, already runnable, or nothing runnable).
 */
export function resolveUntouchedCabinetSelection(args: {
  cabinets: readonly Cabinet[];
  providersAvailable: readonly string[] | null | undefined;
  currentName: string;
  /** True once the operator, editor handoff, or a prior auto decision locked selection. */
  selectionLocked: boolean;
  preferredDefault?: string;
}): string | null {
  const {
    cabinets,
    providersAvailable,
    currentName,
    selectionLocked,
    preferredDefault = DEFAULT_CABINET_NAME,
  } = args;

  if (selectionLocked) return null;
  // Availability inventory not loaded yet — wait; do not thrash on null.
  if (providersAvailable == null) return null;
  if (cabinets.length === 0) return null;

  const current = cabinets.find((c) => c.name === currentName);
  if (current && isCabinetRunnable(current, providersAvailable)) {
    return null; // keep current (typically the preferred default)
  }

  return pickRunnableCabinetName(cabinets, providersAvailable, preferredDefault);
}

/** Single actionable line when every cabinet is blocked by missing providers. */
export function noRunnableCabinetExplanation(
  cabinets: readonly Cabinet[],
  providersAvailable: readonly string[] | null | undefined,
): string | null {
  if (providersAvailable == null || cabinets.length === 0) return null;
  if (cabinets.some((c) => isCabinetRunnable(c, providersAvailable))) return null;
  return (
    "No cabinet has all required providers available. " +
    "Open Discover to configure transports, or edit a cabinet to use providers you have."
  );
}
