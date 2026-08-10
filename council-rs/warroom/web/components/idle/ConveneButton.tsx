import { Loader2, Play } from "lucide-react";
import { cn } from "@/lib/cn";
import { isCouncilLoadingBlocker } from "./conveneBlocker";

export function ConveneButton({
  canStart,
  submitting,
  providerSelectionProblem,
  variant,
  onSubmit,
}: {
  canStart: boolean;
  submitting: boolean;
  providerSelectionProblem: string | null;
  variant: "standalone" | "shell";
  onSubmit: () => void;
}) {
  return (
    <div className="space-y-2">
      <button
        type="button"
        onClick={onSubmit}
        disabled={!canStart || submitting}
        title={providerSelectionProblem ?? undefined}
        className={cn(
          "btn btn-primary w-full min-h-12 justify-center text-xs py-3",
          canStart && !submitting && variant !== "shell" && "animate-pulse-amber",
        )}
      >
        {submitting ? (
          <Loader2 className="w-4 h-4 animate-spin" />
        ) : (
          variant !== "shell" && <Play className="w-4 h-4" />
        )}
        {submitting ? "Convening…" : "Convene the Council"}
      </button>
      {providerSelectionProblem && (
        <div
          className={cn(
            "text-[10px] font-mono",
            isCouncilLoadingBlocker(providerSelectionProblem)
              ? "text-fg-dim"
              : "text-danger",
          )}
          data-testid="provider-selection-warning"
        >
          {providerSelectionProblem}
        </div>
      )}
    </div>
  );
}
