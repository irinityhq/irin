"use client";

import { useEffect, useRef, useState } from "react";
import { Trash2, Plus, Pencil } from "lucide-react";
import type { LibrarianChatSummary, LibrarianHealth } from "@/lib/librarian";
import { cn } from "@/lib/cn";

export default function LibrarianSidebar({
  chats, activeId, onSelect, onNew, onDelete, onRename, health, renameDisabled,
}: {
  chats: LibrarianChatSummary[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
  onDelete: (id: string) => void;
  onRename: (id: string, title: string) => Promise<void>;
  health: LibrarianHealth | null;
  renameDisabled?: boolean;
}) {
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draftTitle, setDraftTitle] = useState("");
  const [saving, setSaving] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const savedViaEnterRef = useRef(false);

  useEffect(() => {
    if (editingId) inputRef.current?.focus();
  }, [editingId]);

  function startEdit(c: LibrarianChatSummary) {
    if (renameDisabled) return;
    setEditingId(c.id);
    setDraftTitle(c.title || "");
  }

  function cancelEdit() {
    setEditingId(null);
    setDraftTitle("");
  }

  async function saveEdit(c: LibrarianChatSummary) {
    const title = draftTitle.trim();
    if (!title || title === (c.title || "")) {
      cancelEdit();
      return;
    }
    setSaving(true);
    try {
      await onRename(c.id, title);
      cancelEdit();
    } catch {
      // parent surfaces error / toast
    } finally {
      setSaving(false);
      savedViaEnterRef.current = false;
    }
  }

  return (
    <aside className="w-64 shrink-0 border-r border-border bg-bg-elevated flex flex-col min-h-0">
      <div className="shrink-0 p-3 border-b border-border">
        <button onClick={onNew} className="w-full btn btn-primary">
          <Plus className="w-4 h-4" />
          New conversation
        </button>
      </div>
      <div className="shrink-0 px-3 pt-2.5 pb-1.5">
        <p className="cg-section-label mb-0">Conversations</p>
      </div>
      <ul className="flex-1 min-h-0 overflow-y-auto overscroll-contain px-1.5 pb-2">
        {chats.length === 0 && (
          <li className="px-2 py-4 text-[10px] font-mono text-fg-dim leading-relaxed">
            No conversations yet. Start one to query the librarian.
          </li>
        )}
        {chats.map((c) => (
          <li
            key={c.id}
            className={cn(
              "group grid grid-cols-[1fr_auto] gap-2 items-start px-2 py-2 mb-0.5 rounded cursor-pointer",
              "border border-transparent border-l-2 border-l-transparent transition-colors",
              "hover:bg-bg-overlay hover:border-border",
              c.id === activeId && "bg-amber/[0.05] border-border border-l-amber",
            )}
            onClick={() => {
              if (editingId !== c.id) onSelect(c.id);
            }}
          >
            <div className="min-w-0">
              {editingId === c.id ? (
                <input
                  ref={inputRef}
                  value={draftTitle}
                  onChange={(e) => setDraftTitle(e.target.value)}
                  onClick={(e) => e.stopPropagation()}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      savedViaEnterRef.current = true;
                      void saveEdit(c);
                    }
                    if (e.key === "Escape") cancelEdit();
                  }}
                  onBlur={() => {
                    if (savedViaEnterRef.current) {
                      savedViaEnterRef.current = false;
                      return;
                    }
                    if (!saving) void saveEdit(c);
                  }}
                  disabled={saving}
                  className="input w-full text-[11px] py-0.5"
                  aria-label="Rename conversation"
                />
              ) : (
                <div
                  className={cn(
                    "text-[11px] leading-snug truncate",
                    c.id === activeId ? "text-fg font-medium" : "text-fg-muted",
                    renameDisabled && "opacity-60",
                  )}
                  onDoubleClick={(e) => {
                    e.stopPropagation();
                    startEdit(c);
                  }}
                  title={renameDisabled
                    ? "Wait for ask to finish"
                    : "Double-click to rename"}
                >
                  {c.title || "Untitled conversation"}
                </div>
              )}
              <div className="text-[10px] font-mono text-fg-dim mt-0.5 tabular-nums">
                {c.ask_count} ask{c.ask_count === 1 ? "" : "s"}
              </div>
            </div>
            <div className="flex items-center gap-1 pt-0.5">
              {editingId !== c.id && !renameDisabled && (
                <button
                  onClick={(e) => { e.stopPropagation(); startEdit(c); }}
                  className="opacity-0 group-hover:opacity-60 hover:opacity-100 text-fg-muted transition-opacity"
                  aria-label="Rename conversation"
                >
                  <Pencil className="w-3.5 h-3.5" />
                </button>
              )}
              <button
                onClick={(e) => { e.stopPropagation(); onDelete(c.id); }}
                className="opacity-0 group-hover:opacity-60 hover:opacity-100 text-fg-muted hover:text-danger transition-opacity"
                aria-label="Delete conversation"
              >
                <Trash2 className="w-3.5 h-3.5" />
              </button>
            </div>
          </li>
        ))}
      </ul>
      <HealthPill health={health} />
    </aside>
  );
}

function HealthPill({ health }: { health: LibrarianHealth | null }) {
  const state = health?.state ?? "unknown";
  const dot = state === "online" ? "bg-success"
    : state === "warming" ? "bg-amber"
    : state === "offline" ? "bg-danger"
    : "bg-fg-dim";
  return (
    <div className="border-t border-border p-3 flex items-center gap-2 text-xs font-mono">
      <span className={cn("w-2 h-2 rounded-full", dot)} />
      <span className="text-fg-muted">librarian {state}</span>
    </div>
  );
}