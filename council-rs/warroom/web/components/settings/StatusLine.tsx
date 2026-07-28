import { CheckCircle2, Loader2, XCircle } from "lucide-react";
import { cn } from "@/lib/cn";

export type ProbeStatus = "idle" | "loading" | "ok" | "fail" | "skip";

export function StatusLine({
  label,
  status,
  detail,
  testId,
}: {
  label: string;
  status: ProbeStatus;
  detail: string | null;
  testId?: string;
}) {
  return (
    <div
      className="flex items-start gap-2"
      data-testid={testId}
      data-health-state={testId ? status : undefined}
    >
      {status === "loading" && (
        <Loader2 className="w-3.5 h-3.5 animate-spin text-cyan shrink-0 mt-0.5" />
      )}
      {status === "ok" && (
        <CheckCircle2 className="w-3.5 h-3.5 text-success shrink-0 mt-0.5" />
      )}
      {(status === "fail" || status === "skip") && (
        <XCircle
          className={cn(
            "w-3.5 h-3.5 shrink-0 mt-0.5",
            status === "fail" ? "text-danger" : "text-fg-dim",
          )}
        />
      )}
      <div>
        <span className="text-fg">{label}</span>
        {detail && <div className="text-fg-dim">{detail}</div>}
      </div>
    </div>
  );
}
