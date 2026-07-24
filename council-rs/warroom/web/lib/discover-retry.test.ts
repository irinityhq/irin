import { describe, expect, it, vi } from "vitest";
import {
  DISCOVER_RETRY,
  discoverRetryDelayMs,
  fetchWithDiscoverRetry,
  isTransientDiscoverError,
} from "./discover-retry";

describe("isTransientDiscoverError", () => {
  it("treats network / cold-start failures as transient", () => {
    expect(isTransientDiscoverError(new TypeError("Load failed"))).toBe(true);
    expect(isTransientDiscoverError(new Error("Load failed"))).toBe(true);
    expect(isTransientDiscoverError(new Error("Failed to fetch"))).toBe(true);
    expect(isTransientDiscoverError(new Error("NetworkError when attempting to fetch resource."))).toBe(
      true,
    );
    expect(isTransientDiscoverError(new Error("connect ECONNREFUSED 127.0.0.1:8765"))).toBe(
      true,
    );
  });

  it("treats reverse-proxy startup statuses as transient", () => {
    expect(isTransientDiscoverError(new Error("503 discovery temporarily unavailable on /api/discover"))).toBe(
      true,
    );
    expect(isTransientDiscoverError(new Error("502 Bad Gateway on /api/discover"))).toBe(true);
    expect(isTransientDiscoverError(new Error("504 Gateway Timeout on /api/discover"))).toBe(true);
  });

  it("treats real HTTP application failures as terminal", () => {
    expect(isTransientDiscoverError(new Error("401 Unauthorized on /api/discover"))).toBe(false);
    expect(isTransientDiscoverError(new Error("400 Bad Request on /api/discover"))).toBe(false);
    expect(isTransientDiscoverError(new Error("404 Not Found on /api/discover"))).toBe(false);
    expect(isTransientDiscoverError(new Error("500 Internal Server Error on /api/discover"))).toBe(
      false,
    );
  });
});

describe("discoverRetryDelayMs", () => {
  it("grows exponentially and caps", () => {
    expect(discoverRetryDelayMs(0)).toBe(DISCOVER_RETRY.baseDelayMs);
    expect(discoverRetryDelayMs(1)).toBe(DISCOVER_RETRY.baseDelayMs * 2);
    expect(discoverRetryDelayMs(2)).toBe(DISCOVER_RETRY.baseDelayMs * 4);
    expect(discoverRetryDelayMs(10)).toBe(DISCOVER_RETRY.maxDelayMs);
  });
});

describe("fetchWithDiscoverRetry", () => {
  it("retries transient failures then succeeds and clears the path", async () => {
    const sleep = vi.fn(async () => {});
    const onRetry = vi.fn();
    let calls = 0;
    const result = await fetchWithDiscoverRetry(
      async () => {
        calls += 1;
        if (calls < 3) throw new TypeError("Load failed");
        return { ok: true, attempt: calls };
      },
      { sleep, onRetry, maxAttempts: 5 },
    );

    expect(result).toEqual({ ok: true, attempt: 3 });
    expect(calls).toBe(3);
    expect(onRetry).toHaveBeenCalledTimes(2);
    expect(sleep).toHaveBeenCalledTimes(2);
    expect(sleep).toHaveBeenNthCalledWith(1, discoverRetryDelayMs(0));
    expect(sleep).toHaveBeenNthCalledWith(2, discoverRetryDelayMs(1));
  });

  it("does not retry terminal HTTP failures", async () => {
    const sleep = vi.fn(async () => {});
    let calls = 0;
    await expect(
      fetchWithDiscoverRetry(
        async () => {
          calls += 1;
          throw new Error("401 Unauthorized on /api/discover");
        },
        { sleep, maxAttempts: 5 },
      ),
    ).rejects.toThrow(/401 Unauthorized/);
    expect(calls).toBe(1);
    expect(sleep).not.toHaveBeenCalled();
  });

  it("surfaces the last error after exhausting transient retries", async () => {
    const sleep = vi.fn(async () => {});
    let calls = 0;
    await expect(
      fetchWithDiscoverRetry(
        async () => {
          calls += 1;
          throw new Error("Load failed");
        },
        { sleep, maxAttempts: 3 },
      ),
    ).rejects.toThrow(/Load failed/);
    expect(calls).toBe(3);
    expect(sleep).toHaveBeenCalledTimes(2);
  });
});
