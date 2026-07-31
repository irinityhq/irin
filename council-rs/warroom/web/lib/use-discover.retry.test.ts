import { afterEach, describe, expect, it, vi } from "vitest";
import type { DiscoverResponse } from "./types";
import {
  __getDiscoverSnapshotForTests,
  __loadDiscoverForTests,
  __resetDiscoverForTests,
  __subscribeDiscoverForTests,
  notifyDiscoverBackendReady,
} from "./use-discover";

function okPayload(name = "nvidia"): DiscoverResponse {
  return {
    providers: [
      {
        name,
        label: name,
        family: "test",
        transport: "api",
        available: true,
        gateway_supported: true,
        source: "test",
        env_hint: null,
        models: ["m"],
      },
    ],
    log: ["ok"],
  };
}

afterEach(() => {
  __resetDiscoverForTests(null);
});

describe("use-discover shared cache + retry", () => {
  it("retries an initial transient failure then populates cache without operator rescan", async () => {
    const sleep = vi.fn(async () => {});
    let calls = 0;
    const events: Array<{ data: unknown; loading: boolean; error: string | null }> = [];

    __resetDiscoverForTests({
      fetchOnce: async () => {
        calls += 1;
        if (calls === 1) throw new TypeError("Load failed");
        return okPayload();
      },
      retry: { sleep, maxAttempts: 4 },
    });

    const unsub = __subscribeDiscoverForTests((data, loading, error) => {
      events.push({ data, loading, error });
    });

    const loaded = await __loadDiscoverForTests();
    expect(loaded.providers[0]?.name).toBe("nvidia");
    expect(calls).toBe(2);
    expect(sleep).toHaveBeenCalledTimes(1);

    const snap = __getDiscoverSnapshotForTests();
    expect(snap.data?.providers[0]?.name).toBe("nvidia");
    expect(snap.error).toBeNull();
    expect(snap.loading).toBe(false);

    // Never left a permanent error sticky after eventual success.
    expect(events.some((e) => e.error && !e.loading && e.data)).toBe(false);
    expect(events[events.length - 1]).toMatchObject({
      loading: false,
      error: null,
    });
    expect(events[events.length - 1]?.data).toEqual(loaded);

    unsub();
  });

  it("shares one in-flight request across concurrent callers and listeners", async () => {
    let calls = 0;
    let release!: (value: DiscoverResponse) => void;
    const gate = new Promise<DiscoverResponse>((resolve) => {
      release = resolve;
    });

    __resetDiscoverForTests({
      fetchOnce: async () => {
        calls += 1;
        return gate;
      },
      retry: { sleep: async () => {}, maxAttempts: 1 },
    });

    const listenerA = vi.fn();
    const listenerB = vi.fn();
    const unsubA = __subscribeDiscoverForTests(listenerA);
    const unsubB = __subscribeDiscoverForTests(listenerB);

    const p1 = __loadDiscoverForTests();
    const p2 = __loadDiscoverForTests();
    expect(__getDiscoverSnapshotForTests().inFlight).toBe(true);
    expect(__getDiscoverSnapshotForTests().listenerCount).toBe(2);

    release(okPayload("shared"));
    const [r1, r2] = await Promise.all([p1, p2]);
    expect(calls).toBe(1);
    expect(r1).toBe(r2);
    expect(r1.providers[0]?.name).toBe("shared");

    // Both listeners saw the success payload (and no sticky error).
    expect(listenerA).toHaveBeenCalledWith(r1, false, null);
    expect(listenerB).toHaveBeenCalledWith(r1, false, null);

    unsubA();
    unsubB();
  });

  it("surfaces terminal HTTP failures immediately without retry delay", async () => {
    const sleep = vi.fn(async () => {});
    let calls = 0;
    __resetDiscoverForTests({
      fetchOnce: async () => {
        calls += 1;
        throw new Error("401 Unauthorized on /api/discover");
      },
      retry: { sleep, maxAttempts: 5 },
    });

    const events: Array<{ loading: boolean; error: string | null }> = [];
    const unsub = __subscribeDiscoverForTests((_d, loading, error) => {
      events.push({ loading, error });
    });

    await expect(__loadDiscoverForTests()).rejects.toThrow(/401 Unauthorized/);
    expect(calls).toBe(1);
    expect(sleep).not.toHaveBeenCalled();

    const snap = __getDiscoverSnapshotForTests();
    expect(snap.data).toBeNull();
    expect(snap.loading).toBe(false);
    expect(snap.error).toMatch(/401 Unauthorized/);

    const last = events[events.length - 1];
    expect(last).toMatchObject({ loading: false });
    expect(last?.error).toMatch(/401 Unauthorized/);

    unsub();
  });

  it("clears a prior error for every listener after a later success", async () => {
    const sleep = vi.fn(async () => {});
    let mode: "fail" | "ok" = "fail";
    __resetDiscoverForTests({
      fetchOnce: async () => {
        if (mode === "fail") throw new Error("500 boom on /api/discover");
        return okPayload("recovered");
      },
      retry: { sleep, maxAttempts: 1 },
    });

    const seen: Array<string | null> = [];
    const unsub = __subscribeDiscoverForTests((_d, _l, error) => {
      seen.push(error);
    });

    await expect(__loadDiscoverForTests()).rejects.toThrow(/500 boom/);
    expect(__getDiscoverSnapshotForTests().error).toMatch(/500 boom/);

    mode = "ok";
    // Force a second load (cache was cleared on terminal failure).
    const recovered = await __loadDiscoverForTests();
    expect(recovered.providers[0]?.name).toBe("recovered");
    expect(__getDiscoverSnapshotForTests().error).toBeNull();
    expect(seen[seen.length - 1]).toBeNull();

    unsub();
  });
});

describe("notifyDiscoverBackendReady (boot-health ownership)", () => {
  it("re-drives discover after late backend readiness and clears sticky Load failed", async () => {
    const sleep = vi.fn(async () => {});
    let calls = 0;
    let backendReady = false;

    __resetDiscoverForTests({
      fetchOnce: async () => {
        calls += 1;
        if (!backendReady) throw new TypeError("Load failed");
        return okPayload("late-ready");
      },
      // Exhaust short cold-start burst before Council binds (packaged reality).
      retry: { sleep, maxAttempts: 2 },
    });

    const events: Array<{ data: unknown; loading: boolean; error: string | null }> =
      [];
    const unsub = __subscribeDiscoverForTests((data, loading, error) => {
      events.push({ data, loading, error });
    });

    await expect(__loadDiscoverForTests()).rejects.toThrow(/Load failed/);
    expect(calls).toBe(2);
    expect(__getDiscoverSnapshotForTests().error).toMatch(/Load failed/);
    expect(__getDiscoverSnapshotForTests().data).toBeNull();
    expect(__getDiscoverSnapshotForTests().loading).toBe(false);

    // Boot poller transitions to online after delayed Council bind.
    backendReady = true;
    notifyDiscoverBackendReady();
    // Wait for the readiness-owned re-drive to settle fully (not mid-load
    // where error is cleared optimistically while loading=true).
    await vi.waitFor(() => {
      const snap = __getDiscoverSnapshotForTests();
      expect(snap.loading).toBe(false);
      expect(snap.error).toBeNull();
      expect(snap.data?.providers[0]?.name).toBe("late-ready");
    });

    const snap = __getDiscoverSnapshotForTests();
    expect(snap.data?.providers[0]?.name).toBe("late-ready");
    expect(snap.error).toBeNull();
    expect(calls).toBe(3);
    // Stale convene-blocker string must not remain after success.
    expect(events[events.length - 1]).toMatchObject({
      loading: false,
      error: null,
    });
    expect(events[events.length - 1]?.data).toEqual(snap.data);

    unsub();
  });

  it("is a no-op when inventory is already healthy (no duplicate fetch)", async () => {
    const sleep = vi.fn(async () => {});
    let calls = 0;
    __resetDiscoverForTests({
      fetchOnce: async () => {
        calls += 1;
        return okPayload("cached");
      },
      retry: { sleep, maxAttempts: 1 },
    });

    await __loadDiscoverForTests();
    expect(calls).toBe(1);
    expect(__getDiscoverSnapshotForTests().error).toBeNull();

    notifyDiscoverBackendReady();
    notifyDiscoverBackendReady();
    await Promise.resolve();
    expect(calls).toBe(1);
    expect(__getDiscoverSnapshotForTests().data?.providers[0]?.name).toBe(
      "cached",
    );
  });

  it("shares an in-flight load instead of starting a duplicate", async () => {
    let calls = 0;
    let release!: (value: DiscoverResponse) => void;
    const gate = new Promise<DiscoverResponse>((resolve) => {
      release = resolve;
    });

    __resetDiscoverForTests({
      fetchOnce: async () => {
        calls += 1;
        return gate;
      },
      retry: { sleep: async () => {}, maxAttempts: 1 },
    });

    const p = __loadDiscoverForTests();
    expect(__getDiscoverSnapshotForTests().inFlight).toBe(true);

    notifyDiscoverBackendReady();
    notifyDiscoverBackendReady();
    expect(calls).toBe(1);

    release(okPayload("shared-ready"));
    await p;
    expect(calls).toBe(1);
    expect(__getDiscoverSnapshotForTests().error).toBeNull();
  });

  it("re-drives when readiness arrives during an in-flight request that fails", async () => {
    let calls = 0;
    let rejectFirst!: (reason: Error) => void;
    const first = new Promise<DiscoverResponse>((_resolve, reject) => {
      rejectFirst = reject;
    });

    __resetDiscoverForTests({
      fetchOnce: async () => {
        calls += 1;
        if (calls === 1) return first;
        return okPayload("ready-after-shared-failure");
      },
      retry: { sleep: async () => {}, maxAttempts: 1 },
    });

    const load = __loadDiscoverForTests();
    notifyDiscoverBackendReady();
    expect(calls).toBe(1);
    rejectFirst(new TypeError("Load failed"));
    await expect(load).rejects.toThrow(/Load failed/);

    await vi.waitFor(() => {
      const snapshot = __getDiscoverSnapshotForTests();
      expect(snapshot.loading).toBe(false);
      expect(snapshot.error).toBeNull();
      expect(snapshot.data?.providers[0]?.name).toBe(
        "ready-after-shared-failure",
      );
    });
    expect(calls).toBe(2);
  });

  it("recovers after a later readiness signal when a prior readiness re-drive failed", async () => {
    const sleep = vi.fn(async () => {});
    let calls = 0;
    let allow = false;

    __resetDiscoverForTests({
      fetchOnce: async () => {
        calls += 1;
        if (!allow) throw new TypeError("Load failed");
        return okPayload("after-loss");
      },
      retry: { sleep, maxAttempts: 1 },
    });

    await expect(__loadDiscoverForTests()).rejects.toThrow(/Load failed/);
    expect(__getDiscoverSnapshotForTests().error).toMatch(/Load failed/);

    // First readiness pulse still too early (backend flapped).
    notifyDiscoverBackendReady();
    await vi.waitFor(() => {
      const snap = __getDiscoverSnapshotForTests();
      expect(snap.loading).toBe(false);
      expect(snap.error).toMatch(/Load failed/);
    });
    expect(calls).toBe(2);

    // Slow recovery later: second online transition clears the sticky failure.
    allow = true;
    notifyDiscoverBackendReady();
    await vi.waitFor(() => {
      const snap = __getDiscoverSnapshotForTests();
      expect(snap.loading).toBe(false);
      expect(snap.error).toBeNull();
      expect(snap.data?.providers[0]?.name).toBe("after-loss");
    });
    expect(calls).toBe(3);
  });
});
