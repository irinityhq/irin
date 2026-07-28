import type { Cabinet } from "@/lib/types";

export function CabinetHeader({ cab }: { cab: Cabinet }) {
  return (
    <div className="cg-command-panel">
      <div className="text-[10px] font-mono uppercase tracking-widest text-fg-dim mb-1.5">
        {cab.name}
      </div>
      <div className="font-display font-semibold text-xl text-fg-bright leading-snug">
        {cab.label}
      </div>
      <div className="text-sm text-fg-muted mt-1 leading-relaxed">
        {cab.description}
      </div>
    </div>
  );
}
