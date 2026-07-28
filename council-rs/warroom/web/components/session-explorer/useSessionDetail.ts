import { useEffect, useState } from "react";
import { api } from "@/lib/api";
import type { LineageResponse, SessionDetail } from "@/lib/types";
import { errorMessage } from "./errorMessage";

export function useSessionDetail(selected: string | null) {
  const [detail, setDetail] = useState<SessionDetail | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [detailReload, setDetailReload] = useState(0);
  const [lineage, setLineage] = useState<LineageResponse | null>(null);

  useEffect(() => {
    if (!selected) {
      setDetail(null);
      setDetailError(null);
      setDetailLoading(false);
      setLineage(null);
      return;
    }
    let cancelled = false;
    setDetail(null);
    setDetailError(null);
    setDetailLoading(true);
    setLineage(null);
    api
      .session(selected)
      .then((d) => {
        if (!cancelled) setDetail(d as SessionDetail);
      })
      .catch((e) => {
        if (!cancelled) setDetailError(errorMessage(e));
      })
      .finally(() => {
        if (!cancelled) setDetailLoading(false);
      });
    api
      .lineage(selected)
      .then((value) => {
        if (!cancelled) setLineage(value);
      })
      .catch(() => {
        if (!cancelled) setLineage(null);
      });
    return () => {
      cancelled = true;
    };
  }, [selected, detailReload]);

  const retryDetail = () => {
    setDetailError(null);
    setDetailLoading(true);
    setDetailReload((attempt) => attempt + 1);
  };

  return { detail, detailError, detailLoading, lineage, retryDetail };
}
