"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle, ArrowLeft, RotateCcw } from "lucide-react";
import { useDeliberation } from "@/hooks/useDeliberation";
import { api, apiBase } from "@/lib/api";
import {
  councilPortFromApiBase,
  initRuntimeConfig,
  loadRuntimeConfig,
} from "@/lib/runtime-config";
import {
  getGatewayPackStatus,
  isTauri,
  nativeOwnsCouncilStartup,
  reportCouncilRuntimeReady,
  startCouncilServer,
  type GatewayPackStatus,
} from "@/lib/tauri";
import { startWarRoomBackendReady } from "@/lib/warroom-backend-ready";
import type { BootHealthPollHandle } from "@/lib/boot-health-poll";
import { gatewayHeaderTruth } from "@/lib/gateway-pack";
import { notifyDiscoverBackendReady } from "@/lib/use-discover";
import {
  COUNCIL_LOADING_LABEL,
  warroomHealthLabel,
} from "@/lib/warroom-health-label";
import { cn } from "@/lib/cn";
import type { Cabinet, HealthResponse } from "@/lib/types";
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
  const { state, start, intervene, reset, abort } = useDeliberation();
  const [view, setView] = useState<View>("deliberate");
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [gatewayPack, setGatewayPack] = useState<GatewayPackStatus | null>(null);
  const [cabinets, setCabinets] = useState<Cabinet[]>([]);
  const [apiStatus, setApiStatus] = useState<ApiStatus>("loading");
  const [apiError, setApiError] = useState<string | null>(null);
  const [driftInitialReport, setDriftInitialReport] = useState<string | null>(null);
  // Cabinet pre-selection from the editor is consumed once by
  // IdlePanel — same pattern as driftInitialReport).
  const [pendingCabinet, setPendingCabinet] = useState<string | null>(null);
  const [lastSessionId, setLastSessionId] = useState<string | null>(null);
  const [outboxTenant, setOutboxTenant] = useState("system");
  /**
   * Cold-start CONNECTING flag from the readiness-driven boot poller.
   * Stays true while native pack resume / Council bind is still legitimately
   * in flight; false once online or the connecting budget is exhausted.
   */
  const [bootRetryActive, setBootRetryActive] = useState(false);
  const bootPollerRef = useRef<BootHealthPollHandle | null>(null);

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

  /** Probe health + cabinets. Returns true only when the UI can go online. */
  const loadInitialState = useCallback(async (): Promise<boolean> => {
    const runtimeConfig = await loadRuntimeConfig();
    setApiStatus("loading");
    setApiError(null);

    const [healthResult, cabinetsResult, packResult] = await Promise.allSettled([
      api.health(),
      api.cabinets(),
      isTauri() ? getGatewayPackStatus() : Promise.resolve(null),
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

    if (packResult.status === "fulfilled") {
      setGatewayPack(packResult.value);
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
      return false;
    }
    setApiStatus("online");
    setApiError(null);
    return true;
  }, []);

  useEffect(() => {
    // Production backend-readiness effect (poll + native gate + config re-arm).
    const ready = startWarRoomBackendReady({
      loadInitialState,
      isTauri,
      nativeOwnsCouncilStartup,
      startCouncilServer,
      getConfigForStartup: () => loadRuntimeConfig(),
      initRuntimeConfig,
      onRetryActiveChange: setBootRetryActive,
      onDiscoverBackendReady: notifyDiscoverBackendReady,
    });
    bootPollerRef.current = ready.poller();
    return () => {
      ready.stop();
      bootPollerRef.current = null;
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
        gatewayPack={gatewayPack}
        apiStatus={apiStatus}
        bootRetryActive={bootRetryActive}
        active={isActive}
        sessionDone={isDone}
        onReset={reset}
        onAbort={abort}
      />

      {apiStatus === "error" && !bootRetryActive && (
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
        {view === "deliberate" &&
          (apiStatus === "loading" || bootRetryActive ? (
            <div
              role="status"
              data-testid="deliberate-backend-loading"
              className="panel p-5 font-mono text-sm text-fg-muted"
            >
              {COUNCIL_LOADING_LABEL}
            </div>
          ) : (
            <DeliberateWorkspace
              state={state}
              cabinets={cabinets}
              onStart={start}
              onIntervene={intervene}
              onReset={reset}
              onViewDriftReport={navigateToDriftReport}
              onViewOutbox={(tenant) => {
                setOutboxTenant(tenant);
                setView("outbox");
              }}
              onViewHistory={navigateToHistory}
              initialCabinet={pendingCabinet}
              onConsumeInitialCabinet={() => setPendingCabinet(null)}
            />
          ))}

        {view === "direct-fire" && <DirectFirePanel />}
        {view === "history" && (
          <SessionExplorer
            onLaunch={start}
            initialSelectedId={lastSessionId ?? undefined}
            apiStatus={apiStatus}
            apiError={apiError}
            onRetryConnection={() => {
              void loadInitialState().then((ready) => {
                if (ready) {
                  bootPollerRef.current?.markOnline();
                } else {
                  bootPollerRef.current?.startConnecting({ force: true });
                }
              });
            }}
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
  gatewayPack,
  apiStatus,
  bootRetryActive,
  active,
  sessionDone,
  onReset,
  onAbort,
}: {
  view: View;
  onView: (v: View) => void;
  health: HealthResponse | null;
  gatewayPack: GatewayPackStatus | null;
  apiStatus: ApiStatus;
  bootRetryActive: boolean;
  active: boolean;
  sessionDone?: boolean;
  onReset: () => void;
  onAbort: () => void;
}) {
  const healthLabel = warroomHealthLabel(
    health?.council_version ?? null,
    health?.stream_version ?? null,
    apiStatus,
    bootRetryActive,
  );
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
              {healthLabel}
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
            <StatusStrip health={health} pack={gatewayPack} />
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

/** Gateway strip: pack-ready/governed truth preferred over bare health "gateway" flag. */
function StatusStrip({
  health,
  pack,
}: {
  health: HealthResponse | null;
  pack?: GatewayPackStatus | null;
}) {
  if (!health) return null;
  const seats = ["grok", "claude", "gpt", "gemini"];
  const up = seats.filter((p) => health.providers_available.includes(p));
  const healthGw = health.providers_available.includes("gateway");
  const gw = gatewayHeaderTruth(pack ?? null, healthGw);
  // Docker-optional is not a product failure: map neutral → warn (not red "down").
  const tone = gw.tone === "neutral" ? "warn" : gw.tone;
  return (
    <div className="flex items-stretch font-mono border-l border-border">
      <StripCell
        label="Gateway"
        value={gw.label}
        tone={tone}
        title={gw.detail}
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
