"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  DEFAULT_CABINET_NAME,
  resolveUntouchedCabinetSelection,
} from "@/lib/cabinet-selection";
import type { Cabinet } from "@/lib/types";

export function useCabinetSelection({
  initialCabinet,
  onConsumeInitialCabinet,
  cabinets,
  availableIds,
}: {
  initialCabinet?: string | null;
  onConsumeInitialCabinet?: () => void;
  cabinets: Cabinet[];
  availableIds: string[] | null;
}) {
  const [cabinetName, setCabinetName] = useState(
    initialCabinet || DEFAULT_CABINET_NAME,
  );
  // Explicit editor handoff or any operator click locks selection permanently
  // for this idle mount — inventory flaps must not re-auto-switch.
  const selectionLocked = useRef(!!initialCabinet);
  const autoSelectDone = useRef(false);
  const consumedInitialCabinet = useRef(false);
  useEffect(() => {
    if (!consumedInitialCabinet.current && initialCabinet) {
      consumedInitialCabinet.current = true;
      selectionLocked.current = true;
      onConsumeInitialCabinet?.();
    }
  }, [initialCabinet, onConsumeInitialCabinet]);

  const selectCabinet = useCallback((name: string) => {
    selectionLocked.current = true;
    autoSelectDone.current = true;
    setCabinetName(name);
  }, []);

  // Untouched first load: once cabinets AND the Discover inventory are known,
  // prefer a stable runnable cabinet over a blocked default (see
  // cabinet-selection.ts).
  useEffect(() => {
    if (autoSelectDone.current || selectionLocked.current) return;
    // Wait for both inputs — deciding on an empty list would lock on the
    // preferred default and never re-evaluate when cabinets arrive.
    if (availableIds == null || cabinets.length === 0) return;
    const next = resolveUntouchedCabinetSelection({
      cabinets,
      providersAvailable: availableIds,
      currentName: cabinetName,
      selectionLocked: selectionLocked.current,
    });
    autoSelectDone.current = true;
    if (next && next !== cabinetName) {
      setCabinetName(next);
    }
    // Lock after the first decision so later inventory changes never re-pick.
    selectionLocked.current = true;
  }, [cabinets, availableIds, cabinetName]);

  return { cabinetName, selectCabinet };
}
