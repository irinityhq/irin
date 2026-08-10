"use client";

import { useEffect, useMemo, useState } from "react";
import {
  getModelsForProvider,
  unavailableProviderReason,
  useDiscover,
} from "@/lib/use-discover";
import type { Cabinet } from "@/lib/types";
import { CabinetHeader } from "./cabinet-editor/CabinetHeader";
import { CabinetListSidebar } from "./cabinet-editor/CabinetListSidebar";
import { ChairPanel } from "./cabinet-editor/ChairPanel";
import { ImportPanel } from "./cabinet-editor/ImportPanel";
import { SavePanel } from "./cabinet-editor/SavePanel";
import { SeatsPanel } from "./cabinet-editor/SeatsPanel";
import { useCabinetDraft } from "./cabinet-editor/useCabinetDraft";
import { useCabinetImport } from "./cabinet-editor/useCabinetImport";
import { useCabinetSave } from "./cabinet-editor/useCabinetSave";

/**
 * Cabinet viewer + draft editor.
 *
 * feature contract: "Save to council" POSTs {name, yaml} to /api/cabinets/save — the
 * server validates the YAML as a Rust Cabinet, refuses built-in keys, and
 * writes <base_dir>/cabinets/<name>.yaml (reusable from the CLI via
 * `--cabinet`). "Run deliberation" hands the registry key back to WarRoom,
 * which pre-selects it on the Deliberate panel (topic is entered there).
 *
 * feature contract: "Load YAML" reads a cabinet file client-side (plain <input
 * type=file>, works in browser + Tauri webview), lints the required top-level
 * keys, and POSTs the RAW text to the same save endpoint under a user-chosen
 * name — serde_yaml on the server is the real validator; launching then goes
 * through the saved registry name (no custom_cabinet payload needed for the
 * import flow).
 */
export default function CabinetEditor({
  cabinets,
  onRefresh,
  onRun,
}: {
  cabinets: Cabinet[];
  /** Re-fetch the cabinet list after a successful save (no restart needed). */
  onRefresh?: () => void;
  /** Navigate to Deliberate with this registry key pre-selected. */
  onRun?: (cabinetKey: string) => void;
}) {
  const [selected, setSelected] = useState(cabinets[0]?.name ?? "");
  const cab = useMemo(
    () => cabinets.find((c) => c.name === selected),
    [cabinets, selected],
  );
  const { draft, setDraft, updateSeat, updateChair } = useCabinetDraft(cab);

  // Shared discover hook (deduped fetches, one source of truth)
  const {
    data: discoverData,
    loading: discoverLoading,
    error: discoverError,
    rescan,
    providerModelMap,
    providerOptions,
  } = useDiscover();

  const dirty = draft && cab && JSON.stringify(draft) !== JSON.stringify(cab);
  const activeCabinet = draft ?? cab;
  const providerAvailabilityProblem = activeCabinet
    ? discoverLoading || !discoverData
      ? discoverLoading || discoverError === null
        ? "Council loading…"
        : `Provider discovery failed: ${discoverError}. Rescan before saving or running.`
      : unavailableProviderReason(providerOptions, [
            ...activeCabinet.seats.map((seat) => seat.provider),
            activeCabinet.chair.provider,
          ])
    : null;

  const {
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
  } = useCabinetSave({ cab, draft, providerAvailabilityProblem, onRefresh, setDraft });

  const {
    fileRef,
    imported,
    setImported,
    importName,
    setImportName,
    importSaving,
    importError,
    importSavedName,
    handleFile,
    saveImported,
  } = useCabinetImport({ onRefresh });

  // Adopt the saved registry key as the selection once the refreshed list
  // actually contains it (saveToCouncil defers this to avoid unmounting the
  // editor while the refetch is in flight).
  useEffect(() => {
    if (
      savedName &&
      selected !== savedName &&
      cabinets.some((c) => c.name === savedName)
    ) {
      setSelected(savedName);
    }
  }, [savedName, selected, cabinets]);

  const selectCabinet = (name: string) => {
    setSelected(name);
    setDraft(null);
    setSaveNameOverride(null);
    setSaveError(null);
    setSavedPath(null);
    setSavedName(null);
  };

  const resetModelsToDefaults = () => {
    if (!cab) return;
    const base = draft ?? structuredClone(cab);
    let changed = false;
    base.seats.forEach((s, i) => {
      const mods = getModelsForProvider(providerModelMap, s.provider);
      if (mods.length > 0 && s.model !== mods[0]) {
        base.seats[i] = { ...s, model: mods[0] };
        changed = true;
      }
    });
    if (changed) setDraft({ ...base });
  };

  return (
    <div className="grid grid-cols-12 gap-5">
      <CabinetListSidebar
        cabinets={cabinets}
        selected={selected}
        onSelect={selectCabinet}
        fileRef={fileRef}
        onFile={handleFile}
      />

      <section className="col-span-12 lg:col-span-9 space-y-4">
        {imported && (
          <ImportPanel
            imported={imported}
            onDiscard={() => setImported(null)}
            importName={importName}
            setImportName={setImportName}
            importSaving={importSaving}
            saveImported={saveImported}
            importSavedName={importSavedName}
            onRun={onRun}
            importError={importError}
          />
        )}

        {cab && (
          <>
            <CabinetHeader cab={cab} />

            <SeatsPanel
              dirty={!!dirty}
              onResetModels={resetModelsToDefaults}
              rescan={rescan}
              discoverLoading={discoverLoading}
              seats={(draft ?? cab).seats}
              providerModelMap={providerModelMap}
              providerOptions={providerOptions}
              updateSeat={updateSeat}
            />

            <ChairPanel
              chair={(draft ?? cab).chair}
              providerModelMap={providerModelMap}
              providerOptions={providerOptions}
              updateChair={updateChair}
            />

            <SavePanel
              saveName={saveName}
              saveNameValid={saveNameValid}
              setSaveNameOverride={setSaveNameOverride}
              saveToCouncil={saveToCouncil}
              saving={saving}
              providerAvailabilityProblem={providerAvailabilityProblem}
              onRun={onRun}
              runKey={savedName ?? cab.name}
              dirty={!!dirty}
              saveError={saveError}
              savedPath={savedPath}
            />
          </>
        )}
      </section>
    </div>
  );
}
