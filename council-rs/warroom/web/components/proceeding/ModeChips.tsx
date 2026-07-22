import { sessionModeBadge } from "@/lib/session-mode";
import type { ExecutionRoute } from "@/lib/types";

export function ModeChip({ mode }: { mode: string }) {
  const badge = sessionModeBadge(mode);
  if (!badge) return null;
  return <span className={badge.chipClass}>{badge.label}</span>;
}

/** Header chip — always boxed; legacy "normal" reads as TEARDOWN. */
export function RecordModeChip({ mode }: { mode: string }) {
  const badge = sessionModeBadge(mode) ?? {
    label: "TEARDOWN",
    chipClass: "chip chip-teardown",
  };
  return <span className={badge.chipClass}>{badge.label}</span>;
}

export function ExecutionRouteChip({
  route,
  sensitivity,
}: {
  route?: ExecutionRoute;
  sensitivity?: string | null;
}) {
  if (!route || route === "unknown") return null;
  const governed = route === "governed";
  const label = governed
    ? `GOVERNED${sensitivity ? ` · ${sensitivity.toUpperCase()}` : ""}`
    : "DIRECT";
  return (
    <span
      data-testid="execution-route-chip"
      className={governed ? "chip border-amber/50 text-amber" : "chip text-fg-dim"}
      title={governed ? "All model calls were required to pass through Gateway" : "Model calls used direct provider or CLI transports"}
    >
      {label}
    </span>
  );
}
