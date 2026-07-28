import {
  Compass,
  Eye,
  EyeOff,
  FolderTree,
  Search,
  Shield,
  ShieldAlert,
  ShieldCheck,
  Swords,
} from "lucide-react";
import { cn } from "@/lib/cn";
import type { Cabinet, DiscoverProvider } from "@/lib/types";
import type { DesktopRuntimeMode } from "@/lib/tauri";
import { providerOptionLabel } from "@/lib/use-discover";
import type { GatewaySensitivity } from "@/lib/ws";
import type { ToastType } from "../Toast";
import { GatewayRouting } from "./GatewayRouting";
import { ModeChip } from "./ModeChip";
import { ProviderChip } from "./ProviderChip";
import { Toggle } from "./Toggle";
import type { ConveneOptions } from "./useConveneOptions";

export function SessionControls({
  variant,
  options,
  cabinet,
  validatorProviders,
  governedAllowed,
  desktopMode,
  viaGateway,
  setViaGateway,
  sensitivity,
  setSensitivity,
  toast,
}: {
  variant: "standalone" | "shell";
  options: ConveneOptions;
  cabinet?: Cabinet;
  validatorProviders: DiscoverProvider[];
  governedAllowed: boolean;
  desktopMode: DesktopRuntimeMode | "detecting" | "unavailable";
  viaGateway: boolean;
  setViaGateway: (v: boolean) => void;
  sensitivity: GatewaySensitivity;
  setSensitivity: (v: GatewaySensitivity) => void;
  toast: (type: ToastType, message: string) => void;
}) {
  const {
    blind,
    setBlind,
    pause,
    setPause,
    maxRounds,
    setMaxRounds,
    mode,
    setMode,
    validate,
    setValidate,
    validateProvider,
    setValidateProvider,
    validateGate,
    setValidateGate,
    frameCheck,
    setFrameCheck,
    scopeAuditor,
    setScopeAuditor,
    budgetUsd,
    setBudgetUsd,
    tier,
    setTier,
    thenTearDown,
    setThenTearDown,
    specopsThreshold,
    setSpecopsThreshold,
    workerProvJson,
    setWorkerProvJson,
    showAdvanced,
    setShowAdvanced,
  } = options;

  return (
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
      <GatewayRouting
        governedAllowed={governedAllowed}
        desktopMode={desktopMode}
        viaGateway={viaGateway}
        setViaGateway={setViaGateway}
        sensitivity={sensitivity}
        setSensitivity={setSensitivity}
        toast={toast}
      />
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
  );
}
