"use client";

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { Compass, Database, Eye, EyeOff, FolderTree, Loader2, Network, Play, Search, Shield, ShieldAlert, ShieldCheck, Sparkles, Swords } from "lucide-react";
import { motion } from "framer-motion";
import { api } from "@/lib/api";
import { cn, providerColor } from "@/lib/cn";
import { DEFAULT_SENSITIVITY, SENSITIVITY_LEVELS, gatewayStartFields } from "@/lib/gateway-mode";
import { canEnableGovernedProceeding } from "@/lib/gateway-pack";
import type { Cabinet, EmbeddingStats, HealthResponse, MapmakerResult, PrecedentMatch } from "@/lib/types";
import type { GatewaySensitivity, StartPayload } from "@/lib/ws";
import {
  getDesktopRuntimeMode,
  getGatewayPackStatus,
  isTauri,
  type GatewayPackStatus,
} from "@/lib/tauri";
import {
  DEFAULT_CABINET_NAME,
  noRunnableCabinetExplanation,
  resolveUntouchedCabinetSelection,
} from "@/lib/cabinet-selection";
import {
  getProviderOption,
  providerOptionLabel,
  unsupportedGatewayTransportReason,
  unavailableProviderReason,
  useDiscover,
} from "@/lib/use-discover";
import CabinetSelector from "./CabinetSelector";
import ContextUploader from "./ContextUploader";
import ExperimentalBanner from "./ExperimentalBanner";
import MapScanner from "./MapScanner";
import PrecedentAmbient from "./PrecedentAmbient";
import WeeklyDriftCard from "./WeeklyDriftCard";
import { useToast } from "./Toast";
import { ProceedingRulingColumn } from "./proceeding/ProceedingRulingColumn";
import { RecordModeChip } from "./proceeding/ModeChips";

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
  health,
  onViewDriftReport,
  initialCabinet,
  onConsumeInitialCabinet,
  variant = "standalone",
}: {
  cabinets: Cabinet[];
  onStart: (p: StartPayload) => void;
  health: HealthResponse | null;
  onViewDriftReport?: (reportFilename: string) => void;
  /** Cabinet selection from the editor, applied once on mount. */
  initialCabinet?: string | null;
  onConsumeInitialCabinet?: () => void;
  /** `shell` — 3-col command-grade workspace (rail | convene | ruling). */
  variant?: "standalone" | "shell";
}) {
  const [topic, setTopic] = useState("");
  const [cabinetName, setCabinetName] = useState(
    initialCabinet || DEFAULT_CABINET_NAME,
  );
  // Explicit editor handoff or any operator click locks selection permanently
  // for this idle mount — health flaps must not re-auto-switch.
  const selectionLocked = useRef(!!initialCabinet);
  const autoSelectDone = useRef(false);
  const consumedInitialCabinet = useRef(false);
  useEffect(() => {
    if (!consumedInitialCabinet.current && initialCabinet) {
      consumedInitialCabinet.current = true;
      selectionLocked.current = true;
      onConsumeInitialCabinet?.();
    }
  }, [initialCabinet, onConsumeInitialCabinet]);

  const selectCabinet = useCallback((name: string) => {
    selectionLocked.current = true;
    autoSelectDone.current = true;
    setCabinetName(name);
  }, []);
  const [context, setContext] = useState("");
  const [mapDir, setMapDir] = useState("");
  const [blind, setBlind] = useState(false);
  const [pause, setPause] = useState(true);
  const [maxRounds, setMaxRounds] = useState<number | "">("");
  const [mode, setMode] = useState<"teardown" | "pathfind" | "harden">("teardown");
  const [validate, setValidate] = useState(false);
  const [validateProvider, setValidateProvider] = useState<string>("grok_build");
  const [validateGate, setValidateGate] = useState(false);
  const [frameCheck, setFrameCheck] = useState(true);
  const [scopeAuditor, setScopeAuditor] = useState(false);
  const [budgetUsd, setBudgetUsd] = useState<number | "">("");
  const [tier, setTier] = useState("best");
  const [thenTearDown, setThenTearDown] = useState(false);
  const [specopsThreshold, setSpecopsThreshold] = useState(0.8);
  const [viaGateway, setViaGateway] = useState(false);
  const [sensitivity, setSensitivity] = useState<GatewaySensitivity>(DEFAULT_SENSITIVITY);
  const [workerProvJson, setWorkerProvJson] = useState("");
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [precedent, setPrecedent] = useState<PrecedentMatch[]>([]);
  const [precedentMode, setPrecedentMode] = useState<"semantic" | "keyword">("keyword");
  const [mapBrief, setMapBrief] = useState<{ text: string; result: MapmakerResult } | null>(null);
  const [embStats, setEmbStats] = useState<EmbeddingStats | null>(null);
  const [rebuilding, setRebuilding] = useState(false);
  const [reindexingPrecedent, setReindexingPrecedent] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [desktopMode, setDesktopMode] = useState<string | undefined>(undefined);
  const [packStatus, setPackStatus] = useState<GatewayPackStatus | null>(null);
  const topicRef = useRef<HTMLTextAreaElement>(null);
  const { toast } = useToast();

  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    void getDesktopRuntimeMode()
      .then((m) => {
        if (!cancelled) setDesktopMode(m);
      })
      .catch(() => {
        if (!cancelled) setDesktopMode(undefined);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (!isTauri() || desktopMode !== "installed-release") return;
    let cancelled = false;
    const tick = () => {
      void getGatewayPackStatus()
        .then((s) => {
          if (!cancelled) setPackStatus(s);
        })
        .catch(() => {
          if (!cancelled) setPackStatus(null);
        });
    };
    tick();
    const id = window.setInterval(tick, 8000);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, [desktopMode]);

  const governedAllowed = canEnableGovernedProceeding(packStatus, {
    requireInstalledRelease: true,
    desktopMode: desktopMode ?? "development",
  });

  useEffect(() => {
    if (!governedAllowed && viaGateway) {
      setViaGateway(false);
    }
  }, [governedAllowed, viaGateway]);

  // Validator choices are exact transport identities. Keep unavailable choices
  // visible for explanation, but never allow them to be selected or launched.
  const { data: discoverData, loading: discoverLoading, error: discoverError, providerOptions } = useDiscover();

  // Untouched first load: once health inventory is known, prefer a stable
  // runnable cabinet over a blocked default (see lib/cabinet-selection.ts).
  useEffect(() => {
    if (autoSelectDone.current || selectionLocked.current) return;
    if (!health) return;
    const next = resolveUntouchedCabinetSelection({
      cabinets,
      providersAvailable: health.providers_available,
      currentName: cabinetName,
      selectionLocked: selectionLocked.current,
    });
    autoSelectDone.current = true;
    if (next && next !== cabinetName) {
      setCabinetName(next);
    }
    // Lock after the first decision so later health changes never re-pick.
    selectionLocked.current = true;
  }, [cabinets, health, cabinetName]);

  const validatorProviders = useMemo(() => {
    const allowed = [
      "grok_build",
      "grok_hermes",
      "grok_api",
      "claude_code",
      "claude_api",
      "codex_cli",
      "openai_api",
      "gemini_agy",
      "gemini_vertex",
    ];
    return allowed
      .map((id) => getProviderOption(providerOptions, id))
      .filter((provider): provider is NonNullable<typeof provider> => !!provider);
  }, [providerOptions]);

  const loadEmbStats = useCallback(() => {
    api.embeddingsStats().then(setEmbStats).catch(() => {});
  }, []);

  useEffect(() => {
    loadEmbStats();
  }, [loadEmbStats]);

  // Debounced precedent search as topic is typed
  useEffect(() => {
    if (blind || topic.trim().length < 8) {
      setPrecedent([]);
      return;
    }
    const id = setTimeout(() => {
      api
        .precedent(topic, 0.15, 5)
        .then((r) => {
          setPrecedent(r.matches);
          setPrecedentMode(r.mode);
        })
        .catch(() => setPrecedent([]));
    }, 600);
    return () => clearTimeout(id);
  }, [topic, blind]);

  useLayoutEffect(() => {
    if (variant !== "shell") return;
    const el = topicRef.current;
    if (!el) return;
    const cap = Math.floor(window.innerHeight * 0.4);
    el.style.height = "auto";
    const full = el.scrollHeight;
    const next = Math.min(Math.max(full, 72), cap);
    el.style.height = `${next}px`;
    el.style.overflowY = full > cap ? "auto" : "hidden";
  }, [topic, variant]);

  const cabinet = useMemo(
    () => cabinets.find((c) => c.name === cabinetName),
    [cabinets, cabinetName],
  );

  const isWargameCabinet = useMemo(() => {
    const n = cabinetName.toLowerCase();
    return n === "wargame" || n.includes("wargame");
  }, [cabinetName]);

  const cabinetProviderProblem = cabinet
    ? unavailableProviderReason(providerOptions, [
        ...cabinet.seats.map((seat) => seat.provider),
        cabinet.chair.provider,
      ])
    : "Selected cabinet was not found.";
  const validatorProviderProblem = validate
    ? unavailableProviderReason(providerOptions, [validateProvider])
    : null;
  const selectedProviderIds = cabinet
    ? [
        ...cabinet.seats.map((seat) => seat.provider),
        cabinet.chair.provider,
        ...(validate ? [validateProvider] : []),
      ]
    : [];
  const gatewayProviderProblem = viaGateway
    ? unsupportedGatewayTransportReason(providerOptions, selectedProviderIds)
    : null;
  const noRunnableExplanation = noRunnableCabinetExplanation(
    cabinets,
    health?.providers_available,
  );
  const providerSelectionProblem = discoverError
    ? `Provider discovery failed: ${discoverError}`
    : !discoverData || discoverLoading
      ? "Provider availability is still being checked."
      : noRunnableExplanation
        ?? cabinetProviderProblem
        ?? validatorProviderProblem
        ?? gatewayProviderProblem;
  const canStart = topic.trim().length > 4 && !providerSelectionProblem;

  const submit = () => {
    if (!canStart || submitting) return;
    // If the Mapmaker produced a brief, inject it as structured context and
    // suppress the raw --map bundle so the council doesn't see both.
    const briefBlock = mapBrief
      ? `--- EXECUTION MAP (Mapmaker · ${mapBrief.result.model}) ---\n${mapBrief.text}`
      : null;
    const finalContext =
      [context.trim() || null, briefBlock].filter(Boolean).join("\n\n") || undefined;
    let worker_provenance: Record<string, unknown> | undefined;
    if (workerProvJson.trim()) {
      try {
        worker_provenance = JSON.parse(workerProvJson) as Record<string, unknown>;
      } catch {
        toast("error", "worker_provenance must be valid JSON");
        return;
      }
    }
    setSubmitting(true);
    onStart({
      topic: topic.trim(),
      cabinet_name: cabinetName,
      context: finalContext,
      map_dir: mapBrief ? undefined : (mapDir || undefined),
      blind,
      pause_after_each_round: pause,
      max_rounds: typeof maxRounds === "number" ? maxRounds : undefined,
      mode: thenTearDown ? "pathfind" : mode,
      then_tear_down: thenTearDown,
      budget_max_usd: typeof budgetUsd === "number" ? budgetUsd : undefined,
      tier,
      auto_specops_threshold: specopsThreshold,
      worker_provenance,
      validate,
      validate_provider: validate ? validateProvider : undefined,
      validate_gate: validate ? validateGate : undefined,
      frame_check: cabinet?.local_code_only ? false : frameCheck,
      ...(scopeAuditor ? { scope_auditor: true } : {}),
      // Interactive War Room starts always declare Direct or Governed.
      ...gatewayStartFields(viaGateway, sensitivity),
    });
  };

  const wireMode = conveneWireMode(mode, thenTearDown, blind);

  const formBody = (
    <>
        <div className={cn(variant === "shell" ? "hidden" : "panel p-6 space-y-4 relative overflow-hidden")}>
          <div className="absolute inset-0 bg-amber-radial opacity-50 pointer-events-none" />
          <div className="relative">
            <div className="flex items-center gap-2 mb-2">
              <Sparkles className="w-4 h-4 text-amber" />
              <span className="label">Deliberation Topic</span>
            </div>
            <textarea
              value={topic}
              onChange={(e) => setTopic(e.target.value)}
              placeholder="State the question, decision, or proposal the council should deliberate on…"
              rows={5}
              className="input text-base resize-y min-h-[120px]"
              autoFocus
            />
            <div className="flex items-center justify-between mt-2 text-xs text-fg-dim font-mono">
              <span>{topic.length} chars</span>
              {!blind && precedent.length > 0 && (
                <span className="text-amber">
                  {precedent.length} prior ruling
                  {precedent.length === 1 ? "" : "s"} match
                </span>
              )}
            </div>
          </div>
        </div>

        {variant === "shell" ? (
          <div className="cg-convene-cabinet-block">
            <p className="cg-section-label mb-0">Cabinet</p>
            <CabinetSelector
              variant="command"
              embedded
              cabinets={cabinets}
              selected={cabinetName}
              onSelect={selectCabinet}
              health={health}
            />
          </div>
        ) : (
          <CabinetSelector
            cabinets={cabinets}
            selected={cabinetName}
            onSelect={selectCabinet}
            health={health}
          />
        )}

        {isWargameCabinet && (
          <ExperimentalBanner
            title="Experimental wargame cabinet"
            icon={<Swords className="w-4 h-4" />}
            testId="wargame-idle-help"
          >
            <p className={variant === "shell" ? "text-[11px]" : undefined}>
              {variant === "shell" ? (
                "MDMP-style adversarial COA analysis — convene as usual."
              ) : (
                <>
                  MDMP-style adversarial course-of-action analysis: Red attacks the
                  plan, Blue defends it, White arbitrates, Green audits
                  feasibility. Convene from here as usual — no CLI flags needed
                  (terminal parity: <code className="text-cyan">--wargame</code>).
                  The premortem direct-fire twin lives in the{" "}
                  <strong>Direct Fire</strong> tab.
                </>
              )}
            </p>
            {variant !== "shell" && cabinet && cabinet.seats.length > 0 && (
              <div className="font-mono text-[10px] text-fg-dim space-y-0.5">
                <div className="text-fg-muted">Expected seat roles:</div>
                {cabinet.seats.map((s) => (
                  <div key={s.name}>
                    <span className="text-fg">{s.name}</span> · {s.provider}
                  </div>
                ))}
              </div>
            )}
          </ExperimentalBanner>
        )}

        <div
          className={cn(
            variant === "shell"
              ? "cg-convene-options-block"
              : "grid grid-cols-1 md:grid-cols-2 gap-6",
          )}
        >
          {variant === "shell" && (
            <p className="cg-section-label mb-0">Context &amp; map</p>
          )}
          <ContextUploader value={context} onChange={setContext} />
          <MapScanner
            value={mapDir}
            onChange={setMapDir}
            onMapReady={setMapBrief}
          />
        </div>

        <div
          className={cn(
            variant === "shell" ? "cg-convene-options-block" : "panel p-5 space-y-4",
          )}
        >
          {variant === "shell" && (
            <p className="cg-section-label mb-0">Session controls</p>
          )}
          <div className="space-y-4">
          <div>
            <span className="label">Deliberation Mode</span>
            <div className="grid grid-cols-2 md:grid-cols-4 gap-2 mt-1.5">
              <ModeChip
                active={mode === "teardown" && !thenTearDown}
                onClick={() => { setMode("teardown"); setThenTearDown(false); }}
                icon={<Swords className="w-3.5 h-3.5" />}
                label="TearDown"
                sub="Kill bad ideas"
              />
              <ModeChip
                active={mode === "pathfind" && !thenTearDown}
                onClick={() => { setMode("pathfind"); setThenTearDown(false); }}
                icon={<Compass className="w-3.5 h-3.5" />}
                label="Pathfind"
                sub="No dead ends"
              />
              <ModeChip
                active={thenTearDown}
                onClick={() => { setMode("pathfind"); setThenTearDown(true); }}
                icon={<Compass className="w-3.5 h-3.5" />}
                label="Pathfind → Tear-down"
                sub="Two-phase CLI parity"
              />
              <ModeChip
                active={mode === "harden" && !thenTearDown}
                onClick={() => { setMode("harden"); setThenTearDown(false); }}
                icon={<ShieldAlert className="w-3.5 h-3.5" />}
                label="Harden"
                sub="Stress + fix"
              />
            </div>
          </div>
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            <div>
              <span className="label">Budget cap (USD, optional)</span>
              <input
                type="number"
                min={0}
                step={0.01}
                value={budgetUsd}
                onChange={(e) =>
                  setBudgetUsd(e.target.value ? Number(e.target.value) : "")
                }
                placeholder="No cap"
                className="input mt-1.5"
              />
            </div>
            <div>
              <span className="label">Routing tier</span>
              <select
                value={tier}
                onChange={(e) => setTier(e.target.value)}
                className="input mt-1.5"
              >
                <option value="best">best</option>
                <option value="sovereign">sovereign</option>
                <option value="strict_sovereign">strict_sovereign</option>
              </select>
            </div>
          </div>
          <div className="rounded-md border border-border bg-bg-overlay/40 p-3 space-y-3" data-testid="gateway-routing">
            <Toggle
              label="Governed via Gateway"
              sub={
                !governedAllowed && desktopMode === "installed-release"
                  ? "Requires authenticated Gateway Pack (Settings → Enable Gateway)"
                  : viaGateway
                    ? "All model calls fail closed through Gateway"
                    : "Direct provider and CLI calls"
              }
              value={viaGateway && governedAllowed}
              onChange={(v) => {
                if (v && !governedAllowed) {
                  toast(
                    "error",
                    "Gateway Pack is not authenticated-ready. Use Settings → Enable Gateway first.",
                  );
                  return;
                }
                setViaGateway(v);
              }}
              icon={<Network className="w-4 h-4" />}
              tone="cyan"
              testId="gateway-toggle"
            />
            {viaGateway ? (
              <div>
                <span className="label">Sensitivity</span>
                <select
                  data-testid="gateway-sensitivity"
                  value={sensitivity}
                  onChange={(e) => setSensitivity(e.target.value as GatewaySensitivity)}
                  className="input mt-1.5 max-w-[180px]"
                >
                  {SENSITIVITY_LEVELS.map((level) => (
                    <option key={level} value={level}>
                      {level.toUpperCase()}
                    </option>
                  ))}
                </select>
                <p className="text-[10px] text-fg-dim mt-1">
                  Sent lowercase on the wire; the server maps it to the gateway&apos;s{" "}
                  <code className="text-cyan">X-Sensitivity-Level</code> header.
                </p>
              </div>
            ) : (
              <p className="text-[10px] font-mono text-fg-dim">
                Direct — this proceeding explicitly bypasses Gateway governance.
              </p>
            )}
          </div>
          <div className="grid grid-cols-2 gap-4">
            <Toggle
              label="Blind mode"
              sub="No precedent injection"
              value={blind}
              onChange={setBlind}
              icon={blind ? <EyeOff className="w-4 h-4" /> : <Eye className="w-4 h-4" />}
              tone="cyan"
            />
            <Toggle
              label="Pause after each round"
              sub="Default on (steer mid-flight); off = continuous"
              value={pause}
              onChange={setPause}
              icon={<FolderTree className="w-4 h-4" />}
              tone="amber"
            />
            {!cabinet?.local_code_only && (
              <Toggle
                label="Frame check"
                sub="Round 1 prompt framing pass"
                value={frameCheck}
                onChange={setFrameCheck}
                icon={<Shield className="w-4 h-4" />}
                tone="cyan"
                testId="frame-check-toggle"
              />
            )}
            <div className="col-span-2">
              <span className="label">Max rounds (override cabinet default)</span>
              <input
                type="number"
                min={1}
                max={6}
                value={maxRounds}
                onChange={(e) =>
                  setMaxRounds(e.target.value ? Number(e.target.value) : "")
                }
                placeholder={cabinet ? String(cabinet.rounds) : "2"}
                className="input mt-1.5 max-w-[180px]"
              />
            </div>
          </div>
          <div className="pt-4 border-t border-border space-y-3">
            <button
              type="button"
              onClick={() => setShowAdvanced((v) => !v)}
              className="text-xs font-mono text-fg-dim hover:text-amber"
            >
              {showAdvanced ? "▼" : "▶"} Advanced stream tuning
            </button>
            {showAdvanced && (
              <div className="space-y-3 pl-2 border-l border-border">
                <div>
                  <span className="label">Auto SpecOps threshold</span>
                  <input
                    type="number"
                    min={0}
                    max={1}
                    step={0.05}
                    value={specopsThreshold}
                    onChange={(e) => setSpecopsThreshold(Number(e.target.value))}
                    className="input mt-1.5 max-w-[180px]"
                  />
                </div>
                <div>
                  <span className="label">worker_provenance (JSON, optional)</span>
                  <textarea
                    value={workerProvJson}
                    onChange={(e) => setWorkerProvJson(e.target.value)}
                    placeholder='{"tenant":"system",...}'
                    rows={3}
                    className="input mt-1.5 font-mono text-xs"
                  />
                </div>
              </div>
            )}
            <div className="flex items-center gap-2">
              <Shield className="w-4 h-4 text-cyan" />
              <span className="label">Sheldon Validator</span>
              <span className="text-[10px] font-mono text-fg-dim ml-auto">
                between-round claim verification
              </span>
            </div>
            <Toggle
              label="Validate"
              sub="Verify claims with web evidence"
              value={validate}
              onChange={setValidate}
              icon={<Shield className="w-4 h-4" />}
              tone="cyan"
            />
            {validate && (
              <>
                <div>
                  <span className="label">Validator Provider</span>
                  <div className="grid grid-cols-5 gap-2 mt-1.5">
                    {validatorProviders.map((p) => (
                      <ProviderChip
                        key={p.name}
                        active={validateProvider === p.name}
                        provider={p.name}
                        label={providerOptionLabel(p)}
                        disabled={!p.available}
                        onClick={() => setValidateProvider(p.name)}
                      />
                    ))}
                  </div>
                </div>
                <Toggle
                  label="Gate mode"
                  sub="Redact CONTRADICTED claims"
                  value={validateGate}
                  onChange={setValidateGate}
                  icon={<ShieldCheck className="w-4 h-4" />}
                  tone="amber"
                />
              </>
            )}
            <div className="flex items-center gap-2 mt-3">
              <Search className="w-4 h-4 text-amber" />
              <span className="label">Scope Auditor</span>
              <span className="text-[10px] font-mono text-fg-dim ml-auto">
                steering & boundary review (beyond frame check)
              </span>
            </div>
            <Toggle
              label="Enable"
              sub="Detect operator steering, framing, scope creep (preview: not yet wired)"
              value={scopeAuditor}
              onChange={setScopeAuditor}
              icon={<Search className="w-4 h-4" />}
              tone="amber"
              testId="scope-auditor-toggle"
            />
          </div>
          </div>
        </div>
    </>
  );

  const conveneButtonEl = (
    <div className="space-y-2">
      <button
        type="button"
        onClick={submit}
        disabled={!canStart || submitting}
        title={providerSelectionProblem ?? undefined}
        className={cn(
          "btn btn-primary w-full min-h-12 justify-center text-xs py-3",
          canStart && !submitting && variant !== "shell" && "animate-pulse-amber",
        )}
      >
        {submitting ? (
          <Loader2 className="w-4 h-4 animate-spin" />
        ) : (
          variant !== "shell" && <Play className="w-4 h-4" />
        )}
        {submitting ? "Convening…" : "Convene the Council"}
      </button>
      {providerSelectionProblem && (
        <div className="text-[10px] font-mono text-danger" data-testid="provider-selection-warning">
          {providerSelectionProblem}
        </div>
      )}
    </div>
  );

  const formSection = variant === "shell" ? (
    <>
      <div className="cg-record-head cg-convene-head">
        <div className="cg-record-kicker">
          <span className="text-fg-dim">
            <em className="text-amber not-italic font-semibold">File the matter</em>
          </span>
          <span className="text-fg-dim/60" aria-hidden>/</span>
          <RecordModeChip mode={wireMode} />
          <span className="text-fg-dim/60" aria-hidden>/</span>
          <span className="chip text-[9px] normal-case tracking-normal font-medium">
            {cabinet?.label ?? cabinetName}
          </span>
        </div>
        <div className="cg-convene-topic-wrap mt-3">
          <label htmlFor="convene-topic" className="cg-convene-matter-infield">
            The matter
          </label>
          <textarea
            id="convene-topic"
            ref={topicRef}
            value={topic}
            onChange={(e) => setTopic(e.target.value)}
            placeholder="State the question, decision, or proposal the council should deliberate on…"
            rows={2}
            className="cg-convene-topic"
            autoFocus
            aria-label="Proceeding statement"
          />
        </div>
        <div className="cg-convene-meta">
          <span>{topic.length} chars</span>
          {!blind && precedent.length > 0 && (
            <span className="cg-convene-meta-match">
              {precedent.length} prior match{precedent.length === 1 ? "" : "es"}
            </span>
          )}
        </div>
      </div>
      <div className="cg-convene-body">{formBody}</div>
      <div className="cg-convene-sticky-bar">{conveneButtonEl}</div>
    </>
  ) : (
    <section className="col-span-12 lg:col-span-8 space-y-6">
      {formBody}
      <div className="flex gap-3">{conveneButtonEl}</div>
    </section>
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
      <div className="space-y-6">
        <PrecedentAmbient matches={precedent} blind={blind} mode={precedentMode} />
        <div className="panel p-5">
          <div className="flex items-center gap-2 mb-2">
            <Database className="w-4 h-4 text-fg-dim" />
            <span className="label">Precedent index</span>
          </div>
          <p className="text-[10px] font-mono text-fg-dim mb-2">
            Rebuild JSONL index from session files (distinct from embeddings).
          </p>
          <button
            onClick={async () => {
              setReindexingPrecedent(true);
              try {
                const r = await api.precedentReindex();
                toast("success", `Precedent reindexed (${r.reindexed} sessions)`);
              } catch (e) {
                toast("error", e instanceof Error ? e.message : "Precedent reindex failed");
              }
              finally { setReindexingPrecedent(false); }
            }}
            disabled={reindexingPrecedent}
            className="btn btn-secondary text-xs w-full"
          >
            {reindexingPrecedent
              ? <><Loader2 className="w-3 h-3 animate-spin" /> Reindexing…</>
              : "Reindex precedent"}
          </button>
        </div>
        {embStats?.available && (
          <div className="panel p-5">
            <div className="flex items-center gap-2 mb-2">
              <Database className={cn("w-4 h-4", embStats.stale ? "text-amber" : "text-fg-dim")} />
              <span className="label">Memory</span>
              <span className={cn(
                "chip text-[10px] ml-auto",
                embStats.stale ? "chip-amber" : embStats.present ? "chip-success" : "",
              )}>
                {embStats.stale ? "stale" : embStats.present ? "ready" : "unbuilt"}
              </span>
            </div>
            {embStats.present && (
              <div className="text-[10px] font-mono text-fg-dim space-y-0.5">
                <div>{embStats.session_count} vectors · {embStats.session_index_count} sessions</div>
                <div>{embStats.model} · {embStats.vector_dim}d</div>
              </div>
            )}
            {(embStats.stale || !embStats.present) && (
              <button
                onClick={async () => {
                  setRebuilding(true);
                  try {
                    await api.embeddingsRebuild(!embStats.present);
                    loadEmbStats();
                  } catch {
                    toast("error", "Embeddings rebuild failed");
                  }
                  finally { setRebuilding(false); }
                }}
                disabled={rebuilding}
                className="btn btn-primary text-xs mt-2 w-full"
              >
                {rebuilding
                  ? <><Loader2 className="w-3 h-3 animate-spin" /> Rebuilding…</>
                  : <><Database className="w-3 h-3" /> {embStats.present ? "Rebuild index" : "Build index"}</>
                }
              </button>
            )}
          </div>
        )}
        <CabinetPreview cabinet={cabinet} />
      </div>
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

function Toggle({
  label,
  sub,
  value,
  onChange,
  icon,
  tone,
  testId,
}: {
  label: string;
  sub: string;
  value: boolean;
  onChange: (v: boolean) => void;
  icon: React.ReactNode;
  tone: "amber" | "cyan";
  testId?: string;
}) {
  return (
    <button
      type="button"
      data-testid={testId}
      onClick={() => onChange(!value)}
      className={cn(
        "flex items-start gap-3 p-3 rounded-md border text-left transition-all",
        value
          ? tone === "amber"
            ? "border-amber/50 bg-amber/5"
            : "border-cyan/50 bg-cyan/5"
          : "border-border bg-bg-overlay/40 hover:border-border-bright",
      )}
    >
      <span className={cn(value ? `text-${tone}` : "text-fg-muted")}>
        {icon}
      </span>
      <span>
        <span
          className={cn(
            "block text-sm font-medium",
            value ? `text-${tone}` : "text-fg",
          )}
        >
          {label}
        </span>
        <span className="block text-xs text-fg-dim">{sub}</span>
      </span>
    </button>
  );
}

function ModeChip({
  active,
  onClick,
  icon,
  label,
  sub,
}: {
  active: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  label: string;
  sub: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "text-left p-2 rounded-md border transition-all",
        active
          ? "border-amber/60 bg-amber/10"
          : "border-border bg-bg-overlay/40 hover:border-border-bright",
      )}
    >
      <div className={cn(
        "flex items-center gap-1.5 text-sm font-medium",
        active ? "text-amber" : "text-fg",
      )}>
        {icon}
        {label}
      </div>
      <div className="text-[10px] font-mono text-fg-dim mt-0.5">{sub}</div>
    </button>
  );
}

function ProviderChip({
  active,
  provider,
  label,
  disabled,
  onClick,
}: {
  active: boolean;
  provider: string;
  label?: string;
  disabled?: boolean;
  onClick: () => void;
}) {
  const tone = providerColor(provider);
  const activeBorder: Record<string, string> = {
    magenta: "border-magenta/60 bg-magenta/10",
    amber: "border-amber/60 bg-amber/10",
    success: "border-success/60 bg-success/10",
    cyan: "border-cyan/60 bg-cyan/10",
    muted: "border-border bg-bg-overlay",
  };
  const activeText: Record<string, string> = {
    magenta: "text-magenta",
    amber: "text-amber",
    success: "text-success",
    cyan: "text-cyan",
    muted: "text-fg",
  };
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={disabled ? `${label ?? provider} is unavailable` : undefined}
      className={cn(
        "text-center px-2 py-1.5 rounded-md border text-xs font-medium transition-all",
        active
          ? activeBorder[tone]
          : "border-border bg-bg-overlay/40 hover:border-border-bright",
        active ? activeText[tone] : "text-fg-muted",
        disabled && "cursor-not-allowed opacity-45 hover:border-border",
      )}
    >
      {label ?? provider.replaceAll("_", " ")}
    </button>
  );
}

function CabinetPreview({
  cabinet,
  variant = "default",
}: {
  cabinet?: Cabinet;
  variant?: "default" | "command";
}) {
  // Shell mode: selection lives in the center grid, so the rail collapses to a
  // one-line summary by default — full roster behind a disclosure.
  const [rosterOpen, setRosterOpen] = useState(false);
  if (!cabinet) return null;
  const shell = variant === "command";

  const seatList = (
    <div className="space-y-1">
      {cabinet.seats.map((s) => (
        <div key={s.name} className="flex items-center justify-between text-[10px] font-mono leading-tight">
          <span className="text-fg">{s.name}</span>
          <span className="text-fg-muted">{s.provider}</span>
        </div>
      ))}
      <div className="flex items-center justify-between text-[10px] font-mono pt-1.5 mt-1 border-t border-border leading-tight">
        <span className="text-amber">Chair</span>
        <span className="text-fg-muted">
          {cabinet.chair.provider} · {cabinet.chair.model}
        </span>
      </div>
    </div>
  );

  if (shell) {
    return (
      <div className="cg-command-panel cg-command-panel--tight">
        <button
          type="button"
          onClick={() => setRosterOpen((v) => !v)}
          aria-expanded={rosterOpen}
          data-testid="rail-roster-summary"
          className="w-full flex items-center gap-1.5 text-left text-[10px] font-mono leading-tight hover:text-fg transition-colors"
        >
          <span className="text-fg-dim shrink-0">{rosterOpen ? "▾" : "▸"}</span>
          <span className="text-amber font-semibold shrink-0">{cabinet.label}</span>
          <span className="text-fg-dim truncate">
            · {cabinet.seats.length} seats · {cabinet.rounds} rounds
          </span>
        </button>
        {rosterOpen && <div className="mt-2 pt-2 border-t border-border">{seatList}</div>}
      </div>
    );
  }

  return (
    <div className="panel p-5">
      <div className="flex items-center justify-between mb-2">
        <span className="label">Cabinet</span>
        <span className="chip chip-amber text-[9px]">{cabinet.rounds} rounds</span>
      </div>
      <div className="font-display font-bold text-fg-bright">{cabinet.label}</div>
      <div className="text-xs text-fg-muted mt-1 mb-4">{cabinet.description}</div>
      {seatList}
    </div>
  );
}
