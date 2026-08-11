import { describe, expect, it, vi } from "vitest";
import { createBootHealthPoller } from "./boot-health-poll";

async function flushMicrotasks() {
  await Promise.resolve();
  await Promise.resolve();
}

describe("createBootHealthPoller restart", () => {
  it("re-arms polling when startConnecting is called while online", async () => {
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

    poller.startConnecting();
    await flushMicrotasks();
    expect(probe).toHaveBeenCalledTimes(2);
    expect(phases.at(-1)).toBe("connecting");

    timers.shift()?.();
    await flushMicrotasks();
    expect(probe).toHaveBeenCalledTimes(3);
    expect(phases.at(-1)).toBe("online");
  });
});
