"use client";

import { FileUp, Loader2, Play, UploadCloud } from "lucide-react";
import type { ImportedCabinet } from "./useCabinetImport";

export function ImportPanel({
  imported,
  onDiscard,
  importName,
  setImportName,
  importSaving,
  saveImported,
  importSavedName,
  onRun,
  importError,
}: {
  imported: ImportedCabinet;
  onDiscard: () => void;
  importName: string;
  setImportName: (name: string) => void;
  importSaving: boolean;
  saveImported: () => Promise<void>;
  importSavedName: string | null;
  onRun?: (cabinetKey: string) => void;
  importError: string | null;
}) {
  return (
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
          onClick={onDiscard}
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
  );
}
