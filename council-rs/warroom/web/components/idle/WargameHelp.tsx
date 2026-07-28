import { Swords } from "lucide-react";
import type { Cabinet } from "@/lib/types";

export function WargameHelp({
  variant,
  cabinet,
}: {
  variant: "standalone" | "shell";
  cabinet?: Cabinet;
}) {
  return (
    <div
      data-testid="wargame-idle-help"
      className="panel p-4 text-xs text-fg-muted space-y-2"
    >
      <div className="font-display font-bold text-fg flex items-center gap-2">
        <Swords className="w-4 h-4" />
        Wargame cabinet
      </div>
      <p className={variant === "shell" ? "text-[11px]" : undefined}>
        {variant === "shell" ? (
          "MDMP-style adversarial analysis: Blue plans, Red attacks, White arbitrates, and Green audits feasibility."
        ) : (
          <>
            MDMP-style adversarial course-of-action analysis: Red attacks the
            plan, Blue defends it, White arbitrates, Green audits
            feasibility. Convene from here as usual — no CLI flags needed
            (terminal parity: <code className="text-cyan">--wargame</code>).
            The premortem direct-fire twin lives in the{" "}
            <strong>Direct Fire</strong> tab.
          </>
        )}
      </p>
      {variant !== "shell" && cabinet && cabinet.seats.length > 0 && (
        <div className="font-mono text-[10px] text-fg-dim space-y-0.5">
          <div className="text-fg-muted">Seat roles:</div>
          {cabinet.seats.map((s) => (
            <div key={s.name}>
              <span className="text-fg">{s.name}</span> · {s.provider}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
