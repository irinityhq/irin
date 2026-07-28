import { cn } from "@/lib/cn";

export function ModeChip({
  active,
  onClick,
  icon,
  label,
  sub,
}: {
  active: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  label: string;
  sub: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "text-left p-2 rounded-md border transition-all",
        active
          ? "border-amber/60 bg-amber/10"
          : "border-border bg-bg-overlay/40 hover:border-border-bright",
      )}
    >
      <div className={cn(
        "flex items-center gap-1.5 text-sm font-medium",
        active ? "text-amber" : "text-fg",
      )}>
        {icon}
        {label}
      </div>
      <div className="text-[10px] font-mono text-fg-dim mt-0.5">{sub}</div>
    </button>
  );
}
