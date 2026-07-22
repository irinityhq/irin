import type { DiscoverProvider, DiscoverResponse } from "./types";

/**
 * Defensive normalizer for `GET /api/discover` (feature contract).
 *
 * Contract notes:
 * - `env_hint` is an env var NAME only (e.g. "XAI_API_KEY") — never a value
 *   or key fragment. Empty strings are coerced to null so the UI renders a
 *   hint only when one exists.
 * - `label`, `family`, and `transport` are additive. Older servers fall back
 *   to the exact provider identifier rather than merging transport identities.
 * - `models` may be empty.
 * - Providers are sorted available-first, then by name, so missing providers
 *   with their env hints group together at the bottom.
 */
export { providerModelMap } from "./use-discover";

export function normalizeDiscoverResponse(raw: unknown): DiscoverResponse {
  const obj = (raw && typeof raw === "object" ? raw : {}) as {
    providers?: unknown;
    log?: unknown;
  };
  const providers = (Array.isArray(obj.providers) ? obj.providers : [])
    .map(normalizeProvider)
    .filter((p): p is DiscoverProvider => p !== null)
    .sort((a, b) =>
      a.available === b.available
        ? a.name.localeCompare(b.name)
        : a.available
          ? -1
          : 1,
    );
  const log = (Array.isArray(obj.log) ? obj.log : []).filter(
    (l): l is string => typeof l === "string",
  );
  return { providers, log };
}

function normalizeProvider(raw: unknown): DiscoverProvider | null {
  if (!raw || typeof raw !== "object") return null;
  const p = raw as Record<string, unknown>;
  if (typeof p.name !== "string" || !p.name.trim()) return null;
  const rawEnvHint = typeof p.env_hint === "string" ? p.env_hint.trim() : "";
  const envHint = /^[A-Z_][A-Z0-9_]*$/.test(rawEnvHint) ? rawEnvHint : null;
  return {
    name: p.name,
    label:
      typeof p.label === "string" && p.label.trim() ? p.label.trim() : p.name,
    family: typeof p.family === "string" ? p.family.trim() : "",
    transport: typeof p.transport === "string" ? p.transport.trim() : "",
    available: p.available === true,
    // Schema skew must fail closed for Governed mode. Older servers that do
    // not declare transport capability remain Direct-only in the new UI.
    gateway_supported: p.gateway_supported === true,
    source: typeof p.source === "string" ? p.source : "",
    env_hint: envHint,
    models: (Array.isArray(p.models) ? p.models : []).filter(
      (m): m is string => typeof m === "string" && m.trim() !== "",
    ),
  };
}
