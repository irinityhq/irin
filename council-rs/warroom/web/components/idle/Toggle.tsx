import { cn } from "@/lib/cn";

export function Toggle({
  label,
  sub,
  value,
  onChange,
  icon,
  tone,
  testId,
}: {
  label: string;
  sub: string;
  value: boolean;
  onChange: (v: boolean) => void;
  icon: React.ReactNode;
  tone: "amber" | "cyan";
  testId?: string;
}) {
  return (
    <button
      type="button"
      data-testid={testId}
      onClick={() => onChange(!value)}
      className={cn(
        "flex items-start gap-3 p-3 rounded-md border text-left transition-all",
        value
          ? tone === "amber"
            ? "border-amber/50 bg-amber/5"
            : "border-cyan/50 bg-cyan/5"
          : "border-border bg-bg-overlay/40 hover:border-border-bright",
      )}
    >
      <span className={cn(value ? `text-${tone}` : "text-fg-muted")}>
        {icon}
      </span>
      <span>
        <span
          className={cn(
            "block text-sm font-medium",
            value ? `text-${tone}` : "text-fg",
          )}
        >
          {label}
        </span>
        <span className="block text-xs text-fg-dim">{sub}</span>
      </span>
    </button>
  );
}
