"use client";

import { Loader2, Play, UploadCloud } from "lucide-react";
import { cn } from "@/lib/cn";

export function SavePanel({
  saveName,
  saveNameValid,
  setSaveNameOverride,
  saveToCouncil,
  saving,
  providerAvailabilityProblem,
  onRun,
  runKey,
  dirty,
  saveError,
  savedPath,
}: {
  saveName: string;
  saveNameValid: boolean;
  setSaveNameOverride: (name: string) => void;
  saveToCouncil: () => Promise<void>;
  saving: boolean;
  providerAvailabilityProblem: string | null;
  onRun?: (cabinetKey: string) => void;
  /** Registry key Run launches: the just-saved key, else the selection. */
  runKey: string;
  dirty: boolean;
  saveError: string | null;
  savedPath: string | null;
}) {
  return (
    <div className="cg-command-panel space-y-3">
      <span className="cg-section-label mb-0">Save to council</span>
      <p className="text-[10px] font-mono text-fg-dim leading-relaxed">
        Writes cabinets/&lt;name&gt;.yaml on the server (built-in names
        are protected). The saved cabinet appears in the list without a
        restart and works from the CLI via --cabinet.
      </p>
      <div className="flex items-end gap-2 flex-wrap">
        <div className="flex-1 min-w-[200px]">
          <span className="label">Registry name</span>
          <input
            data-testid="cabinet-save-name"
            className={cn(
              "input mt-1.5 w-full text-xs",
              saveName && !saveNameValid && "border-danger/60",
            )}
            value={saveName}
            onChange={(e) => setSaveNameOverride(e.target.value)}
            placeholder="my-cabinet"
          />
        </div>
        <button
          data-testid="cabinet-save-submit"
          onClick={() => void saveToCouncil()}
          disabled={saving || !!providerAvailabilityProblem}
          title={providerAvailabilityProblem ?? undefined}
          className="btn btn-primary text-xs"
        >
          {saving ? (
            <Loader2 className="w-3.5 h-3.5 animate-spin" />
          ) : (
            <UploadCloud className="w-3.5 h-3.5" />
          )}
          Save to council
        </button>
        {onRun && (
          <button
            data-testid="cabinet-run"
            onClick={() => onRun(runKey)}
            disabled={!!dirty || !!providerAvailabilityProblem}
            title={
              providerAvailabilityProblem
                ? providerAvailabilityProblem
                : dirty
                ? "Save to council first — Run launches the registry version"
                : "Pre-select this cabinet on the Deliberate panel"
            }
            className="btn btn-cyan text-xs"
          >
            <Play className="w-3.5 h-3.5" />
            Run deliberation
          </button>
        )}
      </div>
      {providerAvailabilityProblem && (
        <div
          data-testid="cabinet-provider-warning"
          className="text-[11px] font-mono text-warning"
        >
          {providerAvailabilityProblem}
        </div>
      )}
      {saveError && (
        <div
          data-testid="cabinet-save-error"
          className="text-[11px] font-mono text-danger"
        >
          {saveError}
        </div>
      )}
      {savedPath && (
        <div
          data-testid="cabinet-save-success"
          className="text-[11px] font-mono text-success"
        >
          Saved → {savedPath}
        </div>
      )}
    </div>
  );
}
