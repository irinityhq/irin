"use client";

import { getModelsForProvider } from "@/lib/use-discover";
import type { CabinetSeat, DiscoverProvider } from "@/lib/types";
import { providerSelectOptions } from "./provider-options";

export function SeatsPanel({
  dirty,
  onResetModels,
  rescan,
  discoverLoading,
  seats,
  providerModelMap,
  providerOptions,
  updateSeat,
}: {
  dirty: boolean;
  /** Point every seat's model at the first discovered model for its provider. */
  onResetModels: () => void;
  rescan: () => Promise<void>;
  discoverLoading: boolean;
  seats: CabinetSeat[];
  providerModelMap: Record<string, string[]>;
  providerOptions: DiscoverProvider[];
  updateSeat: (i: number, field: string, value: string) => void;
}) {
  return (
    <div className="cg-command-panel space-y-3">
      <div className="flex items-center gap-2 flex-wrap">
        <span className="cg-section-label mb-0">Seats</span>
        {dirty && (
          <span className="chip chip-amber text-[9px] normal-case tracking-normal">
            Unsaved changes
          </span>
        )}
        <div className="ml-auto flex items-center gap-2">
          <button
            type="button"
            className="btn text-xs"
            onClick={onResetModels}
          >
            Reset models to discovered defaults
          </button>
          <button
            type="button"
            className="btn text-xs"
            onClick={() => void rescan()}
            disabled={discoverLoading}
          >
            Rescan providers
          </button>
        </div>
      </div>
      <div className="divide-y divide-border">
        {seats.map((s, i) => {
          const provModels = getModelsForProvider(providerModelMap, s.provider);
          return (
          <div key={i} className="py-3 first:pt-0 last:pb-0 space-y-2">
            <div className="text-[10px] font-mono uppercase tracking-widest text-fg-dim">
              Seat {i + 1}
            </div>
            <div className="grid grid-cols-3 gap-2">
              <input className="input text-xs" value={s.name}
                onChange={(e) => updateSeat(i, "name", e.target.value)} />
              <select className="input text-xs" value={s.provider}
                onChange={(e) => {
                  const newProv = e.target.value;
                  updateSeat(i, "provider", newProv);
                  // auto pick first model for new provider if available
                  const newModels = getModelsForProvider(providerModelMap, newProv);
                  if (newModels.length > 0 && !newModels.includes(s.model)) {
                    updateSeat(i, "model", newModels[0]);
                  }
                }}>
                {providerSelectOptions(providerOptions, s.provider)}
              </select>
              {provModels.length > 0 ? (
                <select className="input text-xs" value={s.model}
                  onChange={(e) => updateSeat(i, "model", e.target.value)}>
                  {provModels.map(m => (
                    <option key={m} value={m}>{m}</option>
                  ))}
                  {!provModels.includes(s.model) && s.model && (
                    <option value={s.model}>{s.model} (custom)</option>
                  )}
                </select>
              ) : (
                <input className="input text-xs" value={s.model}
                  onChange={(e) => updateSeat(i, "model", e.target.value)} />
              )}
            </div>
            <textarea className="input text-xs" rows={4} value={s.system}
              onChange={(e) => updateSeat(i, "system", e.target.value)} />
            {s.system_source && (
              <details className="text-xs">
                <summary className="cursor-pointer text-[10px] font-mono uppercase tracking-widest text-fg-dim hover:text-fg transition-colors">
                  Preview prompt source
                </summary>
                <pre className="rounded border border-border bg-bg-deep p-2 mt-1.5 overflow-auto max-h-40 text-[10px] font-mono text-fg-muted whitespace-pre-wrap leading-relaxed">{s.system_source}</pre>
              </details>
            )}
          </div>
          );
        })}
      </div>
    </div>
  );
}
