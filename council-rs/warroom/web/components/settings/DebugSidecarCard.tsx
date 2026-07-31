import { Shield, Trash2 } from "lucide-react";
import {
  clearServerLogs,
  startCouncilServer,
  stopCouncilServer,
} from "@/lib/tauri";
import type { RuntimeConfig } from "@/lib/runtime-config";
import type { ToastType } from "@/components/Toast";

export function DebugSidecarCard({
  form,
  notify,
  showServerLog,
  onShowServerLogChange,
  serverLogs,
  onServerLogsChange,
  refreshLogs,
}: {
  form: RuntimeConfig;
  notify: (type: ToastType, message: string) => void;
  showServerLog: boolean;
  onShowServerLogChange: (show: boolean) => void;
  serverLogs: string[];
  onServerLogsChange: (logs: string[]) => void;
  refreshLogs: () => Promise<void>;
}) {
  return (
    <div className="border border-border bg-bg-elevated p-5 space-y-4">
      <div className="flex items-center gap-2 border-b border-border pb-3">
        <Shield className="w-3.5 h-3.5 text-fg-dim" />
        <span className="label text-fg">Council connection / debug sidecar</span>
      </div>
      <div className="flex flex-wrap gap-2">
        <button
          type="button"
          className="btn btn-primary text-xs"
          onClick={async () => {
            try {
              const msg = await startCouncilServer(
                undefined,
                form.authToken,
                form.librarianBase || undefined,
              );
              notify("success", msg);
              if (showServerLog) void refreshLogs();
            } catch (e) {
              notify("error", e instanceof Error ? e.message : String(e));
            }
          }}
        >
          Connect / start debug server
        </button>
        <button
          type="button"
          className="btn btn-danger text-xs"
          onClick={async () => {
            try {
              const msg = await stopCouncilServer();
              notify("success", msg);
            } catch (e) {
              notify("error", e instanceof Error ? e.message : String(e));
            }
          }}
        >
          Stop debug server
        </button>
      </div>
      <label className="flex items-center gap-2 text-xs font-mono cursor-pointer">
        <input
          type="checkbox"
          checked={showServerLog}
          onChange={(e) => onShowServerLogChange(e.target.checked)}
          className="rounded border-border"
        />
        Show debug backend log panel
      </label>
      {showServerLog && (
        <div className="relative">
          <pre className="border border-border bg-bg-deep p-3 text-[10px] font-mono max-h-64 overflow-y-auto text-fg-muted whitespace-pre-wrap">
            {serverLogs.length ? serverLogs.join("\n") : "(no logs yet)"}
          </pre>
          <button
            type="button"
            className="btn text-xs absolute top-2 right-2"
            onClick={async () => {
              await clearServerLogs();
              onServerLogsChange([]);
            }}
          >
            <Trash2 className="w-3 h-3" />
            Clear
          </button>
        </div>
      )}
    </div>
  );
}
