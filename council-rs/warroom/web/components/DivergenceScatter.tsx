"use client";

import { providerHex } from "@/lib/cn";
import type { DivergencePoint, SeatRef } from "@/lib/types";

/**
 * N02 divergence map. Renders the hand-rolled 2-component PCA projection
 * (`round_divergence` event) of each seat's response as a 2D scatter. Tighter
 * clusters = higher agreement; spread = divergence. The backend labels the
 * method "pca" truthfully (no UMAP — no mature Rust crate). Absence is
 * tolerated: the parent only mounts this when `points` exist.
 */
export default function DivergenceScatter({
  points,
  seats = [],
  size = 120,
}: {
  points: DivergencePoint[];
  seats?: SeatRef[];
  size?: number;
}) {
  if (!points || points.length === 0) return null;

  const pad = 10;
  const inner = size - pad * 2;
  const xs = points.map((p) => p.x);
  const ys = points.map((p) => p.y);
  const minX = Math.min(...xs);
  const maxX = Math.max(...xs);
  const minY = Math.min(...ys);
  const maxY = Math.max(...ys);
  const spanX = maxX - minX || 1;
  const spanY = maxY - minY || 1;

  const provFor = (seatName: string) =>
    seats.find((s) => s.name === seatName)?.provider ?? "";

  const place = (p: DivergencePoint) => ({
    cx: pad + ((p.x - minX) / spanX) * inner,
    // SVG y grows downward — flip so higher y renders toward the top.
    cy: pad + (1 - (p.y - minY) / spanY) * inner,
  });

  return (
    <div
      data-testid="divergence-scatter"
      className="flex items-center gap-3"
      title="Seat responses projected to 2D (PCA over embeddings) — spread = divergence"
    >
      <svg
        width={size}
        height={size}
        viewBox={`0 0 ${size} ${size}`}
        className="rounded border border-border bg-bg-deep/40 shrink-0"
        role="img"
        aria-label="Seat divergence scatter"
      >
        <line
          x1={pad}
          y1={size / 2}
          x2={size - pad}
          y2={size / 2}
          className="stroke-border"
          strokeWidth={0.5}
        />
        <line
          x1={size / 2}
          y1={pad}
          x2={size / 2}
          y2={size - pad}
          className="stroke-border"
          strokeWidth={0.5}
        />
        {points.map((p) => {
          const { cx, cy } = place(p);
          return (
            <circle
              key={p.seat}
              cx={cx}
              cy={cy}
              r={4}
              style={{ fill: providerHex(provFor(p.seat)) }}
              data-seat={p.seat}
            >
              <title>{p.seat}</title>
            </circle>
          );
        })}
      </svg>
      <ul className="space-y-0.5 text-[10px] font-mono text-fg-dim">
        {points.map((p) => (
          <li key={p.seat} className="flex items-center gap-1.5">
            <span
              className="inline-block w-2 h-2 rounded-full"
              style={{ background: providerHex(provFor(p.seat)) }}
            />
            <span className="text-fg-muted truncate max-w-[120px]">{p.seat}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
