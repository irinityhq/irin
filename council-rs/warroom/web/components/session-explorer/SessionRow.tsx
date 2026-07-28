import { cn } from "@/lib/cn";
import { proceedingTitle } from "@/lib/proceeding-display";
import type { SessionIndexEntry } from "@/lib/types";
import { ModeChip } from "../proceeding/ModeChips";

export function SessionRow({
  entry: s,
  selected,
  compareMode,
  onClick,
}: {
  entry: SessionIndexEntry;
  selected: boolean;
  compareMode: string | null;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "cg-session-row",
        selected && "selected",
        compareMode === s.id && "border-l-cyan",
      )}
    >
      <div>
        <div className="text-[10px] font-mono text-fg-dim mb-0.5">
          <span className="text-fg-muted">{s.id.slice(0, 12)}</span>
          {" · "}
          {s.ts.slice(0, 10)}
          {" · "}
          <ModeChip mode={s.mode} />
        </div>
        <div
          className="text-[11px] leading-snug text-fg-muted line-clamp-2"
          title={s.topic}
        >
          {proceedingTitle(s.topic, 96)}
        </div>
      </div>
      <div
        className={cn(
          "text-[10px] font-semibold font-mono pt-0.5",
          selected ? "text-success" : "text-fg-dim",
        )}
      >
        {Math.round((s.convergence ?? 0) * 100)}%
      </div>
    </button>
  );
}
