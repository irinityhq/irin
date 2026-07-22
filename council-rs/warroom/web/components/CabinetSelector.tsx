"use client";

import { useState } from "react";
import { Users } from "lucide-react";
import { cn } from "@/lib/cn";
import type { Cabinet, HealthResponse } from "@/lib/types";

export default function CabinetSelector({
  cabinets,
  selected,
  onSelect,
  health,
  variant = "default",
  embedded = false,
}: {
  cabinets: Cabinet[];
  selected: string;
  onSelect: (n: string) => void;
  health: HealthResponse | null;
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
    <div className={rootClass}>
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
                    health={health}
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
                      health={health}
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

function cabinetMissing(cabinet: Cabinet, health: HealthResponse | null) {
  const need = new Set(cabinet.seats.map((s) => s.provider));
  need.add(cabinet.chair.provider);
  const have = health ? new Set(health.providers_available) : new Set();
  return [...need].filter((p) => !have.has(p));
}

function CabinetChip({
  cabinet,
  active,
  onClick,
  health,
  compact = false,
}: {
  cabinet: Cabinet;
  active: boolean;
  onClick: () => void;
  health: HealthResponse | null;
  compact?: boolean;
}) {
  const missing = cabinetMissing(cabinet, health);

  return (
    <button
      data-testid="cabinet-chip"
      data-cabinet-name={cabinet.name}
      onClick={onClick}
      className={cn(
        "text-left rounded-md border transition-all",
        compact ? "p-2" : "p-3",
        active
          ? "border-amber/60 bg-amber/10 shadow-[inset_0_1px_0_rgba(229,163,58,0.1)]"
          : "border-border/60 bg-bg-overlay/30 hover:border-amber/30 hover:bg-bg-overlay/60 hover:-translate-y-px",
        missing.length > 0 && "opacity-60",
      )}
    >
      <div
        className={cn(
          "font-mono font-semibold",
          compact ? "text-[10px]" : "text-[11px]",
          active ? "text-amber" : "text-fg",
        )}
      >
        {cabinet.label}
      </div>
      <div className="text-[9px] font-mono text-fg-dim mt-0.5 leading-tight">
        {cabinet.seats.length} seats · {cabinet.rounds} rounds
        {missing.length > 0 && (
          <span className="text-danger ml-1">
            (need {missing.join(", ")})
          </span>
        )}
      </div>
    </button>
  );
}
