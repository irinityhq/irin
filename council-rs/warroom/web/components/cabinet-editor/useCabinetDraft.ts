"use client";

import { useState } from "react";
import type { Cabinet } from "@/lib/types";

/**
 * Editable draft of the selected cabinet. `draft` is null until the operator
 * touches a field; updateSeat/updateChair clone the registry cabinet on first
 * edit so the original list entry is never mutated.
 */
export function useCabinetDraft(cab: Cabinet | undefined) {
  const [draft, setDraft] = useState<Cabinet | null>(null);

  function updateSeat(i: number, field: string, value: string) {
    if (!cab) return;
    setDraft((previous) => {
      const base = previous ?? structuredClone(cab);
      base.seats[i] = { ...base.seats[i], [field]: value };
      return { ...base };
    });
  }

  function updateChair(field: string, value: string) {
    if (!cab) return;
    setDraft((previous) => {
      const base = previous ?? structuredClone(cab);
      base.chair = { ...base.chair, [field]: value };
      return { ...base };
    });
  }

  return { draft, setDraft, updateSeat, updateChair };
}
