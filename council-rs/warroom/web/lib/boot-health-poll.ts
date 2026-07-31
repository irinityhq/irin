/**
 * Readiness-driven boot health poll for packaged War Room cold start.
 *
 * Native pack resume + Council bind often take tens of seconds. The old
 * fixed one-shot window ([1.5s, 3s, 6s]) went offline before the backend
 * existed. This controller:
 * - stays in CONNECTING while the cold-start budget is open;
 * - polls with exponential backoff (not a tight loop);
 * - transitions to online as soon as a probe succeeds;
 * - after budget exhaustion, recovers slowly without hiding permanent failure;
 * - cleans up all timers and ignores stale in-flight probes on stop().
 */

export const BOOT_HEALTH_POLL = {
  /** Delay before the first retry after an immediate probe fails. */
  baseDelayMs: 1_500,
  /** Cap between retries so we do not spin aggressively. */
  maxDelayMs: 12_000,
  /**
   * Cold-start CONNECTING budget. Packaged Council + gateway-pack resume
   * commonly land after ~30–60s; keep headroom without eternal CONNECTING.
   */
  connectingBudgetMs: 180_000,
  /** Slow re-check while offline after the connecting budget ends. */
  recoveryIntervalMs: 15_000,
} as const;

export type BootHealthProbeResult = "ready" | "not_ready";

export type BootHealthPhase =
  | "idle"
  | "connecting"
  | "online"
  | "offline"
  | "recovering";

export type BootHealthPollHooks = {
  /** One health+cabinets (etc.) attempt. Resolve ready only when UI can go online. */
  probe: () => Promise<BootHealthProbeResult>;
  /** True while cold-start CONNECTING retries are active (header label). */
  onRetryActiveChange?: (active: boolean) => void;
  onPhaseChange?: (phase: BootHealthPhase) => void;
  now?: () => number;
  setTimeoutFn?: typeof setTimeout;
  clearTimeoutFn?: typeof clearTimeout;
  connectingBudgetMs?: number;
  recoveryIntervalMs?: number;
  baseDelayMs?: number;
  maxDelayMs?: number;
};

export type BootHealthPollHandle = {
  /** Begin cold-start polling (immediate probe + retries). Idempotent. */
  startConnecting: () => void;
  /** Begin slow recovery after offline. Idempotent. */
  startRecovery: () => void;
  /** Mark online without probing (e.g. parallel initial load already succeeded). */
  markOnline: () => void;
  /** Cancel timers and ignore in-flight probes (unmount). */
  stop: () => void;
  phase: () => BootHealthPhase;
  isRetryActive: () => boolean;
};

/** Delay before connecting-phase attempt `attemptIndex` (0 = first retry). */
export function bootHealthRetryDelayMs(
  attemptIndex: number,
  opts?: { baseDelayMs?: number; maxDelayMs?: number },
): number {
  const base = opts?.baseDelayMs ?? BOOT_HEALTH_POLL.baseDelayMs;
  const max = opts?.maxDelayMs ?? BOOT_HEALTH_POLL.maxDelayMs;
  const raw = base * 2 ** Math.max(0, attemptIndex);
  return Math.min(raw, max);
}

export function createBootHealthPoller(
  hooks: BootHealthPollHooks,
): BootHealthPollHandle {
  const now = hooks.now ?? (() => Date.now());
  const setTimeoutFn = hooks.setTimeoutFn ?? setTimeout;
  const clearTimeoutFn = hooks.clearTimeoutFn ?? clearTimeout;
  const connectingBudgetMs =
    hooks.connectingBudgetMs ?? BOOT_HEALTH_POLL.connectingBudgetMs;
  const recoveryIntervalMs =
    hooks.recoveryIntervalMs ?? BOOT_HEALTH_POLL.recoveryIntervalMs;
  const baseDelayMs = hooks.baseDelayMs ?? BOOT_HEALTH_POLL.baseDelayMs;
  const maxDelayMs = hooks.maxDelayMs ?? BOOT_HEALTH_POLL.maxDelayMs;

  let phase: BootHealthPhase = "idle";
  let stopped = false;
  let inFlight = false;
  let generation = 0;
  let attemptIndex = 0;
  let budgetStartedAt = 0;
  let retryActive = false;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const setRetryActive = (active: boolean) => {
    if (retryActive === active) return;
    retryActive = active;
    hooks.onRetryActiveChange?.(active);
  };

  const setPhase = (next: BootHealthPhase) => {
    if (phase === next) return;
    phase = next;
    hooks.onPhaseChange?.(next);
  };

  const clearTimer = () => {
    if (timer !== null) {
      clearTimeoutFn(timer);
      timer = null;
    }
  };

  const schedule = (delayMs: number, fn: () => void) => {
    clearTimer();
    timer = setTimeoutFn(() => {
      timer = null;
      fn();
    }, delayMs);
  };

  const becomeOnline = () => {
    clearTimer();
    setPhase("online");
    setRetryActive(false);
    attemptIndex = 0;
  };

  const becomeOfflineAndRecover = () => {
    setPhase("offline");
    setRetryActive(false);
    // Honest offline, then slow recovery without re-entering CONNECTING.
    scheduleRecoveryLoop();
  };

  const scheduleRecoveryLoop = () => {
    if (stopped) return;
    setPhase("recovering");
    schedule(recoveryIntervalMs, () => {
      void runProbe();
    });
  };

  const scheduleNextConnecting = () => {
    if (stopped) return;
    const elapsed = now() - budgetStartedAt;
    if (elapsed >= connectingBudgetMs) {
      becomeOfflineAndRecover();
      return;
    }
    const delay = bootHealthRetryDelayMs(attemptIndex, {
      baseDelayMs,
      maxDelayMs,
    });
    attemptIndex += 1;
    // Remaining budget may be shorter than the nominal delay.
    const remaining = Math.max(0, connectingBudgetMs - elapsed);
    const wait = Math.min(delay, remaining);
    setRetryActive(true);
    schedule(wait, () => {
      void runProbe();
    });
  };

  const runProbe = async () => {
    if (stopped || inFlight) return;
    inFlight = true;
    const gen = generation;
    try {
      const result = await hooks.probe();
      if (stopped || gen !== generation) return;

      if (result === "ready") {
        becomeOnline();
        return;
      }

      if (phase === "connecting") {
        scheduleNextConnecting();
        return;
      }

      if (phase === "recovering" || phase === "offline") {
        // Stay offline between recovery probes.
        setRetryActive(false);
        setPhase("recovering");
        scheduleRecoveryLoop();
      }
    } catch {
      if (stopped || gen !== generation) return;
      if (phase === "connecting") {
        scheduleNextConnecting();
      } else if (phase === "recovering" || phase === "offline") {
        setRetryActive(false);
        scheduleRecoveryLoop();
      }
    } finally {
      if (gen === generation) {
        inFlight = false;
      }
    }
  };

  return {
    startConnecting: () => {
      if (stopped) return;
      if (phase === "connecting") return;
      if (phase === "online") return;

      generation += 1;
      clearTimer();
      inFlight = false;
      attemptIndex = 0;
      budgetStartedAt = now();
      setPhase("connecting");
      setRetryActive(true);
      void runProbe();
    },

    startRecovery: () => {
      if (stopped) return;
      if (phase === "online" || phase === "connecting" || phase === "recovering") {
        return;
      }
      generation += 1;
      clearTimer();
      inFlight = false;
      setPhase("recovering");
      setRetryActive(false);
      void runProbe();
    },

    markOnline: () => {
      if (stopped) return;
      generation += 1;
      clearTimer();
      inFlight = false;
      becomeOnline();
    },

    stop: () => {
      stopped = true;
      generation += 1;
      clearTimer();
      inFlight = false;
      setRetryActive(false);
      setPhase("idle");
    },

    phase: () => phase,
    isRetryActive: () => retryActive,
  };
}
