import { describe, expect, it, vi } from "vitest";
import { createBootHealthPoller } from "./boot-health-poll";

async function flushMicrotasks() {
  await Promise.resolve();
  await Promise.resolve();
}

describe("createBootHealthPoller restart", () => {
  // Force-only re-arm keeps cold-start markOnline races sticky-online.
  it("does not re-arm from online without force (cold-start race)", async () => {
    const phases: string[] = [];
    const probe = vi
      .fn<Parameters<typeof createBootHealthPoller>[0]["probe"]>()
      .mockResolvedValue("ready");

    const poller = createBootHealthPoller({
      baseDelayMs: 1,
      maxDelayMs: 1,
      connectingBudgetMs: 100,
      recoveryIntervalMs: 1,
      now: () => 0,
      probe,
      onPhaseChange: (phase) => phases.push(phase),
      setTimeoutFn: (() => 1) as unknown as typeof setTimeout,
      clearTimeoutFn: () => undefined,
    });

    poller.startConnecting();
    await flushMicrotasks();
    expect(probe).toHaveBeenCalledTimes(1);
    expect(phases.at(-1)).toBe("online");

    // Plain schedule after markOnline / first ready must stay a no-op.
    poller.startConnecting();
    await flushMicrotasks();
    expect(probe).toHaveBeenCalledTimes(1);
    expect(phases.at(-1)).toBe("online");
    expect(poller.isRetryActive()).toBe(false);

    poller.stop();
  });

  it("re-arms polling when startConnecting is forced while online", async () => {
    const phases: string[] = [];
    const timers: Array<() => void> = [];
    const probe = vi
      .fn<Parameters<typeof createBootHealthPoller>[0]["probe"]>()
      .mockResolvedValueOnce("ready")
      .mockResolvedValueOnce("not_ready")
      .mockResolvedValueOnce("ready");

    const setTimeoutFn = ((callback: () => void) => {
      timers.push(callback);
      return timers.length;
    }) as unknown as typeof setTimeout;

    const poller = createBootHealthPoller({
      baseDelayMs: 1,
      maxDelayMs: 1,
      connectingBudgetMs: 100,
      recoveryIntervalMs: 1,
      now: () => 0,
      probe,
      onPhaseChange: (phase) => phases.push(phase),
      onRetryActiveChange: () => undefined,
      setTimeoutFn,
      clearTimeoutFn: () => undefined,
    });

    poller.startConnecting();
    await flushMicrotasks();
    expect(probe).toHaveBeenCalledTimes(1);
    expect(phases.at(-1)).toBe("online");

    poller.startConnecting({ force: true });
    await flushMicrotasks();
    expect(probe).toHaveBeenCalledTimes(2);
    expect(phases.at(-1)).toBe("connecting");

    timers.shift()?.();
    await flushMicrotasks();
    expect(probe).toHaveBeenCalledTimes(3);
    expect(phases.at(-1)).toBe("online");

    poller.stop();
  });

  it("force while connecting invalidates the in-flight probe (config change)", async () => {
    const resolvers: Array<(v: "ready" | "not_ready") => void> = [];
    const probe = vi
      .fn<Parameters<typeof createBootHealthPoller>[0]["probe"]>()
      .mockImplementation(
        () =>
          new Promise((resolve) => {
            resolvers.push(resolve);
          }),
      );

    const poller = createBootHealthPoller({
      baseDelayMs: 1,
      maxDelayMs: 1,
      connectingBudgetMs: 100,
      recoveryIntervalMs: 1,
      now: () => 0,
      probe,
      setTimeoutFn: (() => 1) as unknown as typeof setTimeout,
      clearTimeoutFn: () => undefined,
    });

    poller.startConnecting();
    expect(probe).toHaveBeenCalledTimes(1);

    // Config change re-arms while the first probe is still in flight.
    poller.startConnecting({ force: true });
    expect(probe).toHaveBeenCalledTimes(2);

    // The superseded probe resolving ready must not mark the app online.
    resolvers[0]?.("ready");
    await flushMicrotasks();
    expect(poller.phase()).toBe("connecting");

    // The fresh-generation probe still governs.
    resolvers[1]?.("ready");
    await flushMicrotasks();
    expect(poller.phase()).toBe("online");

    poller.stop();
  });
});
