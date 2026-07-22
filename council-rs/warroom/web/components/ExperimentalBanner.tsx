"use client";

import { AlertTriangle } from "lucide-react";

/** Shared warning for experimental modes whose output needs human scoring. */
export default function ExperimentalBanner({
  title,
  icon,
  testId,
  children,
}: {
  title: string;
  icon?: React.ReactNode;
  testId?: string;
  children?: React.ReactNode;
}) {
  return (
    <div
      data-testid={testId}
      className="panel border-amber/40 bg-amber/5 p-4 text-xs text-fg-muted space-y-2"
    >
      <div className="font-display font-bold text-amber flex items-center gap-2">
        {icon ?? <AlertTriangle className="w-4 h-4" />}
        {title}
      </div>
      {children}
      <p className="text-fg-dim font-mono text-[10px]">
        Experimental — validate output quality before relying on results.
      </p>
    </div>
  );
}
