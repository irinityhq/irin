import { useEffect, useState, type Dispatch, type SetStateAction } from "react";
import { api } from "@/lib/api";
import { HISTORY_SESSION_LIST_LIMIT } from "@/lib/clusters";
import type { ClustersResponse, SessionIndexEntry } from "@/lib/types";
import { errorMessage } from "./errorMessage";

export function useSessionList({
  apiStatus,
  apiError,
  onRetryConnection,
  setSelected,
}: {
  apiStatus: "loading" | "online" | "error";
  apiError?: string | null;
  onRetryConnection?: () => void;
  setSelected: Dispatch<SetStateAction<string | null>>;
}) {
  const [sessions, setSessions] = useState<SessionIndexEntry[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [listReload, setListReload] = useState(0);
  const [clusters, setClusters] = useState<ClustersResponse | null>(null);

  useEffect(() => {
    if (apiStatus === "error") {
      setLoading(false);
      setSessions([]);
      setLoadError(apiError ?? "Council bridge unreachable");
      setSelected(null);
      return;
    }
    if (apiStatus === "loading") {
      setLoading(true);
      return;
    }
    setLoading(true);
    setLoadError(null);
    let cancelled = false;
    api
      .sessions(HISTORY_SESSION_LIST_LIMIT)
      .then((r) => {
        if (cancelled) return;
        setSessions(r.sessions);
        setSelected((current) =>
          current && r.sessions.some((session) => session.id === current)
            ? current
            : null,
        );
        setLoading(false);
      })
      .catch((e) => {
        if (cancelled) return;
        setSessions([]);
        setSelected(null);
        setLoadError(errorMessage(e));
        setLoading(false);
      });
    api.clusters().then(setClusters).catch(() => setClusters(null));
    return () => {
      cancelled = true;
    };
  }, [apiStatus, apiError, listReload, setSelected]);

  const retryList = () => {
    setLoadError(null);
    setLoading(true);
    if (apiStatus === "error") onRetryConnection?.();
    setListReload((attempt) => attempt + 1);
  };

  return { sessions, loadError, loading, clusters, retryList };
}
