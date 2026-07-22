import { cn, convergenceTone, fmtCost, fmtLatency, fmtTokens } from "@/lib/cn";

export function ProceedingMetrics({
  rounds,
  tokens,
  costUsd,
  latencyMs,
  convergence,
}: {
  rounds: number;
  tokens: number;
  costUsd: number;
  latencyMs: number;
  convergence?: number;
}) {
  const conv = convergence ?? 0;
  return (
    <div className="cg-metrics">
      <div className="cg-metric">
        <span>Rounds</span>
        <b>{rounds}</b>
      </div>
      <div className="cg-metric cg-metric--amber">
        <span>Tokens</span>
        <b>{fmtTokens(tokens)}</b>
      </div>
      <div className="cg-metric cg-metric--amber">
        <span>Cost</span>
        <b>{fmtCost(costUsd)}</b>
      </div>
      <div className="cg-metric">
        <span>Latency</span>
        <b>{fmtLatency(latencyMs)}</b>
      </div>
      {convergence != null && (
        <div
          className={cn(
            "cg-metric",
            convergenceTone(conv) === "success" && "cg-metric--success",
          )}
        >
          <span>Conv</span>
          <b>{Math.round(conv * 100)}%</b>
        </div>
      )}
    </div>
  );
}
