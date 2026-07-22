"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "@/lib/api";
import { normalizeDiscoverResponse } from "./discover";
import type { DiscoverProvider, DiscoverResponse } from "@/lib/types";

// Module-level cache and listeners for deduped fetches across components.
let cache: DiscoverResponse | null = null;
let inFlight: Promise<DiscoverResponse> | null = null;
const listeners = new Set<(data: DiscoverResponse | null, loading: boolean, error: string | null) => void>();

async function doFetch(): Promise<DiscoverResponse> {
  if (inFlight) return inFlight;
  inFlight = (async () => {
    try {
      const raw = await api.discover();
      const normalized = normalizeDiscoverResponse(raw);
      cache = normalized;
      notify(cache, false, null);
      return normalized;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      notify(null, false, msg);
      throw err;
    } finally {
      inFlight = null;
    }
  })();
  return inFlight;
}

function notify(data: DiscoverResponse | null, loading: boolean, error: string | null) {
  listeners.forEach((l) => l(data, loading, error));
}

export function useDiscover() {
  const [data, setData] = useState<DiscoverResponse | null>(cache);
  const [loading, setLoading] = useState<boolean>(false);
  const [error, setError] = useState<string | null>(null);

  const listener = useCallback((d: DiscoverResponse | null, l: boolean, e: string | null) => {
    setData(d);
    setLoading(l);
    setError(e);
  }, []);

  useEffect(() => {
    listeners.add(listener);
    if (cache) {
      setData(cache);
    } else if (!inFlight) {
      setLoading(true);
      setError(null);
      doFetch().catch(() => {});
    }
    return () => {
      listeners.delete(listener);
    };
  }, [listener]);

  const rescan = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      await doFetch();
    } catch {
      // error already notified
    } finally {
      setLoading(false);
    }
  }, []);

  const providerModelMap = useMemo(() => {
    return buildProviderModelMap(data);
  }, [data]);

  const providerOptions = useMemo(() => data?.providers ?? [], [data]);

  return { data, loading, error, rescan, providerModelMap, providerOptions };
}

/**
 * Provider IDs are transport identities. Normalize casing/whitespace only;
 * never collapse API, first-party CLI, or Hermes transports together.
 */
export function normalizeProviderKey(name: string): string {
  return name.toLowerCase().trim();
}

export function getModelsForProvider(
  map: Record<string, string[]>,
  provider: string,
): string[] {
  const key = normalizeProviderKey(provider);
  return map[key] || [];
}


// Standalone helper (for tests or non-hook use)
export function buildProviderModelMap(data: DiscoverResponse | null): Record<string, string[]> {
  const map: Record<string, string[]> = {};
  if (data?.providers) {
    for (const p of data.providers) {
      if (p.available && p.models && p.models.length > 0) {
        map[p.name] = p.models;
      }
    }
  }
  return map;
}

// Backward-compatible export used by focused unit tests and non-hook callers.
export const providerModelMap = buildProviderModelMap;

export function getProviderOption(
  providers: DiscoverProvider[],
  provider: string,
): DiscoverProvider | undefined {
  const key = normalizeProviderKey(provider);
  return providers.find((p) => normalizeProviderKey(p.name) === key);
}

export function providerOptionLabel(provider: DiscoverProvider): string {
  const identity = provider.label === provider.name
    ? provider.name
    : `${provider.label} (${provider.name})`;
  return provider.available ? identity : `${identity} — unavailable`;
}

export interface ProviderChoice {
  name: string;
  label: string;
  available: boolean;
  legacy: boolean;
}

/**
 * Selection rows include the complete discovery inventory. If an existing
 * cabinet references an unknown legacy ID, preserve it as a disabled row so
 * rendering never silently changes the configured transport.
 */
export function buildProviderChoices(
  providers: DiscoverProvider[],
  currentProvider: string,
): ProviderChoice[] {
  const choices = providers.map((provider) => ({
    name: provider.name,
    label: providerOptionLabel(provider),
    available: provider.available,
    legacy: false,
  }));
  if (currentProvider && !getProviderOption(providers, currentProvider)) {
    choices.unshift({
      name: currentProvider,
      label: `${currentProvider} — legacy/unavailable`,
      available: false,
      legacy: true,
    });
  }
  return choices;
}

/** Returns a blocking reason for any provider that discovery cannot select. */
export function unavailableProviderReason(
  providers: DiscoverProvider[],
  providerIds: string[],
): string | null {
  const unavailable = [...new Set(providerIds.filter(Boolean))].filter((id) => {
    const option = getProviderOption(providers, id);
    return !option?.available;
  });
  if (unavailable.length === 0) return null;
  return `Unavailable or legacy provider transport${unavailable.length === 1 ? "" : "s"}: ${unavailable.join(", ")}. Choose an available transport before saving or running.`;
}

/** Returns a blocking reason when Governed mode cannot execute a transport. */
export function unsupportedGatewayTransportReason(
  providers: DiscoverProvider[],
  providerIds: string[],
): string | null {
  const unsupported = [...new Set(providerIds.filter(Boolean))].filter((id) => {
    const option = getProviderOption(providers, id);
    return option?.available === true && option.gateway_supported === false;
  });
  if (unsupported.length === 0) return null;
  return `Gateway has no adapter for transport${unsupported.length === 1 ? "" : "s"}: ${unsupported.join(", ")}. Choose a Gateway-supported transport or use Direct mode.`;
}
