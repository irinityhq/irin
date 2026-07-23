"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "@/lib/api";
import { normalizeDiscoverResponse } from "./discover";
import {
  fetchWithDiscoverRetry,
  type DiscoverRetryHooks,
} from "./discover-retry";
import type { DiscoverProvider, DiscoverResponse } from "@/lib/types";

// Module-level cache and listeners for deduped fetches across components.
let cache: DiscoverResponse | null = null;
let inFlight: Promise<DiscoverResponse> | null = null;
let lastLoading = false;
let lastError: string | null = null;
const listeners = new Set<
  (data: DiscoverResponse | null, loading: boolean, error: string | null) => void
>();

/** Test/diag seam — not part of the product UI contract. */
export type DiscoverFetchDeps = {
  fetchOnce?: () => Promise<DiscoverResponse>;
  retry?: DiscoverRetryHooks;
};

let testDeps: DiscoverFetchDeps | null = null;

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

function notify(
  data: DiscoverResponse | null,
  loading: boolean,
  error: string | null,
) {
  lastLoading = loading;
  lastError = error;
  listeners.forEach((l) => l(data, loading, error));
}

async function runDiscoverFetch(): Promise<DiscoverResponse> {
  const fetchOnce =
    testDeps?.fetchOnce ??
    (async () => {
      const raw = await api.discover();
      return normalizeDiscoverResponse(raw);
    });

  const normalized = await fetchWithDiscoverRetry(fetchOnce, testDeps?.retry);
  cache = normalized;
  notify(cache, false, null);
  return normalized;
}

/**
 * Shared discover load. Concurrent callers share one in-flight promise.
 * Transient cold-start failures retry with backoff; terminal HTTP errors
 * surface immediately. Successful completion clears the error for every listener.
 */
async function doFetch(): Promise<DiscoverResponse> {
  if (inFlight) return inFlight;
  inFlight = (async () => {
    notify(cache, true, null);
    try {
      return await runDiscoverFetch();
    } catch (err) {
      const msg = errorMessage(err);
      // Terminal (or exhausted) failure: drop stale optimistic data so the UI
      // does not show a previous scan as current.
      cache = null;
      notify(null, false, msg);
      throw err;
    } finally {
      inFlight = null;
    }
  })();
  return inFlight;
}

export function useDiscover() {
  const [data, setData] = useState<DiscoverResponse | null>(cache);
  const [loading, setLoading] = useState<boolean>(lastLoading || !!inFlight || !cache);
  const [error, setError] = useState<string | null>(lastError);

  const listener = useCallback(
    (d: DiscoverResponse | null, l: boolean, e: string | null) => {
      setData(d);
      setLoading(l);
      setError(e);
    },
    [],
  );

  useEffect(() => {
    listeners.add(listener);
    // Sync late subscribers to the shared module snapshot (cache / in-flight / error).
    if (cache) {
      setData(cache);
      setLoading(!!inFlight);
      setError(inFlight ? null : lastError);
    } else if (inFlight) {
      setLoading(true);
      setError(null);
    } else {
      // No cache and idle: start a shared load (covers first mount and a
      // remount after a prior terminal failure — still one in-flight request).
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
      // Wait for any shared in-flight request, then force a fresh scan.
      if (inFlight) {
        try {
          await inFlight;
        } catch {
          // previous attempt failed; continue into a fresh rescan below
        }
      }
      // Clear cache so doFetch actually re-hits the network.
      cache = null;
      await doFetch();
    } catch {
      // error already notified
    } finally {
      setLoading(lastLoading);
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

// --- Test seams (deterministic unit coverage for cache / retry / listeners) ---

/** Reset module cache, in-flight, listeners, and optional fetch deps. */
export function __resetDiscoverForTests(deps: DiscoverFetchDeps | null = null): void {
  cache = null;
  inFlight = null;
  lastLoading = false;
  lastError = null;
  listeners.clear();
  testDeps = deps;
}

export function __getDiscoverSnapshotForTests(): {
  data: DiscoverResponse | null;
  loading: boolean;
  error: string | null;
  inFlight: boolean;
  listenerCount: number;
} {
  return {
    data: cache,
    loading: lastLoading,
    error: lastError,
    inFlight: !!inFlight,
    listenerCount: listeners.size,
  };
}

/** Drive a shared load without mounting React (unit tests). */
export function __loadDiscoverForTests(): Promise<DiscoverResponse> {
  return doFetch();
}

export function __subscribeDiscoverForTests(
  listener: (data: DiscoverResponse | null, loading: boolean, error: string | null) => void,
): () => void {
  listeners.add(listener);
  listener(cache, lastLoading || !!inFlight, lastError);
  return () => {
    listeners.delete(listener);
  };
}
