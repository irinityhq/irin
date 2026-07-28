import { useMemo } from "react";
import { ChevronRight, X } from "lucide-react";
import { buildThemeRows } from "@/lib/clusters";
import { cn } from "@/lib/cn";
import type { ClustersResponse, SessionIndexEntry } from "@/lib/types";

export function ThemesDisclosure({
  clusters,
  sessions,
  selected,
  open,
  onOpenChange,
  activeThemeLabel,
  onToggle,
  onClear,
}: {
  clusters: ClustersResponse;
  sessions: SessionIndexEntry[];
  selected: ReadonlySet<number>;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  activeThemeLabel: string | null;
  onToggle: (id: number) => void;
  onClear: () => void;
}) {
  const themeCount = clusters.clusters.length;

  return (
    <div className="cg-themes-disclosure" data-testid="cluster-tile">
      <button
        type="button"
        className="cg-themes-disclosure-toggle"
        aria-expanded={open}
        onClick={() => onOpenChange(!open)}
      >
        <ChevronRight
          className={cn(
            "w-3.5 h-3.5 shrink-0 text-fg-dim transition-transform duration-150",
            open && "rotate-90",
          )}
        />
        <span className="font-semibold uppercase tracking-widest text-fg-muted">Themes</span>
        <span className="text-fg-dim tabular-nums">({themeCount})</span>
        {activeThemeLabel && !open && (
          <span className="ml-auto truncate max-w-[45%] text-amber font-medium normal-case tracking-normal">
            {activeThemeLabel}
          </span>
        )}
      </button>
      {open && (
        <ThemesPanel
          clusters={clusters}
          sessions={sessions}
          selected={selected}
          onToggle={onToggle}
          onClear={onClear}
        />
      )}
    </div>
  );
}

function ThemesPanel({
  clusters,
  sessions,
  selected,
  onToggle,
  onClear,
}: {
  clusters: ClustersResponse;
  sessions: SessionIndexEntry[];
  selected: ReadonlySet<number>;
  onToggle: (id: number) => void;
  onClear: () => void;
}) {
  const rows = useMemo(
    () => buildThemeRows(clusters.clusters, sessions),
    [clusters.clusters, sessions],
  );
  const listNote =
    clusters.n_sessions > sessions.length
      ? `Latest ${sessions.length} of ${clusters.n_sessions} indexed proceedings loaded.`
      : null;

  return (
    <div className="cg-themes-panel">
      <p className="text-[9px] font-mono text-fg-dim mb-1.5 leading-relaxed">
        Grouped by similarity — filter proceedings below.
      </p>
      <div
        className="grid grid-cols-[1fr_auto] gap-2 px-1 pb-1 text-[9px] font-mono font-semibold uppercase tracking-widest text-fg-dim"
        title={
          listNote ??
          "Proceedings in the loaded list that match this theme when selected"
        }
      >
        <span>Keywords</span>
        <span>In list</span>
      </div>
      {selected.size > 0 && (
        <button
          type="button"
          data-testid="cluster-clear"
          onClick={onClear}
          className="mb-1.5 flex items-center gap-1 text-[10px] font-mono text-amber hover:text-amber/80"
        >
          <X className="w-3 h-3" />
          Clear theme filter
        </button>
      )}
      <div className="space-y-0">
        {rows.map((row) => {
          const on = selected.has(row.cluster.id);
          const inert = row.filterable === 0;
          return (
            <button
              key={row.cluster.id}
              type="button"
              data-testid="cluster-chip"
              aria-pressed={on}
              disabled={inert}
              onClick={() => !inert && onToggle(row.cluster.id)}
              title={
                inert
                  ? `${row.label}\nNo members in the loaded proceedings window.\n${row.countTitle}`
                  : row.sample
                    ? `${row.label}\n${row.countTitle}\nExample: ${row.sample}`
                    : `${row.label}\n${row.countTitle}`
              }
              className={cn(
                "cg-theme-entry group w-full text-left rounded-sm transition-colors",
                on && "selected",
                inert && "inert",
              )}
            >
              <div className="min-w-0">
                <div
                  className={cn(
                    "text-[11px] font-mono leading-snug",
                    on ? "text-fg font-medium" : "text-fg-muted",
                  )}
                >
                  {row.label}
                </div>
                {inert && (
                  <div className="text-[9px] font-mono text-fg-dim mt-0.5">
                    none in loaded window
                  </div>
                )}
                {row.sample && !inert && (
                  <div className="cg-theme-example">e.g. &quot;{row.sample}&quot;</div>
                )}
              </div>
              <span
                className={cn(
                  "cg-theme-count",
                  (row.filterable > 0 || on) && "active",
                )}
                title={row.countTitle}
              >
                {row.countText}
              </span>
            </button>
          );
        })}
      </div>
    </div>
  );
}
