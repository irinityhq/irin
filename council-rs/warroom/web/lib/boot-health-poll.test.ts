import { afterEach, describe, expect, it, vi } from "vitest";
import {
  BOOT_HEALTH_POLL,
  bootHealthRetryDelayMs,
  createBootHealthPoller,
  type BootHealthPhase,
  type BootHealthProbeResult,
} from "./boot-health-poll";

describe("bootHealthRetryDelayMs", () => {
  it("grows exponentially and caps (beyond the old 6s one-shot window)", () => {
    expect(bootHealthRetryDelayMs(0)).toBe(BOOT_HEALTH_POLL.baseDelayMs);
    expect(bootHealthRetryDelayMs(1)).toBe(BOOT_HEALTH_POLL.baseDelayMs * 2);
    expect(bootHealthRetryDelayMs(2)).toBe(BOOT_HEALTH_POLL.baseDelayMs * 4);
    // Old window last retry was absolute 6s; new schedule continues past that.
    expect(bootHealthRetryDelayMs(3)).toBe(BOOT_HEALTH_POLL.baseDelayMs * 8);
    expect(bootHealthRetryDelayMs(10)).toBe(BOOT_HEALTH_POLL.maxDelayMs);
  });
});

describe("createBootHealthPoller", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  function installFakeClock(startMs = 0) {
    vi.useFakeTimers();
    let now = startMs;
    vi.setSystemTime(startMs);
    return {
      advance: async (ms: number) => {
        now += ms;
        vi.setSystemTime(now);
        await vi.advanceTimersByTimeAsync(ms);
      },
      now: () => now,
    };
  }

  it("keeps polling past the old 6s window and becomes online when ready late", async () => {
    const clock = installFakeClock();
    const phases: BootHealthPhase[] = [];
    const retryFlags: boolean[] = [];
    let calls = 0;

    // Fail until well past the historical 6s absolute retry ceiling.
    const readyAfterMs = 45_000;
    const poller = createBootHealthPoller({
      now: clock.now,
      connectingBudgetMs: 120_000,
      probe: async () => {
        calls += 1;
        return clock.now() >= readyAfterMs
          ? ("ready" as BootHealthProbeResult)
          : ("not_ready" as BootHealthProbeResult);
      },
      onPhaseChange: (p) => phases.push(p),
      onRetryActiveChange: (a) => retryFlags.push(a),
    });

    poller.startConnecting();
    // Flush the immediate probe.
    await vi.advanceTimersByTimeAsync(0);
    expect(poller.phase()).toBe("connecting");
    expect(poller.isRetryActive()).toBe(true);
    expect(calls).toBeGreaterThanOrEqual(1);

    // Advance past the old one-shot window; must still be connecting, not offline.
    await clock.advance(8_000);
    expect(poller.phase()).toBe("connecting");
    expect(poller.isRetryActive()).toBe(true);
    expect(phases).not.toContain("offline");

    // Reach delayed readiness.
    await clock.advance(40_000);
    // Allow any scheduled probe at the ready threshold to complete.
    await vi.advanceTimersByTimeAsync(0);
    expect(poller.phase()).toBe("online");
    expect(poller.isRetryActive()).toBe(false);
    expect(calls).toBeGreaterThan(3);
    expect(phases).toContain("online");

    const callsAtOnline = calls;
    // No further polling once online.
    await clock.advance(30_000);
    expect(calls).toBe(callsAtOnline);

    poller.stop();
  });

  it("cleans up timers on stop and ignores late probe results", async () => {
    const clock = installFakeClock();
    const resolvers: Array<(r: BootHealthProbeResult) => void> = [];
    let calls = 0;
    const poller = createBootHealthPoller({
      now: clock.now,
      connectingBudgetMs: 60_000,
      probe: () =>
        new Promise<BootHealthProbeResult>((resolve) => {
          calls += 1;
          resolvers.push(resolve);
        }),
    });

    poller.startConnecting();
    await vi.advanceTimersByTimeAsync(0);
    expect(calls).toBe(1);
    expect(poller.isRetryActive()).toBe(true);
    expect(resolvers).toHaveLength(1);

    poller.stop();
    expect(poller.phase()).toBe("idle");
    expect(poller.isRetryActive()).toBe(false);

    // Late probe must not resurrect connecting or schedule more work.
    resolvers[0]!("not_ready");
    await vi.advanceTimersByTimeAsync(0);
    await clock.advance(30_000);
    expect(calls).toBe(1);
    expect(poller.phase()).toBe("idle");
  });

  it("does not start a duplicate connecting loop", async () => {
    const clock = installFakeClock();
    let calls = 0;
    const poller = createBootHealthPoller({
      now: clock.now,
      connectingBudgetMs: 60_000,
      baseDelayMs: 1_000,
      maxDelayMs: 1_000,
      probe: async () => {
        calls += 1;
        return "not_ready";
      },
    });

    poller.startConnecting();
    poller.startConnecting();
    poller.startConnecting();
    await vi.advanceTimersByTimeAsync(0);
    expect(calls).toBe(1);

    await clock.advance(1_000);
    expect(calls).toBe(2);

    poller.stop();
  });

  it("recovers to online after a transient offline window", async () => {
    const clock = installFakeClock();
    let calls = 0;
    const poller = createBootHealthPoller({
      now: clock.now,
      connectingBudgetMs: 5_000,
      recoveryIntervalMs: 2_000,
      baseDelayMs: 1_000,
      maxDelayMs: 1_000,
      probe: async () => {
        calls += 1;
        // First connecting budget exhausts as not_ready; later recovery succeeds.
        return clock.now() >= 9_000
          ? ("ready" as BootHealthProbeResult)
          : ("not_ready" as BootHealthProbeResult);
      },
    });

    poller.startConnecting();
    await vi.advanceTimersByTimeAsync(0);

    // Exhaust connecting budget → offline/recovering.
    await clock.advance(5_000);
    await vi.advanceTimersByTimeAsync(0);
    expect(["offline", "recovering"]).toContain(poller.phase());
    // Permanent failure is not hidden as CONNECTING.
    expect(poller.isRetryActive()).toBe(false);

    // Slow recovery eventually finds readiness.
    await clock.advance(6_000);
    await vi.advanceTimersByTimeAsync(0);
    expect(poller.phase()).toBe("online");
    expect(poller.isRetryActive()).toBe(false);
    expect(calls).toBeGreaterThan(3);

    poller.stop();
  });

  it("markOnline short-circuits an in-flight connecting schedule", async () => {
    const clock = installFakeClock();
    let calls = 0;
    const poller = createBootHealthPoller({
      now: clock.now,
      connectingBudgetMs: 60_000,
      probe: async () => {
        calls += 1;
        return "not_ready";
      },
    });

    poller.startConnecting();
    await vi.advanceTimersByTimeAsync(0);
    expect(poller.phase()).toBe("connecting");

    poller.markOnline();
    expect(poller.phase()).toBe("online");
    expect(poller.isRetryActive()).toBe(false);

    const atMark = calls;
    await clock.advance(20_000);
    expect(calls).toBe(atMark);

    poller.stop();
  });
});
