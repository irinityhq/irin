import { Sparkles } from "lucide-react";
import { cn } from "@/lib/cn";
import type { PrecedentMatch } from "@/lib/types";

export function TopicPanel({
  variant,
  topic,
  setTopic,
  blind,
  precedent,
}: {
  variant: "standalone" | "shell";
  topic: string;
  setTopic: (v: string) => void;
  blind: boolean;
  precedent: PrecedentMatch[];
}) {
  return (
    <div className={cn(variant === "shell" ? "hidden" : "panel p-6 space-y-4 relative overflow-hidden")}>
      <div className="absolute inset-0 bg-amber-radial opacity-50 pointer-events-none" />
      <div className="relative">
        <div className="flex items-center gap-2 mb-2">
          <Sparkles className="w-4 h-4 text-amber" />
          <span className="label">Deliberation Topic</span>
        </div>
        <textarea
          value={topic}
          onChange={(e) => setTopic(e.target.value)}
          placeholder="State the question, decision, or proposal the council should deliberate on…"
          rows={5}
          className="input text-base resize-y min-h-[120px]"
          autoFocus
        />
        <div className="flex items-center justify-between mt-2 text-xs text-fg-dim font-mono">
          <span>{topic.length} chars</span>
          {!blind && precedent.length > 0 && (
            <span className="text-amber">
              {precedent.length} prior ruling
              {precedent.length === 1 ? "" : "s"} match
            </span>
          )}
        </div>
      </div>
    </div>
  );
}
