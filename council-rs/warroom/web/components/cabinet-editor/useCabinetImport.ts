"use client";

import { useRef, useState, type ChangeEvent } from "react";
import { api } from "@/lib/api";
import {
  isValidCabinetName,
  lintCabinetYaml,
  suggestCabinetKey,
  type CabinetYamlLint,
} from "@/lib/cabinet-save";
import { useToast } from "../Toast";

export interface ImportedCabinet {
  fileName: string;
  yaml: string;
  lint: CabinetYamlLint;
}

/**
 * "Load YAML" import flow: read a cabinet file client-side, lint the required
 * top-level keys, and POST the raw text to the save endpoint under a
 * user-chosen name (the server's serde_yaml parse is authoritative).
 */
export function useCabinetImport({ onRefresh }: { onRefresh?: () => void }) {
  const { toast } = useToast();

  // feature contract import state.
  const fileRef = useRef<HTMLInputElement>(null);
  const [imported, setImported] = useState<ImportedCabinet | null>(null);
  const [importName, setImportName] = useState("");
  const [importSaving, setImportSaving] = useState(false);
  const [importError, setImportError] = useState<string | null>(null);
  const [importSavedName, setImportSavedName] = useState<string | null>(null);

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

  return {
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
  };
}
