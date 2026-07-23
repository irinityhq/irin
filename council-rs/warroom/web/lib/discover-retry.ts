/**
 * Bounded retry policy for `GET /api/discover` cold-start races.
 *
 * Packaged War Room often hits discover before Council finishes binding.
 * Live `/api/discover` can take several seconds cold; a single failed
 * fetch must not stick as a permanent UI error until the operator hits Rescan.
 */

export const DISCOVER_RETRY = {
  /** Inclusive of the first attempt. */
  maxAttempts: 6,
  baseDelayMs: 400,
  maxDelayMs: 3200,
} as const;

/**
 * Transient = fetch/network/startup only. Real HTTP application failures
 * (4xx, most 5xx) surface immediately so operators are not left waiting.
 */
export function isTransientDiscoverError(err: unknown): boolean {
  if (err instanceof TypeError) return true;

  const msg = err instanceof Error ? err.message : String(err);
  if (!msg.trim()) return true;

  // Our api.get shapes errors as `${status} ${detail} on ${path}`.
  const statusMatch = msg.match(/^(\d{3})\b/);
  if (statusMatch) {
    const status = Number(statusMatch[1]);
    // Gateway / reverse-proxy / process-not-ready styles only.
    return status === 502 || status === 503 || status === 504;
  }

  return (
    /load failed/i.test(msg) ||
    /failed to fetch/i.test(msg) ||
    /networkerror/i.test(msg) ||
    /network error/i.test(msg) ||
    /network request failed/i.test(msg) ||
    /econnrefused/i.test(msg) ||
    /econnreset/i.test(msg) ||
    /etimedout/i.test(msg) ||
    /socket hang up/i.test(msg) ||
    /connection refused/i.test(msg) ||
    /connection reset/i.test(msg) ||
    /timed out/i.test(msg) ||
    /timeout/i.test(msg)
  );
}

/** Delay before attempt `attemptIndex` (0 = first retry after a failure). */
export function discoverRetryDelayMs(attemptIndex: number): number {
  const raw = DISCOVER_RETRY.baseDelayMs * 2 ** Math.max(0, attemptIndex);
  return Math.min(raw, DISCOVER_RETRY.maxDelayMs);
}

export type DiscoverRetryHooks = {
  maxAttempts?: number;
  /** Injected for deterministic tests; defaults to real wall-clock sleep. */
  sleep?: (ms: number) => Promise<void>;
  isTransient?: (err: unknown) => boolean;
  /** Called after each failed attempt that will be retried (1-based attempt that failed). */
  onRetry?: (info: { attempt: number; delayMs: number; error: unknown }) => void;
};

function defaultSleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

/**
 * Run `fetchOnce` with bounded backoff on transient errors only.
 * Terminal failures throw immediately (or after the final transient attempt).
 */
export async function fetchWithDiscoverRetry<T>(
  fetchOnce: () => Promise<T>,
  hooks: DiscoverRetryHooks = {},
): Promise<T> {
  const maxAttempts = hooks.maxAttempts ?? DISCOVER_RETRY.maxAttempts;
  const sleep = hooks.sleep ?? defaultSleep;
  const isTransient = hooks.isTransient ?? isTransientDiscoverError;

  let lastError: unknown;
  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    try {
      return await fetchOnce();
    } catch (err) {
      lastError = err;
      const canRetry = attempt < maxAttempts && isTransient(err);
      if (!canRetry) throw err;
      const delayMs = discoverRetryDelayMs(attempt - 1);
      hooks.onRetry?.({ attempt, delayMs, error: err });
      await sleep(delayMs);
    }
  }
  throw lastError instanceof Error
    ? lastError
    : new Error(String(lastError ?? "discover retry exhausted"));
}
