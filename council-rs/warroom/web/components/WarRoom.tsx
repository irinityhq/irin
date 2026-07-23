"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle, ArrowLeft, RotateCcw } from "lucide-react";
import { useDeliberation } from "@/hooks/useDeliberation";
import { api, apiBase } from "@/lib/api";
import {
  councilPortFromApiBase,
  configReady,
  initRuntimeConfig,
  loadRuntimeConfig,
} from "@/lib/runtime-config";
import {
  isTauri,
  reportCouncilRuntimeReady,
  startCouncilServer,
} from "@/lib/tauri";
import { cn } from "@/lib/cn";
import type { Cabinet, HealthResponse } from "@/lib/types";
import type { StartPayload } from "@/lib/ws";
import DeliberateWorkspace from "./DeliberateWorkspace";
import DirectFirePanel from "./DirectFirePanel";
import DiscoverPanel from "./DiscoverPanel";
import LiveAnalytics from "./LiveAnalytics";
import SessionExplorer from "./SessionExplorer";
import CabinetEditor from "./CabinetEditor";
import PatternsView from "./PatternsView";
import DriftView from "./DriftView";
import MetaReviewView from "./MetaReviewView";
import LibrarianView from "./LibrarianView";
import OutboxView from "./OutboxView";
import WatchView from "./WatchView";
import SettingsPanel from "./SettingsPanel";

type View = "deliberate" | "direct-fire" | "history" | "outbox" | "watch" | "patterns" | "drift" | "meta-review" | "cabinets" | "librarian" | "discover" | "settings";
type ApiStatus = "loading" | "online" | "error";

export default function WarRoom() {
  const { state, start: rawStart, intervene, reset, abort } = useDeliberation();
  const [view, setView] = useState<View>("deliberate");
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [cabinets, setCabinets] = useState<Cabinet[]>([]);
  const [apiStatus, setApiStatus] = useState<ApiStatus>("loading");
  const [apiError, setApiError] = useState<string | null>(null);
  const [driftInitialReport, setDriftInitialReport] = useState<string | null>(null);
  // Cabinet pre-selection from the editor is consumed once by
  // IdlePanel — same pattern as driftInitialReport).
  const [pendingCabinet, setPendingCabinet] = useState<string | null>(null);
  const [lastSessionId, setLastSessionId] = useState<string | null>(null);
  const [outboxTenant, setOutboxTenant] = useState("system");
  const lastStartRef = useRef<StartPayload | null>(null);
  const sidecarAutoStartRef = useRef(false);
  const [hasLastStart, setHasLastStart] = useState(false);

  const start = useCallback(
    (p: StartPayload) => {
      lastStartRef.current = p;
      setHasLastStart(true);
      rawStart(p);
    },
    [rawStart],
  );

  const reconnect = useCallback(() => {
    const p = lastStartRef.current;
    if (!p) return;
    rawStart(p);
  }, [rawStart]);

  const lastError = state.errors[state.errors.length - 1];
  const isConnectionLoss =
    !!lastError && lastError.message === "Connection to council bridge lost";
  const canReconnect = isConnectionLoss && hasLastStart;

  const navigateToDriftReport = (reportFilename: string) => {
    setDriftInitialReport(reportFilename);
    setView("drift");
  };

  const navigateToDeliberateWithCabinet = (cabinetKey: string) => {
    setPendingCabinet(cabinetKey);
    setView("deliberate");
  };

  const navigateToHistory = (sessionId?: string) => {
    if (sessionId) {
      setLastSessionId(sessionId);
    }
    setView("history");
  };

  const loadInitialState = useCallback(async () => {
    const runtimeConfig = await loadRuntimeConfig();
    setApiStatus("loading");
    setApiError(null);

    const [healthResult, cabinetsResult] = await Promise.allSettled([
      api.health(),
      api.cabinets(),
    ]);

    if (healthResult.status === "fulfilled") {
      setHealth(healthResult.value);
    } else {
      setHealth(null);
    }

    if (cabinetsResult.status === "fulfilled") {
      setCabinets(cabinetsResult.value.cabinets);
    } else {
      setCabinets([]);
    }

    if (
      isTauri() &&
      healthResult.status === "fulfilled" &&
      cabinetsResult.status === "fulfilled"
    ) {
      void reportCouncilRuntimeReady(
        councilPortFromApiBase(runtimeConfig.apiBase),
      ).catch(() => {});
    }

    const failures = [
      healthResult.status === "rejected"
        ? `health: ${errorMessage(healthResult.reason)}`
        : null,
      cabinetsResult.status === "rejected"
        ? `cabinets: ${errorMessage(cabinetsResult.reason)}`
        : null,
    ].filter(Boolean);

    if (failures.length > 0) {
      setApiStatus("error");
      setApiError(failures.join(" · "));
    } else {
      setApiStatus("online");
    }
  }, []);

  useEffect(() => {
    initRuntimeConfig();
    let aborted = false;

    void loadRuntimeConfig();
    void loadInitialState();

    void configReady.then((cfg) => {
      if (!isTauri() || sidecarAutoStartRef.current) return;
      sidecarAutoStartRef.current = true;
      void startCouncilServer(
        cfg.councilPath || undefined,
        councilPortFromApiBase(cfg.apiBase),
        cfg.authToken,
        cfg.councilRoot || undefined,
        cfg.librarianBase || undefined,
      )
        .then(() => {
          // The sidecar takes a moment to bind; the mount-time health check
          // has usually already failed by now, so re-poll a few times.
          for (const ms of [1500, 3000, 6000]) {
            window.setTimeout(() => {
              if (!aborted) void loadInitialState();
            }, ms);
          }
        })
        .catch(() => {});
    });

    const onConfig = () => {
      void loadInitialState();
    };
    window.addEventListener("warroom-config-changed", onConfig);

    return () => {
      aborted = true;
      window.removeEventListener("warroom-config-changed", onConfig);
    };
  }, [loadInitialState]);

  const isActive = state.phase !== "idle" && state.phase !== "error";
  const isDone = state.phase === "done";

  useEffect(() => {
    if (isDone && state.session_id) {
      setLastSessionId(state.session_id);
    }
  }, [isDone, state.session_id]);

  return (
    <div className="min-h-screen flex flex-col">
      <Header
        view={view}
        onView={setView}
        health={health}
        apiStatus={apiStatus}
        active={isActive}
        sessionDone={isDone}
        onReset={reset}
        onAbort={abort}
      />

      {apiStatus === "error" && (
        <BackendConnectionBanner message={apiError} />
      )}

      <main
        className={cn(
          "flex-1 w-full mx-auto max-w-[1600px]",
          view === "history" || view === "deliberate"
            ? "px-3 py-4 md:px-4"
            : "px-6 py-8",
        )}
      >
        {view === "deliberate" && (
          <DeliberateWorkspace
            state={state}
            cabinets={cabinets}
            health={health}
            onStart={start}
            onIntervene={intervene}
            onReset={reset}
            onReconnect={reconnect}
            canReconnect={canReconnect}
            onViewDriftReport={navigateToDriftReport}
            onViewOutbox={(tenant) => {
              setOutboxTenant(tenant);
              setView("outbox");
            }}
            onViewHistory={navigateToHistory}
            initialCabinet={pendingCabinet}
            onConsumeInitialCabinet={() => setPendingCabinet(null)}
          />
        )}

        {view === "direct-fire" && <DirectFirePanel />}
        {view === "history" && (
          <SessionExplorer
            onLaunch={start}
            initialSelectedId={lastSessionId ?? undefined}
            apiStatus={apiStatus}
            apiError={apiError}
            onRetryConnection={() => void loadInitialState()}
          />
        )}
        {view === "outbox" && <OutboxView initialTenant={outboxTenant} />}
        {view === "watch" && <WatchView initialTenant={outboxTenant} />}
        {view === "patterns" && <PatternsView />}
        {view === "drift" && (
          <DriftView
            initialReport={driftInitialReport}
            onConsumeInitial={() => setDriftInitialReport(null)}
          />
        )}
        {view === "meta-review" && <MetaReviewView />}
        {view === "librarian" && (
          <LibrarianView onOpenSettings={() => setView("settings")} />
        )}
        {view === "cabinets" && (
          <CabinetEditor
            cabinets={cabinets}
            onRefresh={() => void loadInitialState()}
            onRun={navigateToDeliberateWithCabinet}
          />
        )}
        {view === "discover" && <DiscoverPanel />}
        {view === "settings" && <SettingsPanel />}
      </main>

      {isActive && state.phase !== "idle" && (
        <LiveAnalytics state={state} />
      )}
    </div>
  );
}

function errorMessage(reason: unknown): string {
  return reason instanceof Error ? reason.message : String(reason);
}

function BackendConnectionBanner({ message }: { message: string | null }) {
  return (
    <div
      data-testid="backend-connection-error"
      className="border-b border-danger/40 bg-danger/10"
    >
      <div className="max-w-[1600px] w-full mx-auto px-4 md:px-6 py-3 flex flex-col gap-1 md:flex-row md:items-center md:gap-3">
        <div className="flex items-center gap-2 text-danger font-display font-bold text-sm">
          <AlertTriangle className="w-4 h-4" />
          Backend connection issue
        </div>
        <div className="text-xs font-mono text-fg-muted">
          API {apiBase()}
          {message ? ` · ${message}` : ""}
        </div>
      </div>
    </div>
  );
}

function Header({
  view,
  onView,
  health,
  apiStatus,
  active,
  sessionDone,
  onReset,
  onAbort,
}: {
  view: View;
  onView: (v: View) => void;
  health: HealthResponse | null;
  apiStatus: ApiStatus;
  active: boolean;
  sessionDone?: boolean;
  onReset: () => void;
  onAbort: () => void;
}) {
  return (
    <header className="border-b border-border bg-bg-deep sticky top-0 z-30">
      <div className="max-w-[1600px] w-full mx-auto px-4 md:px-6 h-12 flex items-center gap-3 md:gap-6">
        <div className="flex items-center gap-2.5 shrink-0">
          <div className="w-7 h-7 shrink-0 rounded-sm border border-amber/50 bg-amber/10 grid place-items-center font-mono text-sm font-semibold text-amber select-none">
            C
          </div>
          <div className="hidden sm:block leading-tight">
            <div className="font-display font-bold text-sm tracking-tight text-fg-bright">
              COUNCIL · WAR ROOM
            </div>
            <div
              data-testid="warroom-health-status"
              className="text-[9px] font-mono uppercase tracking-widest text-fg-dim"
            >
              {health
                ? `gen ${health.council_version} · stream ${health.stream_version}`
                : apiStatus === "loading" ? "connecting" : "offline"}
            </div>
          </div>
        </div>
        <nav className="flex items-stretch self-stretch overflow-x-auto scrollbar-thin flex-1 min-w-0">
          <NavBtn active={view === "deliberate"} onClick={() => onView("deliberate")}>
            Deliberate
          </NavBtn>
          <NavBtn active={view === "direct-fire"} onClick={() => onView("direct-fire")}>
            Direct Fire
          </NavBtn>
          <NavBtn active={view === "history"} onClick={() => onView("history")}>
            History
          </NavBtn>
          <NavBtn active={view === "cabinets"} onClick={() => onView("cabinets")}>
            Cabinets
          </NavBtn>
          <NavBtn active={view === "discover"} onClick={() => onView("discover")}>
            Discover
          </NavBtn>
          <NavBtn active={view === "settings"} onClick={() => onView("settings")}>
            Settings
          </NavBtn>
          <NavBtn active={view === "patterns"} onClick={() => onView("patterns")}>
            Patterns
          </NavBtn>
          <NavBtn active={view === "drift"} onClick={() => onView("drift")}>
            Drift
          </NavBtn>
          <NavBtn active={view === "librarian"} onClick={() => onView("librarian")}>
            Librarian
          </NavBtn>
          <NavBtn active={view === "meta-review"} onClick={() => onView("meta-review")}>
            Meta-review
          </NavBtn>
          <NavBtn active={view === "outbox"} onClick={() => onView("outbox")}>
            Outbox
          </NavBtn>
          <NavBtn active={view === "watch"} onClick={() => onView("watch")}>
            Watch
          </NavBtn>
        </nav>

        <div className="flex items-center self-stretch gap-3 md:gap-4 shrink-0">
          <div className="hidden md:flex self-stretch">
            <StatusStrip health={health} />
          </div>
          {sessionDone ? (
            <button
              type="button"
              onClick={onReset}
              className="btn btn-primary"
              data-testid="new-deliberation-nav"
            >
              <RotateCcw className="w-4 h-4" />
              <span className="hidden sm:inline">New deliberation</span>
            </button>
          ) : active ? (
            <button type="button" onClick={onAbort} className="btn btn-danger">
              <ArrowLeft className="w-4 h-4" />
              <span className="hidden sm:inline">Abort</span>
            </button>
          ) : null}
        </div>
      </div>
    </header>
  );
}

function NavBtn({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      className={cn(
        "relative flex items-center px-2.5 font-mono text-xs",
        "transition-colors whitespace-nowrap shrink-0",
        active
          ? "text-fg-bright font-semibold"
          : "text-fg-muted hover:text-fg",
      )}
    >
      {children}
      {active && (
        <span aria-hidden className="absolute left-2 right-2 bottom-0 h-0.5 bg-amber" />
      )}
    </button>
  );
}

/** Council health reports Gateway configuration, not live reachability. */
function StatusStrip({ health }: { health: HealthResponse | null }) {
  if (!health) return null;
  const seats = ["grok", "claude", "gpt", "gemini"];
  const up = seats.filter((p) => health.providers_available.includes(p));
  const gatewayConfigured = health.providers_available.includes("gateway");
  return (
    <div className="flex items-stretch font-mono border-l border-border">
      <StripCell
        label="Gateway"
        value={gatewayConfigured ? "configured" : "not set"}
        tone={gatewayConfigured ? "ok" : "down"}
        title="Gateway credentials are configured. Settings > Test Connection checks live reachability."
      />
      <StripCell
        label="Providers"
        value={`${up.length}/${seats.length}`}
        tone={up.length === seats.length ? "ok" : up.length > 0 ? "warn" : "down"}
        title={seats
          .map((p) => `${p}: ${up.includes(p) ? "up" : "down"}`)
          .join(" · ")}
      />
    </div>
  );
}

function StripCell({
  label,
  value,
  tone,
  title,
}: {
  label: string;
  value: string;
  tone: "ok" | "warn" | "down";
  title?: string;
}) {
  return (
    <div
      className="flex flex-col justify-center gap-0.5 px-3 border-r border-border"
      title={title}
    >
      <span className="text-[9px] uppercase tracking-widest text-fg-dim leading-none">
        {label}
      </span>
      <span className="flex items-center gap-1.5 text-[11px] text-fg leading-none">
        <span
          className={cn(
            "w-[5px] h-[5px] rounded-full shrink-0",
            tone === "ok" && "bg-success",
            tone === "warn" && "bg-warning",
            tone === "down" && "bg-danger",
          )}
        />
        {value}
      </span>
    </div>
  );
}
