"use client";

import { useEffect, useMemo, useState } from "react";
import { motion } from "framer-motion";
import { DEFAULT_SENSITIVITY } from "@/lib/gateway-mode";
import { canEnableGovernedProceeding } from "@/lib/gateway-pack";
import { mergeIfNewer } from "@/lib/desktop-status";
import type { Cabinet } from "@/lib/types";
import type { GatewaySensitivity, StartPayload } from "@/lib/ws";
import {
  getDesktopRuntimeMode,
  getDesktopStatusSnapshot,
  isTauri,
  type DesktopRuntimeMode,
  type DesktopStatusSnapshot,
  type GatewayPackStatus,
} from "@/lib/tauri";
import {
  availableProviderIds,
  useDiscover,
} from "@/lib/use-discover";
import PrecedentAmbient from "./PrecedentAmbient";
import WeeklyDriftCard from "./WeeklyDriftCard";
import { useToast } from "./Toast";
import { ProceedingRulingColumn } from "./proceeding/ProceedingRulingColumn";
import { CabinetPreview } from "./idle/CabinetPreview";
import { ConveneButton } from "./idle/ConveneButton";
import { ConveneForm } from "./idle/ConveneForm";
import { StandaloneRail } from "./idle/StandaloneRail";
import { conveneBlocker } from "./idle/conveneBlocker";
import { useCabinetSelection } from "./idle/useCabinetSelection";
import { useConveneMatter } from "./idle/useConveneMatter";
import { useConveneOptions } from "./idle/useConveneOptions";
import { useConveneSubmit } from "./idle/useConveneSubmit";
import { usePrecedentIndex } from "./idle/usePrecedentIndex";
import { usePrecedentSearch } from "./idle/usePrecedentSearch";
import { useValidatorProviders } from "./idle/useValidatorProviders";

function conveneWireMode(
  mode: "teardown" | "pathfind" | "harden",
  thenTearDown: boolean,
  blind: boolean,
): string {
  if (blind) return "blind";
  if (thenTearDown) return "pathfind";
  return mode;
}

export default function IdlePanel({
  cabinets,
  onStart,
  onViewDriftReport,
  initialCabinet,
  onConsumeInitialCabinet,
  variant = "standalone",
}: {
  cabinets: Cabinet[];
  onStart: (p: StartPayload) => void;
  onViewDriftReport?: (reportFilename: string) => void;
  /** Cabinet selection from the editor, applied once on mount. */
  initialCabinet?: string | null;
  onConsumeInitialCabinet?: () => void;
  /** `shell` — 3-col command-grade workspace (rail | convene | ruling). */
  variant?: "standalone" | "shell";
}) {
  const [viaGateway, setViaGateway] = useState(false);
  const [sensitivity, setSensitivity] = useState<GatewaySensitivity>(DEFAULT_SENSITIVITY);
  // Same tri-state as Settings: under Tauri the build mode starts "detecting"
  // and may end "unavailable"; both must fail the governed gate closed.
  const [desktopMode, setDesktopMode] = useState<
    DesktopRuntimeMode | "detecting" | "unavailable"
  >(isTauri() ? "detecting" : "unavailable");
  const [desktopStatus, setDesktopStatus] =
    useState<DesktopStatusSnapshot | null>(null);
  const packStatus: GatewayPackStatus | null = desktopStatus?.pack ?? null;
  const { toast } = useToast();

  const matter = useConveneMatter();
  const options = useConveneOptions();
  const { blind, mode, thenTearDown, validate, validateProvider } = options;

  // Validator choices are exact transport identities. Keep unavailable choices
  // visible for explanation, but never allow them to be selected or launched.
  const { data: discoverData, loading: discoverLoading, error: discoverError, providerOptions } = useDiscover();

  // Single availability source for runnability, auto-select, and convene
  // gating: the normalized Discover inventory. `/api/health` stays liveness
  // only — it deliberately reports host CLI transports as unavailable.
  const availableIds = useMemo(
    () => availableProviderIds(discoverData),
    [discoverData],
  );

  const { cabinetName, selectCabinet } = useCabinetSelection({
    initialCabinet,
    onConsumeInitialCabinet,
    cabinets,
    availableIds,
  });
  const { precedent, precedentMode } = usePrecedentSearch(matter.topic, blind);
  const precedentIndex = usePrecedentIndex(toast);
  const validatorProviders = useValidatorProviders(providerOptions);

  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    void getDesktopRuntimeMode()
      .then((m) => {
        if (!cancelled) setDesktopMode(m);
      })
      .catch(() => {
        if (!cancelled) setDesktopMode("unavailable");
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // Same host-authoritative snapshot subscription as Settings (no poll epochs).
  useEffect(() => {
    if (!isTauri() || desktopMode !== "installed-release") return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void getDesktopStatusSnapshot()
      .then((snap) => {
        if (!cancelled) setDesktopStatus((prev) => mergeIfNewer(prev, snap));
      })
      .catch(() => {
        // Keep last known projection on failure.
      });
    void import("@tauri-apps/api/event")
      .then(({ listen }) =>
        listen<DesktopStatusSnapshot>("desktop-status", (event) => {
          if (!cancelled) {
            setDesktopStatus((prev) => mergeIfNewer(prev, event.payload));
          }
        }),
      )
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [desktopMode]);

  // Fail closed under Tauri while the build mode is detecting or unavailable;
  // the browser (no native mode) keeps the permissive development path.
  const governedAllowed = canEnableGovernedProceeding(packStatus, {
    requireInstalledRelease: true,
    desktopMode: isTauri() ? desktopMode : "development",
  });

  useEffect(() => {
    if (!governedAllowed && viaGateway) {
      setViaGateway(false);
    }
  }, [governedAllowed, viaGateway]);

  const cabinet = useMemo(
    () => cabinets.find((c) => c.name === cabinetName),
    [cabinets, cabinetName],
  );

  const isWargameCabinet = useMemo(() => {
    const n = cabinetName.toLowerCase();
    return n === "wargame" || n.includes("wargame");
  }, [cabinetName]);

  const providerSelectionProblem = conveneBlocker({
    discoverData,
    discoverLoading,
    discoverError,
    cabinets,
    availableIds,
    cabinet,
    providerOptions,
    validate,
    validateProvider,
    viaGateway,
  });
  const canStart = matter.topic.trim().length > 4 && !providerSelectionProblem;

  const submit = useConveneSubmit({
    canStart,
    matter,
    options,
    cabinetName,
    cabinet,
    viaGateway,
    sensitivity,
    toast,
    onStart,
  });

  const wireMode = conveneWireMode(mode, thenTearDown, blind);

  const conveneButtonEl = (
    <ConveneButton
      canStart={canStart}
      submitting={matter.submitting}
      providerSelectionProblem={providerSelectionProblem}
      variant={variant}
      onSubmit={submit}
    />
  );

  const formSection = (
    <ConveneForm
      variant={variant}
      matter={matter}
      options={options}
      precedent={precedent}
      cabinets={cabinets}
      cabinetName={cabinetName}
      selectCabinet={selectCabinet}
      availableIds={availableIds}
      isWargameCabinet={isWargameCabinet}
      cabinet={cabinet}
      validatorProviders={validatorProviders}
      governedAllowed={governedAllowed}
      desktopMode={desktopMode}
      viaGateway={viaGateway}
      setViaGateway={setViaGateway}
      sensitivity={sensitivity}
      setSensitivity={setSensitivity}
      toast={toast}
      wireMode={wireMode}
      conveneButtonEl={conveneButtonEl}
    />
  );

  const shellRail = (
    <>
      <p className="cg-section-label mb-1">Proceeding context</p>
      <PrecedentAmbient
        variant="command"
        matches={precedent}
        blind={blind}
        mode={precedentMode}
      />
      <div className="cg-rail-section-gap">
        <p className="cg-section-label mb-2">Selected cabinet</p>
        <CabinetPreview cabinet={cabinet} variant="command" />
      </div>
    </>
  );

  const standaloneRail = (
    <StandaloneRail
      precedent={precedent}
      blind={blind}
      precedentMode={precedentMode}
      cabinet={cabinet}
      index={precedentIndex}
    />
  );

  const railContent = variant === "shell" ? shellRail : standaloneRail;

  if (variant === "shell") {
    return (
      <div className="cg-history-workspace" data-testid="deliberate-workspace-idle">
        <aside className="cg-rail cg-deliberate-rail cg-deliberate-rail--idle">{railContent}</aside>
        <div className="cg-record-primary cg-convene-record">{formSection}</div>
        <ProceedingRulingColumn
          awaiting
          placeholder="Convene the council to begin. The ruling will appear here when the chair files it."
        />
      </div>
    );
  }

  return (
    <motion.div
      initial={{ opacity: 0, y: 10 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.4 }}
      className="space-y-6"
    >
      <WeeklyDriftCard onViewReport={onViewDriftReport} />

      <div className="grid grid-cols-12 gap-6">
        {formSection}
        <aside className="col-span-12 lg:col-span-4">{railContent}</aside>
      </div>
    </motion.div>
  );
}
