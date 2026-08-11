/**
 * War Room backend readiness controller (production effect body).
 *
 * Owns: cold-start boot poll, packaged native-ownership gate, and the
 * config-change → probe → re-arm path. WarRoom mounts this once; tests drive
 * the same function with faithful Tauri / probe mocks.
 */

import {
  createBootHealthPoller,
  reconcileBootHealthAfterConfigChange,
  type BootHealthPollHandle,
  type BootHealthPollHooks,
} from "./boot-health-poll";
import { councilPortFromApiBase, subscribeWarroomConfigChanged } from "./runtime-config";

export type WarRoomStartupConfig = {
  apiBase: string;
  authToken: string;
  librarianBase: string;
};

export type WarRoomBackendReadyDeps = {
  loadInitialState: () => Promise<boolean>;
  isTauri: () => boolean;
  nativeOwnsCouncilStartup: () => Promise<boolean>;
  startCouncilServer: (
    port: number,
    authToken: string,
    librarianBase?: string,
  ) => Promise<unknown>;
  /** Resolves once for the Tauri sidecar / native-ownership branch. */
  getConfigForStartup: () => Promise<WarRoomStartupConfig>;
  initRuntimeConfig: () => void;
  onRetryActiveChange?: (active: boolean) => void;
  onDiscoverBackendReady?: () => void;
  /** Injectable poller clocks/timers for deterministic tests. */
  pollerOptions?: Partial<
    Pick<
      BootHealthPollHooks,
      | "now"
      | "setTimeoutFn"
      | "clearTimeoutFn"
      | "connectingBudgetMs"
      | "recoveryIntervalMs"
      | "baseDelayMs"
      | "maxDelayMs"
    >
  >;
};

export type WarRoomBackendReadyHandle = {
  stop: () => void;
  poller: () => BootHealthPollHandle;
};

/**
 * Start the production backend-readiness effect. Caller must `stop()` on unmount.
 */
export function startWarRoomBackendReady(
  deps: WarRoomBackendReadyDeps,
): WarRoomBackendReadyHandle {
  deps.initRuntimeConfig();
  let aborted = false;
  let sidecarAutoStarted = false;
  /**
   * Monotonic generation for mount + config-change reconciliations.
   * A newer reconciliation supersedes in-flight older probes so a late
   * mount `ready` cannot markOnline after a config-change force-rearm.
   */
  let reconcileGen = 0;

  const poller = createBootHealthPoller({
    ...deps.pollerOptions,
    probe: async () => {
      const ready = await deps.loadInitialState();
      return ready ? "ready" : "not_ready";
    },
    onRetryActiveChange: (active) => {
      if (!aborted) deps.onRetryActiveChange?.(active);
    },
    onPhaseChange: (phase) => {
      if (!aborted && phase === "online") {
        deps.onDiscoverBackendReady?.();
      }
    },
  });

  const scheduleBootHealthRetries = () => {
    if (aborted) return;
    poller.startConnecting();
  };

  // Browser / fast path: one immediate probe. Packaged Tauri continues via
  // scheduleBootHealthRetries while native owns Council startup.
  const mountGen = ++reconcileGen;
  void deps.loadInitialState().then((ready) => {
    if (aborted || mountGen !== reconcileGen) return;
    if (ready) {
      poller.markOnline();
    }
  });

  void deps.getConfigForStartup().then((cfg) => {
    // Strict Mode / unmount may resolve after stop() — do not touch native IPC.
    if (aborted) return;
    if (!deps.isTauri() || sidecarAutoStarted) return;
    sidecarAutoStarted = true;

    // Packaged install: native setup is the sole Council startup owner.
    // Frontend only polls/retries health — never startCouncilServer (would
    // force Direct via_gateway=None and race the governed restore).
    void deps
      .nativeOwnsCouncilStartup()
      .then((nativeOwns) => {
        if (aborted) return;
        if (nativeOwns) {
          scheduleBootHealthRetries();
          return;
        }
        void deps
          .startCouncilServer(
            councilPortFromApiBase(cfg.apiBase),
            cfg.authToken,
            cfg.librarianBase || undefined,
          )
          .then(() => {
            if (!aborted) scheduleBootHealthRetries();
          })
          .catch(() => {
            // Still poll health; source-dev start can fail transiently.
            if (!aborted) scheduleBootHealthRetries();
          });
      })
      .catch(() => {
        // Command missing on older shells: keep source-dev start path.
        if (aborted) return;
        void deps
          .startCouncilServer(
            councilPortFromApiBase(cfg.apiBase),
            cfg.authToken,
            cfg.librarianBase || undefined,
          )
          .then(() => {
            if (!aborted) scheduleBootHealthRetries();
          })
          .catch(() => {
            if (!aborted) scheduleBootHealthRetries();
          });
      });
  });

  // Only War Room owns backend readiness re-arm on config change.
  const unsubConfig = subscribeWarroomConfigChanged(() => {
    const gen = ++reconcileGen;
    void deps.loadInitialState().then((ready) => {
      if (aborted || gen !== reconcileGen) return;
      reconcileBootHealthAfterConfigChange(poller, ready, {
        forceRearmOnFailure: deps.isTauri(),
      });
    });
  });

  return {
    stop: () => {
      aborted = true;
      // Bump generation so any in-flight mount/config probes are ignored.
      reconcileGen += 1;
      poller.stop();
      unsubConfig();
    },
    poller: () => poller,
  };
}
