"use client";

import { useMemo } from "react";
import { AlertTriangle, Loader2, Server } from "lucide-react";
import { isLoopbackUrl, type RuntimeConfig } from "@/lib/runtime-config";
import { Field } from "./Field";
import { StatusLine, type ProbeStatus } from "./StatusLine";

export function ConnectionCard({
  form,
  onUpdate,
  inTauri,
  debugSidecarAvailable,
  saving,
  onSave,
  onTestConnection,
  healthStatus,
  healthDetail,
  wsStatus,
  wsDetail,
  gatewayStatus,
  gatewayDetail,
}: {
  form: RuntimeConfig;
  onUpdate: (key: keyof RuntimeConfig, value: string) => void;
  inTauri: boolean;
  debugSidecarAvailable: boolean;
  saving: boolean;
  onSave: () => void | Promise<void>;
  onTestConnection: () => void | Promise<void>;
  healthStatus: ProbeStatus;
  healthDetail: string | null;
  wsStatus: ProbeStatus;
  wsDetail: string | null;
  gatewayStatus: ProbeStatus;
  gatewayDetail: string | null;
}) {
  const remoteWarnings = useMemo(() => {
    const urls = [
      { label: "API base", value: form.apiBase },
      { label: "WebSocket base", value: form.wsBase },
      { label: "Gateway base", value: form.gatewayBase },
    ];
    return urls.filter((u) => u.value.trim() && !isLoopbackUrl(u.value));
  }, [form.apiBase, form.wsBase, form.gatewayBase]);

  return (
    <div className="border border-border bg-bg-elevated p-5 space-y-4">
      <div className="flex items-center gap-2 border-b border-border pb-3">
        <Server className="w-3.5 h-3.5 text-fg-dim" />
        <span className="label text-fg">Connection</span>
      </div>
      {remoteWarnings.length > 0 && (
        <div className="border border-border border-l-2 border-l-warning bg-bg-deep p-3 text-xs font-mono text-warning flex gap-2">
          <AlertTriangle className="w-4 h-4 shrink-0" />
          <div>
            Non-loopback URLs may send your auth token off-machine. Prefer
            127.0.0.1 / localhost for local Council and Gateway.
            <ul className="mt-1 list-disc pl-4">
              {remoteWarnings.map((w) => (
                <li key={w.label}>{w.label}</li>
              ))}
            </ul>
          </div>
        </div>
      )}
      <Field
        label="API base"
        value={form.apiBase}
        onChange={(v) => onUpdate("apiBase", v)}
        placeholder="http://127.0.0.1:8765"
        disabled={inTauri}
        hint={
          inTauri
            ? "Managed by the desktop app and matched to its exact allowed origin."
            : undefined
        }
      />
      <Field
        label="WebSocket base"
        value={form.wsBase}
        onChange={(v) => onUpdate("wsBase", v)}
        placeholder="ws://127.0.0.1:8765"
        disabled={inTauri}
        hint={
          inTauri
            ? "Managed by the desktop app and matched to its exact allowed origin."
            : undefined
        }
      />
      <Field
        label="Gateway health base (optional)"
        value={form.gatewayBase}
        onChange={(v) => onUpdate("gatewayBase", v)}
        placeholder="http://127.0.0.1:18080"
        hint="Used only by Test connection for a direct Gateway health probe. Watch and Outbox use the authenticated Council API above."
      />
      <Field
        label="Auth token"
        value={form.authToken}
        onChange={(v) => onUpdate("authToken", v)}
        type="password"
        placeholder="Bearer token for council --serve"
        hint={
          inTauri
            ? "Authenticates the app-owned loopback Council when configured. The installed app owns its bundled Council; debug builds may pass the token to their development sidecar."
            : "Must match COUNCIL_AUTH_TOKEN on the backend, or use COUNCIL_DEV_NO_AUTH=1 for local dev."
        }
      />
      {debugSidecarAvailable && <div data-testid="settings-librarian-base">
        <span className="label">Librarian base (RAG service)</span>
        <input
          className="input mt-1.5 w-full font-mono text-xs"
          value={form.librarianBase}
          onChange={(e) => onUpdate("librarianBase", e.target.value)}
          placeholder="http://127.0.0.1:11435"
        />
        <p className="text-[10px] text-fg-dim mt-1">
          Debug desktop sidecar only: passed as LIBRARIAN_BASE_URL on
          start/restart. Test in the Librarian tab health pill.
        </p>
      </div>}
      <div className="flex flex-wrap gap-2 pt-2">
        <button
          type="button"
          data-testid="settings-save"
          onClick={() => void onSave()}
          disabled={saving}
          className="btn btn-primary"
        >
          {saving ? <Loader2 className="w-4 h-4 animate-spin" /> : null}
          Save
        </button>
        <button
          type="button"
          data-testid="settings-test-connection"
          onClick={() => void onTestConnection()}
          className="btn btn-cyan"
        >
          Test connection
        </button>
      </div>
      {(healthStatus !== "idle" || gatewayStatus !== "idle" || wsStatus !== "idle") && (
        <div
          data-testid="settings-health-probes"
          className="space-y-2 text-xs font-mono pt-2 border-t border-border"
        >
          <StatusLine
            label="Council API"
            status={healthStatus}
            detail={healthDetail}
            testId="settings-health-council"
          />
          <StatusLine
            label="WebSocket"
            status={wsStatus}
            detail={wsDetail}
          />
          <StatusLine
            label="Gateway"
            status={gatewayStatus}
            detail={gatewayDetail}
            testId="settings-health-gateway"
          />
        </div>
      )}
    </div>
  );
}
