import { Database, Loader2 } from "lucide-react";
import { cn } from "@/lib/cn";
import type { Cabinet, PrecedentMatch } from "@/lib/types";
import PrecedentAmbient from "../PrecedentAmbient";
import { CabinetPreview } from "./CabinetPreview";
import type { usePrecedentIndex } from "./usePrecedentIndex";

export function StandaloneRail({
  precedent,
  blind,
  precedentMode,
  cabinet,
  index,
}: {
  precedent: PrecedentMatch[];
  blind: boolean;
  precedentMode: "semantic" | "keyword";
  cabinet?: Cabinet;
  index: ReturnType<typeof usePrecedentIndex>;
}) {
  const {
    embStats,
    rebuilding,
    reindexingPrecedent,
    reindexPrecedent,
    rebuildEmbeddings,
  } = index;

  return (
    <div className="space-y-6">
      <PrecedentAmbient matches={precedent} blind={blind} mode={precedentMode} />
      <div className="panel p-5">
        <div className="flex items-center gap-2 mb-2">
          <Database className="w-4 h-4 text-fg-dim" />
          <span className="label">Precedent index</span>
        </div>
        <p className="text-[10px] font-mono text-fg-dim mb-2">
          Rebuild JSONL index from session files (distinct from embeddings).
        </p>
        <button
          onClick={reindexPrecedent}
          disabled={reindexingPrecedent}
          className="btn btn-secondary text-xs w-full"
        >
          {reindexingPrecedent
            ? <><Loader2 className="w-3 h-3 animate-spin" /> Reindexing…</>
            : "Reindex precedent"}
        </button>
      </div>
      {embStats?.available && (
        <div className="panel p-5">
          <div className="flex items-center gap-2 mb-2">
            <Database className={cn("w-4 h-4", embStats.stale ? "text-amber" : "text-fg-dim")} />
            <span className="label">Memory</span>
            <span className={cn(
              "chip text-[10px] ml-auto",
              embStats.stale ? "chip-amber" : embStats.present ? "chip-success" : "",
            )}>
              {embStats.stale ? "stale" : embStats.present ? "ready" : "unbuilt"}
            </span>
          </div>
          {embStats.present && (
            <div className="text-[10px] font-mono text-fg-dim space-y-0.5">
              <div>{embStats.session_count} vectors · {embStats.session_index_count} sessions</div>
              <div>{embStats.model} · {embStats.vector_dim}d</div>
            </div>
          )}
          {(embStats.stale || !embStats.present) && (
            <button
              onClick={() => rebuildEmbeddings(!embStats.present)}
              disabled={rebuilding}
              className="btn btn-primary text-xs mt-2 w-full"
            >
              {rebuilding
                ? <><Loader2 className="w-3 h-3 animate-spin" /> Rebuilding…</>
                : <><Database className="w-3 h-3" /> {embStats.present ? "Rebuild index" : "Build index"}</>
              }
            </button>
          )}
        </div>
      )}
      <CabinetPreview cabinet={cabinet} />
    </div>
  );
}
