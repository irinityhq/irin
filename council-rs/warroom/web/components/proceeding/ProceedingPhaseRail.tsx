import { cn } from "@/lib/cn";
import type { ProceedingPhase } from "@/lib/proceeding-phases";

export function ProceedingPhaseRail({ phases }: { phases: ProceedingPhase[] }) {
  return (
    <div
      className="cg-phase-rail"
      aria-label="Deliberation phases"
      data-testid="phase-rail"
    >
      {phases.map((p) => (
        <div
          key={p.label}
          className={cn(
            "cg-phase",
            p.state === "done" && "done",
            p.state === "active" && "active",
          )}
        >
          <b>{p.label}</b>
          <span>{p.sub}</span>
        </div>
      ))}
    </div>
  );
}
