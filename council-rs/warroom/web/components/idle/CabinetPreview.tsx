"use client";

import { useState } from "react";
import type { Cabinet } from "@/lib/types";

export function CabinetPreview({
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
