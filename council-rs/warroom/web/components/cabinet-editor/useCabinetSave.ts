"use client";

import { useState, type Dispatch, type SetStateAction } from "react";
import { api } from "@/lib/api";
import {
  cabinetToYaml,
  isValidCabinetName,
  suggestCabinetKey,
  validateCabinetForSave,
} from "@/lib/cabinet-save";
import type { Cabinet } from "@/lib/types";
import { useToast } from "../Toast";

/**
 * "Save to council" state for the selected cabinet: the overridable registry
 * name, validation, the POST to /api/cabinets/save, and the saved-key banner.
 */
export function useCabinetSave({
  cab,
  draft,
  providerAvailabilityProblem,
  onRefresh,
  setDraft,
}: {
  cab: Cabinet | undefined;
  draft: Cabinet | null;
  providerAvailabilityProblem: string | null;
  onRefresh?: () => void;
  setDraft: Dispatch<SetStateAction<Cabinet | null>>;
}) {
  const { toast } = useToast();

  // feature contract save state. The name defaults to the registry key of the selected
  // cabinet (already regex-valid for everything the server listed) and is
  // user-overridable per selection.
  const [saveNameOverride, setSaveNameOverride] = useState<string | null>(null);
  const saveName =
    saveNameOverride ??
    (cab
      ? isValidCabinetName(cab.name)
        ? cab.name
        : suggestCabinetKey(cab.label)
      : "");
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [savedPath, setSavedPath] = useState<string | null>(null);
  // Registry key the server returned on the last successful save. Run launches
  // THIS (the saved key), not the previously selected cabinet — covers the
  // save-under-a-new-name flow where `selected` still points at the old key.
  const [savedName, setSavedName] = useState<string | null>(null);
  const saveNameValid = isValidCabinetName(saveName);

  const saveToCouncil = async () => {
    if (!cab) return;
    setSaveError(null);
    setSavedPath(null);
    if (!saveNameValid) {
      setSaveError(
        "Name must match ^[a-z0-9][a-z0-9_-]{0,63}$ (lowercase, no slashes or dots)",
      );
      return;
    }
    const source = draft ?? cab;
    if (providerAvailabilityProblem) {
      setSaveError(providerAvailabilityProblem);
      return;
    }
    const invalid = validateCabinetForSave(source);
    if (invalid) {
      setSaveError(invalid);
      return;
    }
    setSaving(true);
    try {
      const res = await api.saveCabinet(saveName, cabinetToYaml(source));
      setSavedPath(res.path);
      setSavedName(res.name);
      toast("success", `Cabinet saved → ${res.name}`);
      // Refresh the list and clear the dirty draft so Run re-enables and
      // launches the just-saved registry version. Selection switches to the
      // saved key only once the refreshed list contains it (effect below) —
      // switching immediately would unmount the editor (cab undefined) and
      // drop the success banner. The override is cleared so the name field
      // re-derives from the selection.
      onRefresh?.();
      setDraft(null);
      setSaveNameOverride(null);
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  return {
    saveName,
    saveNameValid,
    setSaveNameOverride,
    saving,
    saveError,
    savedPath,
    savedName,
    setSaveError,
    setSavedPath,
    setSavedName,
    saveToCouncil,
  };
}
