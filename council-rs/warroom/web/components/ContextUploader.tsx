"use client";

import { ChangeEvent, useRef } from "react";
import { FileText, Upload, X } from "lucide-react";

export default function ContextUploader({
  value,
  onChange,
}: {
  value: string;
  onChange: (v: string) => void;
}) {
  const inputRef = useRef<HTMLInputElement>(null);

  const handleFile = async (e: ChangeEvent<HTMLInputElement>) => {
    const f = e.target.files?.[0];
    if (!f) return;
    const text = await f.text();
    onChange(value ? value + "\n\n---\n\n" + text : text);
    if (inputRef.current) inputRef.current.value = "";
  };

  return (
    <div className="panel p-5 space-y-3">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <FileText className="w-4 h-4 text-cyan" />
          <span className="label">Context (--context)</span>
        </div>
        <div className="flex items-center gap-2">
          {value && (
            <button
              onClick={() => onChange("")}
              className="text-xs text-fg-dim hover:text-danger flex items-center gap-1"
            >
              <X className="w-3 h-3" /> clear
            </button>
          )}
          <button
            onClick={() => inputRef.current?.click()}
            className="btn btn-cyan text-xs"
          >
            <Upload className="w-3.5 h-3.5" />
            Upload file
          </button>
          <input
            ref={inputRef}
            type="file"
            accept=".md,.txt,.json,.yaml,.yml,.csv"
            onChange={handleFile}
            className="hidden"
          />
        </div>
      </div>
      <textarea
        value={value}
        onChange={(e) => onChange(e.target.value)}
        rows={5}
        placeholder="Paste research, intel, prior session JSON, anything…"
        className="input text-xs"
      />
      {value && (
        <div className="text-[10px] font-mono text-fg-dim">
          {value.length.toLocaleString()} chars
        </div>
      )}
    </div>
  );
}
