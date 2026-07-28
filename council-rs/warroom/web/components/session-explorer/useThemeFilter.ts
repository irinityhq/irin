import { useEffect, useMemo, useState } from "react";
import { clusterSessionIds, selectedThemeLabels } from "@/lib/clusters";
import type { ClustersResponse, SessionIndexEntry } from "@/lib/types";

export function useThemeFilter(
  sessions: SessionIndexEntry[],
  clusters: ClustersResponse | null,
) {
  const [q, setQ] = useState("");
  const [clusterFilter, setClusterFilter] = useState<Set<number>>(new Set());
  const [themesOpen, setThemesOpen] = useState(false);

  useEffect(() => {
    if (clusterFilter.size > 0) setThemesOpen(true);
  }, [clusterFilter]);

  const filtered = useMemo(() => {
    const ids =
      clusterFilter.size && clusters
        ? clusterSessionIds(clusters.clusters, clusterFilter)
        : null;
    const ql = q.trim().toLowerCase();
    return sessions.filter((s) => {
      if (ids && !ids.has(s.id)) return false;
      if (!ql) return true;
      return (
        s.topic.toLowerCase().includes(ql) ||
        s.keywords?.some((k) => k.toLowerCase().includes(ql)) ||
        s.cabinet.toLowerCase().includes(ql) ||
        s.id.toLowerCase().includes(ql)
      );
    });
  }, [sessions, q, clusters, clusterFilter]);

  const activeThemes = useMemo(
    () =>
      clusterFilter.size && clusters
        ? selectedThemeLabels(clusters.clusters, clusterFilter, sessions)
        : [],
    [clusters, clusterFilter, sessions],
  );

  const toggleCluster = (id: number) =>
    setClusterFilter((prev) => {
      // Single-theme filter: one active theme at a time; click again to clear.
      if (prev.has(id) && prev.size === 1) return new Set();
      return new Set([id]);
    });

  const clearClusterFilter = () => setClusterFilter(new Set());

  return {
    q,
    setQ,
    clusterFilter,
    themesOpen,
    setThemesOpen,
    filtered,
    activeThemes,
    toggleCluster,
    clearClusterFilter,
  };
}
