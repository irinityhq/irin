"use client";

import type { ChangeEvent, RefObject } from "react";
import { FileUp, Users } from "lucide-react";
import { cn } from "@/lib/cn";
import type { Cabinet } from "@/lib/types";

export function CabinetListSidebar({
  cabinets,
  selected,
  onSelect,
  fileRef,
  onFile,
}: {
  cabinets: Cabinet[];
  selected: string;
  /** Switch the selection and clear any draft/save state for the old one. */
  onSelect: (name: string) => void;
  fileRef: RefObject<HTMLInputElement | null>;
  onFile: (e: ChangeEvent<HTMLInputElement>) => void;
}) {
  return (
    <aside className="col-span-12 lg:col-span-3 flex flex-col self-start rounded border border-border bg-bg-deep/60 overflow-hidden">
      <div className="px-3 pt-3">
        <p className="cg-section-label">
          <Users className="w-3.5 h-3.5 text-amber" />
          Cabinets
        </p>
      </div>
      <div className="px-1.5 pb-2 max-h-[52vh] overflow-y-auto overscroll-contain">
        {cabinets.map((c) => (
          <button
            key={c.name}
            onClick={() => onSelect(c.name)}
            className={cn("cg-session-row", selected === c.name && "selected")}
          >
            <div className="min-w-0">
              <div
                className={cn(
                  "text-[11px] font-mono font-semibold leading-snug truncate",
                  selected === c.name ? "text-amber" : "text-fg",
                )}
              >
                {c.label}
              </div>
              <div className="text-[10px] font-mono text-fg-dim mt-0.5">
                {c.seats.length} seats · {c.rounds} rounds
              </div>
            </div>
          </button>
        ))}
      </div>
      <div className="mt-auto border-t border-border p-3">
        <button
          data-testid="cabinet-import-button"
          onClick={() => fileRef.current?.click()}
          className="btn btn-cyan text-xs w-full justify-center"
        >
          <FileUp className="w-3.5 h-3.5" />
          Load YAML
        </button>
        <input
          ref={fileRef}
          data-testid="cabinet-import-input"
          type="file"
          accept=".yaml,.yml"
          onChange={onFile}
          className="hidden"
        />
        <p className="text-[10px] font-mono text-fg-dim mt-2 leading-relaxed">
          Import an external cabinet file, name it, and save it into the
          council registry.
        </p>
      </div>
    </aside>
  );
}
