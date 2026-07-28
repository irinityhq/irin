import { cn, providerColor } from "@/lib/cn";

export function ProviderChip({
  active,
  provider,
  label,
  disabled,
  onClick,
}: {
  active: boolean;
  provider: string;
  label?: string;
  disabled?: boolean;
  onClick: () => void;
}) {
  const tone = providerColor(provider);
  const activeBorder: Record<string, string> = {
    magenta: "border-magenta/60 bg-magenta/10",
    amber: "border-amber/60 bg-amber/10",
    success: "border-success/60 bg-success/10",
    cyan: "border-cyan/60 bg-cyan/10",
    muted: "border-border bg-bg-overlay",
  };
  const activeText: Record<string, string> = {
    magenta: "text-magenta",
    amber: "text-amber",
    success: "text-success",
    cyan: "text-cyan",
    muted: "text-fg",
  };
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={disabled ? `${label ?? provider} is unavailable` : undefined}
      className={cn(
        "text-center px-2 py-1.5 rounded-md border text-xs font-medium transition-all",
        active
          ? activeBorder[tone]
          : "border-border bg-bg-overlay/40 hover:border-border-bright",
        active ? activeText[tone] : "text-fg-muted",
        disabled && "cursor-not-allowed opacity-45 hover:border-border",
      )}
    >
      {label ?? provider.replaceAll("_", " ")}
    </button>
  );
}
