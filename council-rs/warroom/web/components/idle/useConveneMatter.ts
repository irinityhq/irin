import { useState } from "react";
import type { MapmakerResult } from "@/lib/types";

/** Topic/context/map inputs (the "matter" being filed) plus submit state. */
export function useConveneMatter() {
  const [topic, setTopic] = useState("");
  const [context, setContext] = useState("");
  const [mapDir, setMapDir] = useState("");
  const [mapBrief, setMapBrief] = useState<{ text: string; result: MapmakerResult } | null>(null);
  const [submitting, setSubmitting] = useState(false);

  return {
    topic,
    setTopic,
    context,
    setContext,
    mapDir,
    setMapDir,
    mapBrief,
    setMapBrief,
    submitting,
    setSubmitting,
  };
}

export type ConveneMatter = ReturnType<typeof useConveneMatter>;
