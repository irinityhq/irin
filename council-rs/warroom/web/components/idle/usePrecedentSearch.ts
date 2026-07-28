"use client";

import { useEffect, useState } from "react";
import { api } from "@/lib/api";
import type { PrecedentMatch } from "@/lib/types";

export function usePrecedentSearch(topic: string, blind: boolean) {
  const [precedent, setPrecedent] = useState<PrecedentMatch[]>([]);
  const [precedentMode, setPrecedentMode] = useState<"semantic" | "keyword">("keyword");

  // Debounced precedent search as topic is typed
  useEffect(() => {
    if (blind || topic.trim().length < 8) {
      setPrecedent([]);
      return;
    }
    const id = setTimeout(() => {
      api
        .precedent(topic, 0.15, 5)
        .then((r) => {
          setPrecedent(r.matches);
          setPrecedentMode(r.mode);
        })
        .catch(() => setPrecedent([]));
    }, 600);
    return () => clearTimeout(id);
  }, [topic, blind]);

  return { precedent, precedentMode };
}
