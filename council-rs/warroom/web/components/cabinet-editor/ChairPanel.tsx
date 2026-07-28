"use client";

import { getModelsForProvider } from "@/lib/use-discover";
import type { Cabinet, DiscoverProvider } from "@/lib/types";
import { providerSelectOptions } from "./provider-options";

export function ChairPanel({
  chair,
  providerModelMap,
  providerOptions,
  updateChair,
}: {
  chair: Cabinet["chair"] | undefined;
  providerModelMap: Record<string, string[]>;
  providerOptions: DiscoverProvider[];
  updateChair: (field: string, value: string) => void;
}) {
  return (
    <div className="cg-command-panel space-y-3">
      <span className="cg-section-label mb-0">Chair</span>
      <div className="grid grid-cols-2 gap-2">
        <select
          className="input text-xs"
          value={ chair?.provider || "" }
          onChange={(e) => {
            const newProv = e.target.value;
            updateChair("provider", newProv);
            const newModels = getModelsForProvider(providerModelMap, newProv);
            const chairModel = chair?.model || "";
            if (newModels.length > 0 && !newModels.includes(chairModel)) {
              updateChair("model", newModels[0]);
            }
          }}
        >
          <option value="">-- provider --</option>
          {providerSelectOptions(
            providerOptions,
            chair?.provider || "",
          )}
        </select>
        {(() => {
          const chairProv = chair?.provider || "";
          const chairModels = getModelsForProvider(providerModelMap, chairProv);
          return chairModels.length > 0 ? (
            <select
              className="input text-xs"
              value={ chair?.model || "" }
              onChange={(e) => updateChair("model", e.target.value)}
            >
              {chairModels.map((m) => (
                <option key={m} value={m}>{m}</option>
              ))}
            </select>
          ) : (
            <input
              className="input text-xs"
              value={ chair?.model || "" }
              onChange={(e) => updateChair("model", e.target.value)}
            />
          );
        })()}
      </div>
    </div>
  );
}
