"use client";

import { useCallback, useEffect, useState } from "react";
import { api } from "@/lib/api";
import type { EmbeddingStats } from "@/lib/types";
import type { ToastType } from "../Toast";

export function usePrecedentIndex(
  toast: (type: ToastType, message: string) => void,
) {
  const [embStats, setEmbStats] = useState<EmbeddingStats | null>(null);
  const [rebuilding, setRebuilding] = useState(false);
  const [reindexingPrecedent, setReindexingPrecedent] = useState(false);

  const loadEmbStats = useCallback(() => {
    api.embeddingsStats().then(setEmbStats).catch(() => {});
  }, []);

  useEffect(() => {
    loadEmbStats();
  }, [loadEmbStats]);

  const reindexPrecedent = async () => {
    setReindexingPrecedent(true);
    try {
      const r = await api.precedentReindex();
      toast("success", `Precedent reindexed (${r.reindexed} sessions)`);
    } catch (e) {
      toast("error", e instanceof Error ? e.message : "Precedent reindex failed");
    }
    finally { setReindexingPrecedent(false); }
  };

  const rebuildEmbeddings = async (full: boolean) => {
    setRebuilding(true);
    try {
      await api.embeddingsRebuild(full);
      loadEmbStats();
    } catch {
      toast("error", "Embeddings rebuild failed");
    }
    finally { setRebuilding(false); }
  };

  return {
    embStats,
    rebuilding,
    reindexingPrecedent,
    reindexPrecedent,
    rebuildEmbeddings,
  };
}
