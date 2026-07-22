"use client";

import { ChangeEvent, useEffect, useMemo, useRef, useState } from "react";
import { FileUp, Loader2, Play, UploadCloud, Users } from "lucide-react";
import { api } from "@/lib/api";
import {
  cabinetToYaml,
  isValidCabinetName,
  lintCabinetYaml,
  suggestCabinetKey,
  validateCabinetForSave,
  type CabinetYamlLint,
} from "@/lib/cabinet-save";
import { cn } from "@/lib/cn";
import {
  buildProviderChoices,
  getModelsForProvider,
  getProviderOption,
  unavailableProviderReason,
  useDiscover,
} from "@/lib/use-discover";
import type { Cabinet, DiscoverProvider } from "@/lib/types";
import { useToast } from "./Toast";

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
  const { toast } = useToast();
  const [selected, setSelected] = useState(cabinets[0]?.name ?? "");
  const cab = useMemo(
    () => cabinets.find((c) => c.name === selected),
    [cabinets, selected],
  );
  const [draft, setDraft] = useState<Cabinet | null>(null);

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

  // feature contract import state.
  const fileRef = useRef<HTMLInputElement>(null);
  const [imported, setImported] = useState<
    { fileName: string; yaml: string; lint: CabinetYamlLint } | null
  >(null);
  const [importName, setImportName] = useState("");
  const [importSaving, setImportSaving] = useState(false);
  const [importError, setImportError] = useState<string | null>(null);
  const [importSavedName, setImportSavedName] = useState<string | null>(null);

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
  const saveNameValid = isValidCabinetName(saveName);
  const activeCabinet = draft ?? cab;
  const providerAvailabilityProblem = activeCabinet
    ? discoverError
      ? `Provider discovery failed: ${discoverError}. Rescan before saving or running.`
      : !discoverData || discoverLoading
        ? "Provider availability is still being checked. Wait for discovery before saving or running."
        : unavailableProviderReason(providerOptions, [
            ...activeCabinet.seats.map((seat) => seat.provider),
            activeCabinet.chair.provider,
          ])
    : null;

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

  const handleFile = async (e: ChangeEvent<HTMLInputElement>) => {
    const f = e.target.files?.[0];
    if (!f) return;
    const text = await f.text();
    setImported({ fileName: f.name, yaml: text, lint: lintCabinetYaml(text) });
    setImportName(suggestCabinetKey(f.name.replace(/\.(yaml|yml)$/i, "")));
    setImportError(null);
    setImportSavedName(null);
    if (fileRef.current) fileRef.current.value = "";
  };

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

  const saveImported = async () => {
    if (!imported) return;
    setImportError(null);
    if (!isValidCabinetName(importName)) {
      setImportError(
        "Name must match ^[a-z0-9][a-z0-9_-]{0,63}$ (lowercase, no slashes or dots)",
      );
      return;
    }
    setImportSaving(true);
    try {
      // Raw text on purpose — the server's serde_yaml parse is authoritative.
      const res = await api.saveCabinet(importName, imported.yaml);
      setImportSavedName(res.name);
      toast("success", `Cabinet imported → ${res.name}`);
      onRefresh?.();
    } catch (e) {
      setImportError(e instanceof Error ? e.message : String(e));
    } finally {
      setImportSaving(false);
    }
  };

  return (
    <div className="grid grid-cols-12 gap-5">
      <aside className="col-span-12 lg:col-span-3 flex flex-col self-start rounded border border-border bg-bg-deep/60 overflow-hidden">
        <div className="px-3 pt-3">
          <p className="cg-section-label">
            <Users className="w-3.5 h-3.5 text-amber" />
            Cabinets
          </p>
        </div>
        <div className="px-1.5 pb-2 max-h-[52vh] overflow-y-auto overscroll-contain">
          {cabinets.map((c) => (
            <button
              key={c.name}
              onClick={() => {
                setSelected(c.name);
                setDraft(null);
                setSaveNameOverride(null);
                setSaveError(null);
                setSavedPath(null);
                setSavedName(null);
              }}
              className={cn("cg-session-row", selected === c.name && "selected")}
            >
              <div className="min-w-0">
                <div
                  className={cn(
                    "text-[11px] font-mono font-semibold leading-snug truncate",
                    selected === c.name ? "text-amber" : "text-fg",
                  )}
                >
                  {c.label}
                </div>
                <div className="text-[10px] font-mono text-fg-dim mt-0.5">
                  {c.seats.length} seats · {c.rounds} rounds
                </div>
              </div>
            </button>
          ))}
        </div>
        <div className="mt-auto border-t border-border p-3">
          <button
            data-testid="cabinet-import-button"
            onClick={() => fileRef.current?.click()}
            className="btn btn-cyan text-xs w-full justify-center"
          >
            <FileUp className="w-3.5 h-3.5" />
            Load YAML
          </button>
          <input
            ref={fileRef}
            data-testid="cabinet-import-input"
            type="file"
            accept=".yaml,.yml"
            onChange={handleFile}
            className="hidden"
          />
          <p className="text-[10px] font-mono text-fg-dim mt-2 leading-relaxed">
            Import an external cabinet file, name it, and save it into the
            council registry.
          </p>
        </div>
      </aside>

      <section className="col-span-12 lg:col-span-9 space-y-4">
        {imported && (
          <div
            data-testid="cabinet-import-panel"
            className="cg-command-panel space-y-3"
          >
            <div className="flex items-center gap-2">
              <FileUp className="w-3.5 h-3.5 text-fg-muted shrink-0" />
              <span className="cg-section-label mb-0">Imported YAML</span>
              <span className="chip text-[9px] normal-case tracking-normal font-medium">
                {imported.fileName}
              </span>
              <button
                onClick={() => setImported(null)}
                className="text-[10px] font-mono uppercase tracking-widest text-fg-dim hover:text-danger ml-auto transition-colors"
              >
                Discard
              </button>
            </div>
            {!imported.lint.ok && (
              <div
                data-testid="cabinet-import-lint"
                className="rounded border border-warning/40 bg-warning/[0.06] px-3 py-2 text-[11px] font-mono text-warning leading-relaxed"
              >
                Missing top-level key
                {imported.lint.missing.length === 1 ? "" : "s"}:{" "}
                {imported.lint.missing.join(", ")} — the server will reject
                this unless they are present.
              </div>
            )}
            <pre className="rounded border border-border bg-bg-deep p-3 text-[10px] font-mono max-h-40 overflow-y-auto text-fg-muted whitespace-pre-wrap leading-relaxed">
              {imported.yaml.length > 4000
                ? `${imported.yaml.slice(0, 4000)}…`
                : imported.yaml}
            </pre>
            <div className="flex items-end gap-2 flex-wrap">
              <div className="flex-1 min-w-[200px]">
                <span className="label">Registry name</span>
                <input
                  data-testid="cabinet-import-name"
                  className="input mt-1.5 w-full text-xs"
                  value={importName}
                  onChange={(e) => setImportName(e.target.value)}
                  placeholder="my-cabinet"
                />
              </div>
              <button
                data-testid="cabinet-import-save"
                onClick={() => void saveImported()}
                disabled={importSaving}
                className="btn btn-primary text-xs"
              >
                {importSaving ? (
                  <Loader2 className="w-3.5 h-3.5 animate-spin" />
                ) : (
                  <UploadCloud className="w-3.5 h-3.5" />
                )}
                Save to council
              </button>
              {importSavedName && onRun && (
                <button
                  data-testid="cabinet-import-run"
                  onClick={() => onRun(importSavedName)}
                  className="btn btn-cyan text-xs"
                >
                  <Play className="w-3.5 h-3.5" />
                  Run deliberation
                </button>
              )}
            </div>
            {importError && (
              <div
                data-testid="cabinet-import-error"
                className="text-[11px] font-mono text-danger"
              >
                {importError}
              </div>
            )}
            {importSavedName && (
              <div className="text-[11px] font-mono text-success">
                Saved as {importSavedName} — available to the CLI via --cabinet
                and listed under Cabinets.
              </div>
            )}
          </div>
        )}

        {cab && (
          <>
            <div className="cg-command-panel">
              <div className="text-[10px] font-mono uppercase tracking-widest text-fg-dim mb-1.5">
                {cab.name}
              </div>
              <div className="font-display font-semibold text-xl text-fg-bright leading-snug">
                {cab.label}
              </div>
              <div className="text-sm text-fg-muted mt-1 leading-relaxed">
                {cab.description}
              </div>
            </div>

            <div className="cg-command-panel space-y-3">
              <div className="flex items-center gap-2 flex-wrap">
                <span className="cg-section-label mb-0">Seats</span>
                {dirty && (
                  <span className="chip chip-amber text-[9px] normal-case tracking-normal">
                    Unsaved changes
                  </span>
                )}
                <div className="ml-auto flex items-center gap-2">
                  <button
                    type="button"
                    className="btn text-xs"
                    onClick={() => {
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
                    }}
                  >
                    Reset models to discovered defaults
                  </button>
                  <button
                    type="button"
                    className="btn text-xs"
                    onClick={() => void rescan()}
                    disabled={discoverLoading}
                  >
                    Rescan providers
                  </button>
                </div>
              </div>
              <div className="divide-y divide-border">
                {(draft ?? cab).seats.map((s, i) => {
                  const provModels = getModelsForProvider(providerModelMap, s.provider);
                  return (
                  <div key={i} className="py-3 first:pt-0 last:pb-0 space-y-2">
                    <div className="text-[10px] font-mono uppercase tracking-widest text-fg-dim">
                      Seat {i + 1}
                    </div>
                    <div className="grid grid-cols-3 gap-2">
                      <input className="input text-xs" value={s.name}
                        onChange={(e) => updateSeat(i, "name", e.target.value)} />
                      <select className="input text-xs" value={s.provider}
                        onChange={(e) => {
                          const newProv = e.target.value;
                          updateSeat(i, "provider", newProv);
                          // auto pick first model for new provider if available
                          const newModels = getModelsForProvider(providerModelMap, newProv);
                          if (newModels.length > 0 && !newModels.includes(s.model)) {
                            updateSeat(i, "model", newModels[0]);
                          }
                        }}>
                        {providerSelectOptions(providerOptions, s.provider)}
                      </select>
                      {provModels.length > 0 ? (
                        <select className="input text-xs" value={s.model}
                          onChange={(e) => updateSeat(i, "model", e.target.value)}>
                          {provModels.map(m => (
                            <option key={m} value={m}>{m}</option>
                          ))}
                          {!provModels.includes(s.model) && s.model && (
                            <option value={s.model}>{s.model} (custom)</option>
                          )}
                        </select>
                      ) : (
                        <input className="input text-xs" value={s.model}
                          onChange={(e) => updateSeat(i, "model", e.target.value)} />
                      )}
                    </div>
                    {!getProviderOption(providerOptions, s.provider)?.available && discoverData && (
                      <div className="text-[10px] font-mono text-danger">
                        {s.provider} is unavailable or is a legacy provider ID. Choose an available transport.
                      </div>
                    )}
                    <textarea className="input text-xs" rows={4} value={s.system}
                      onChange={(e) => updateSeat(i, "system", e.target.value)} />
                    {s.system_source && (
                      <details className="text-xs">
                        <summary className="cursor-pointer text-[10px] font-mono uppercase tracking-widest text-fg-dim hover:text-fg transition-colors">
                          Preview prompt source
                        </summary>
                        <pre className="rounded border border-border bg-bg-deep p-2 mt-1.5 overflow-auto max-h-40 text-[10px] font-mono text-fg-muted whitespace-pre-wrap leading-relaxed">{s.system_source}</pre>
                      </details>
                    )}
                  </div>
                  );
                })}
              </div>
            </div>

            <div className="cg-command-panel space-y-3">
              <span className="cg-section-label mb-0">Chair</span>
              <div className="grid grid-cols-2 gap-2">
                <select
                  className="input text-xs"
                  value={ (draft ?? cab)?.chair?.provider || "" }
                  onChange={(e) => {
                    const newProv = e.target.value;
                    updateChair("provider", newProv);
                    const newModels = getModelsForProvider(providerModelMap, newProv);
                    const chairModel = (draft ?? cab)?.chair?.model || "";
                    if (newModels.length > 0 && !newModels.includes(chairModel)) {
                      updateChair("model", newModels[0]);
                    }
                  }}
                >
                  <option value="">-- provider --</option>
                  {providerSelectOptions(
                    providerOptions,
                    (draft ?? cab)?.chair?.provider || "",
                  )}
                </select>
                {(() => {
                  const chairProv = (draft ?? cab)?.chair?.provider || "";
                  const chairModels = getModelsForProvider(providerModelMap, chairProv);
                  return chairModels.length > 0 ? (
                    <select
                      className="input text-xs"
                      value={ (draft ?? cab)?.chair?.model || "" }
                      onChange={(e) => updateChair("model", e.target.value)}
                    >
                      {chairModels.map((m) => (
                        <option key={m} value={m}>{m}</option>
                      ))}
                    </select>
                  ) : (
                    <input
                      className="input text-xs"
                      value={ (draft ?? cab)?.chair?.model || "" }
                      onChange={(e) => updateChair("model", e.target.value)}
                    />
                  );
                })()}
              </div>
              {discoverData && !getProviderOption(
                providerOptions,
                (draft ?? cab)?.chair?.provider || "",
              )?.available && (
                <div className="text-[10px] font-mono text-danger">
                  {(draft ?? cab)?.chair?.provider} is unavailable or is a legacy provider ID. Choose an available chair transport.
                </div>
              )}
            </div>

            <div className="cg-command-panel space-y-3">
              <span className="cg-section-label mb-0">Save to council</span>
              <p className="text-[10px] font-mono text-fg-dim leading-relaxed">
                Writes cabinets/&lt;name&gt;.yaml on the server (built-in names
                are protected). The saved cabinet appears in the list without a
                restart and works from the CLI via --cabinet.
              </p>
              <div className="flex items-end gap-2 flex-wrap">
                <div className="flex-1 min-w-[200px]">
                  <span className="label">Registry name</span>
                  <input
                    data-testid="cabinet-save-name"
                    className={cn(
                      "input mt-1.5 w-full text-xs",
                      saveName && !saveNameValid && "border-danger/60",
                    )}
                    value={saveName}
                    onChange={(e) => setSaveNameOverride(e.target.value)}
                    placeholder="my-cabinet"
                  />
                </div>
                <button
                  data-testid="cabinet-save-submit"
                  onClick={() => void saveToCouncil()}
                  disabled={saving || !!providerAvailabilityProblem}
                  title={providerAvailabilityProblem ?? undefined}
                  className="btn btn-primary text-xs"
                >
                  {saving ? (
                    <Loader2 className="w-3.5 h-3.5 animate-spin" />
                  ) : (
                    <UploadCloud className="w-3.5 h-3.5" />
                  )}
                  Save to council
                </button>
                {onRun && (
                  <button
                    data-testid="cabinet-run"
                    onClick={() => onRun(savedName ?? cab.name)}
                    disabled={!!dirty || !!providerAvailabilityProblem}
                    title={
                      providerAvailabilityProblem
                        ? providerAvailabilityProblem
                        : dirty
                        ? "Save to council first — Run launches the registry version"
                        : "Pre-select this cabinet on the Deliberate panel"
                    }
                    className="btn btn-cyan text-xs"
                  >
                    <Play className="w-3.5 h-3.5" />
                    Run deliberation
                  </button>
                )}
              </div>
              {providerAvailabilityProblem && (
                <div
                  data-testid="cabinet-provider-warning"
                  className="text-[11px] font-mono text-danger"
                >
                  {providerAvailabilityProblem}
                </div>
              )}
              {saveError && (
                <div
                  data-testid="cabinet-save-error"
                  className="text-[11px] font-mono text-danger"
                >
                  {saveError}
                </div>
              )}
              {savedPath && (
                <div
                  data-testid="cabinet-save-success"
                  className="text-[11px] font-mono text-success"
                >
                  Saved → {savedPath}
                </div>
              )}
            </div>
          </>
        )}
      </section>
    </div>
  );

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
}

function providerSelectOptions(
  providers: DiscoverProvider[],
  currentProvider: string,
) {
  return (
    <>
      {buildProviderChoices(providers, currentProvider).map((provider) => (
        <option
          key={provider.name}
          value={provider.name}
          disabled={!provider.available}
        >
          {provider.label}
        </option>
      ))}
    </>
  );
}
