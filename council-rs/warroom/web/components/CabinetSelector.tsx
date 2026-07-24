"use client";

import { useState } from "react";
import { Users } from "lucide-react";
import { cn } from "@/lib/cn";
import { cabinetMissingProviders } from "@/lib/cabinet-selection";
import type { Cabinet } from "@/lib/types";

export default function CabinetSelector({
  cabinets,
  selected,
  onSelect,
  providersAvailable,
  variant = "default",
  embedded = false,
}: {
  cabinets: Cabinet[];
  selected: string;
  onSelect: (n: string) => void;
  /** Available transport IDs from the Discover inventory; null while unknown. */
  providersAvailable: readonly string[] | null;
  variant?: "default" | "command";
  /** Flat layout inside convene body — card grid, triads collapsed by default. */
  embedded?: boolean;
}) {
  const cabs = cabinets.filter((c) => !c.is_triad);
  const triads = cabinets.filter((c) => c.is_triad);
  const shell = variant === "command";
  const [triadsOpen, setTriadsOpen] = useState(!embedded);

  const rootClass = embedded
    ? "space-y-2.5"
    : shell
      ? "cg-command-panel space-y-3"
      : "panel p-5 space-y-4";

  return (
    <div className={rootClass} data-testid="cabinet-selector">
      {!embedded && (
        <div className="flex items-center gap-2">
          <Users className="w-3.5 h-3.5 text-amber" />
          <span className={shell ? "cg-section-label mb-0" : "label"}>Cabinet</span>
        </div>
      )}
      {cabinets.length === 0 ? (
        <div
          data-testid="cabinet-empty"
          className="rounded-md border border-danger/30 bg-danger/5 p-3 text-xs font-mono text-danger"
        >
          Cabinet list unavailable. Check the backend connection above.
        </div>
      ) : (
        <>
          {cabs.length > 0 && (
            <div>
              {!embedded && (
                <div className="text-[10px] font-mono font-semibold uppercase tracking-widest text-fg-dim mb-2">
                  Embedded
                </div>
              )}
              <div className="grid grid-cols-2 md:grid-cols-3 gap-1.5">
                {cabs.map((c) => (
                  <CabinetChip
                    key={c.name}
                    cabinet={c}
                    active={selected === c.name}
                    onClick={() => onSelect(c.name)}
                    providersAvailable={providersAvailable}
                    compact={embedded}
                  />
                ))}
              </div>
            </div>
          )}
          {triads.length > 0 && (
            <div>
              {embedded ? (
                <button
                  type="button"
                  onClick={() => setTriadsOpen((v) => !v)}
                  className="text-[10px] font-mono font-semibold uppercase tracking-widest text-fg-dim hover:text-amber transition-colors"
                >
                  {triadsOpen ? "▾" : "▸"} Domain triads ({triads.length})
                </button>
              ) : (
                <div className="text-[10px] font-mono font-semibold uppercase tracking-widest text-fg-dim mb-2">
                  Domain Triads
                </div>
              )}
              {triadsOpen && (
                <div className={cn("grid grid-cols-2 md:grid-cols-3 gap-1.5", embedded && "mt-1.5")}>
                  {triads.map((c) => (
                    <CabinetChip
                      key={c.name}
                      cabinet={c}
                      active={selected === c.name}
                      onClick={() => onSelect(c.name)}
                      providersAvailable={providersAvailable}
                      compact={embedded}
                    />
                  ))}
                </div>
              )}
            </div>
          )}
        </>
      )}
    </div>
  );
}

function CabinetChip({
  cabinet,
  active,
  onClick,
  providersAvailable,
  compact = false,
}: {
  cabinet: Cabinet;
  active: boolean;
  onClick: () => void;
  providersAvailable: readonly string[] | null;
  compact?: boolean;
}) {
  // Inventory null → treat as unknown (no missing list) so chips are not a
  // danger-red Christmas tree before the Discover inventory arrives.
  const missing = providersAvailable
    ? cabinetMissingProviders(cabinet, providersAvailable)
    : [];
  const available = missing.length === 0;

  return (
    <button
      data-testid="cabinet-chip"
      data-cabinet-name={cabinet.name}
      data-cabinet-available={available ? "true" : "false"}
      onClick={onClick}
      className={cn(
        "text-left rounded-md border transition-all",
        compact ? "p-2" : "p-3",
        active
          ? "border-amber/60 bg-amber/10 shadow-[inset_0_1px_0_rgba(229,163,58,0.1)]"
          : "border-border/60 bg-bg-overlay/30 hover:border-amber/30 hover:bg-bg-overlay/60 hover:-translate-y-px",
        // Unavailable cabinets stay selectable (requirements stay visible) but
        // mute the card — danger red is reserved for real action/system errors.
        !available && "opacity-55",
      )}
    >
      <div
        className={cn(
          "font-mono font-semibold",
          compact ? "text-[10px]" : "text-[11px]",
          active ? "text-amber" : available ? "text-fg" : "text-fg-muted",
        )}
      >
        {cabinet.label}
      </div>
      <div className="text-[9px] font-mono text-fg-dim mt-0.5 leading-tight">
        {cabinet.seats.length} seats · {cabinet.rounds} rounds
        {missing.length > 0 && (
          <span className="text-fg-muted ml-1" data-testid="cabinet-need">
            (need {missing.join(", ")})
          </span>
        )}
      </div>
    </button>
  );
}
