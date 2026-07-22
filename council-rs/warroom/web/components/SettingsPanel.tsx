"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  FolderOpen,
  Loader2,
  Network,
  Server,
  Shield,
  Trash2,
  XCircle,
} from "lucide-react";
import { api } from "@/lib/api";

import {
  getGatewayBase,
  getRuntimeConfig,
  isLoopbackUrl,
  loadRuntimeConfig,
  saveRuntimeConfig,
  type RuntimeConfig,
} from "@/lib/runtime-config";
import {
  clearServerLogs,
  disableGatewayPack,
  enableGatewayPack,
  getDesktopRuntimeMode,
  getGatewayPackStatus,
  getServerLogs,
  isTauri,
  onCouncilLog,
  pickFile,
  restartSidecar,
  startCouncilServer,
  stopCouncilServer,
  stopGatewayPack,
  uninstallGatewayPack,
  type DesktopRuntimeMode,
  type GatewayPackStatus,
} from "@/lib/tauri";
import { gatewayPackStateLabel } from "@/lib/gateway-pack";
import { probeWsUpgrade } from "@/lib/ws-probe";
import { cn } from "@/lib/cn";
import { useToast } from "./Toast";

function councilAuthHint(status: number): string {
  if (status === 401) {
    return (
      "401 Unauthorized — Settings auth token must match COUNCIL_AUTH_TOKEN on " +
      "council --serve (same value on both sides). WebSocket uses Sec-WebSocket-Protocol " +
      "token.<value>. Release Tauri does not use COUNCIL_DEV_NO_AUTH."
    );
  }
  return `${status} error`;
}

export default function SettingsPanel() {
  const { toast } = useToast();
  const [form, setForm] = useState<RuntimeConfig>(getRuntimeConfig);
  const [healthStatus, setHealthStatus] = useState<
    "idle" | "loading" | "ok" | "fail"
  >("idle");
  const [healthDetail, setHealthDetail] = useState<string | null>(null);
  const [gatewayStatus, setGatewayStatus] = useState<
    "idle" | "loading" | "ok" | "fail" | "skip"
  >("idle");
  const [gatewayDetail, setGatewayDetail] = useState<string | null>(null);
  const [wsStatus, setWsStatus] = useState<
    "idle" | "loading" | "ok" | "fail" | "skip"
  >("idle");
  const [wsDetail, setWsDetail] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [showServerLog, setShowServerLog] = useState(false);
  const [serverLogs, setServerLogs] = useState<string[]>([]);
  const [sidecarViaGateway, setSidecarViaGateway] = useState(false);
  const [restarting, setRestarting] = useState(false);
  const [packStatus, setPackStatus] = useState<GatewayPackStatus | null>(null);
  const [packBusy, setPackBusy] = useState(false);
  const inTauri = isTauri();
  const [desktopRuntimeMode, setDesktopRuntimeMode] = useState<
    DesktopRuntimeMode | "detecting" | "unavailable"
  >(inTauri ? "detecting" : "unavailable");
  const debugSidecarAvailable = desktopRuntimeMode === "development";
  const installedRelease = desktopRuntimeMode === "installed-release";

  const refreshPackStatus = useCallback(async () => {
    if (!inTauri) return;
    try {
      setPackStatus(await getGatewayPackStatus());
    } catch {
      setPackStatus(null);
    }
  }, [inTauri]);

  const remoteWarnings = useMemo(() => {
    const urls = [
      { label: "API base", value: form.apiBase },
      { label: "WebSocket base", value: form.wsBase },
      { label: "Gateway base", value: form.gatewayBase },
    ];
    return urls.filter((u) => u.value.trim() && !isLoopbackUrl(u.value));
  }, [form.apiBase, form.wsBase, form.gatewayBase]);

  const refreshLogs = useCallback(async () => {
    if (!inTauri) return;
    try {
      setServerLogs(await getServerLogs());
    } catch {
      setServerLogs([]);
    }
  }, [inTauri]);

  useEffect(() => {
    void loadRuntimeConfig().then(setForm);
  }, []);

  useEffect(() => {
    if (!inTauri) return;
    let cancelled = false;
    void getDesktopRuntimeMode()
      .then((mode) => {
        if (!cancelled) setDesktopRuntimeMode(mode);
      })
      .catch(() => {
        if (!cancelled) setDesktopRuntimeMode("unavailable");
      });
    return () => {
      cancelled = true;
    };
  }, [inTauri]);

  useEffect(() => {
    if (!inTauri || desktopRuntimeMode !== "installed-release") return;
    void refreshPackStatus();
    const t = window.setInterval(() => {
      void refreshPackStatus();
    }, 8000);
    return () => window.clearInterval(t);
  }, [inTauri, desktopRuntimeMode, refreshPackStatus]);

  useEffect(() => {
    if (!showServerLog || !inTauri) return;
    void refreshLogs();
    let aborted = false;
    let unlisten: (() => void) | undefined;
    void onCouncilLog((line) => {
      setServerLogs((prev) => [...prev.slice(-499), line]);
    }).then((fn) => {
      if (aborted) fn();
      else unlisten = fn;
    });
    return () => {
      aborted = true;
      unlisten?.();
    };
  }, [showServerLog, inTauri, refreshLogs]);

  const update = (key: keyof RuntimeConfig, value: string) => {
    setForm((f) => ({ ...f, [key]: value }));
  };

  const persist = async () => {
    setSaving(true);
    try {
      const saved = await saveRuntimeConfig(form);
      setForm(saved);
      toast("success", "Settings saved");
    } catch (e) {
      toast("error", e instanceof Error ? e.message : "Save failed");
    } finally {
      setSaving(false);
    }
  };

  const testConnection = async () => {
    setHealthStatus("loading");
    setGatewayStatus("loading");
    setHealthDetail(null);
    setGatewayDetail(null);
    setWsStatus("loading");
    setWsDetail(null);
    try {
      const saved = await saveRuntimeConfig(form);
      setForm(saved);
    } catch (e) {
      setHealthStatus("fail");
      setGatewayStatus("fail");
      const msg = e instanceof Error ? e.message : "Save failed";
      setHealthDetail(msg);
      setGatewayDetail(msg);
      toast("error", msg);
      return;
    }
    try {
      const h = await api.health();
      setHealthStatus("ok");
      const missing =
        h.providers_missing?.length > 0
          ? ` · missing: ${h.providers_missing.join(", ")}`
          : "";
      const build = h.build_sha
        ? ` · build ${h.build_sha.slice(0, 12)}${h.build_dirty ? "-dirty" : ""}`
        : " · build unavailable";
      setHealthDetail(
        `council ${h.council_version} · stream ${h.stream_version}` +
          `${build}${missing}`,
      );

      const wsProbe = await probeWsUpgrade();
      if (wsProbe.ok) {
        setWsStatus("ok");
        setWsDetail(wsProbe.detail);
      } else {
        setWsStatus("fail");
        setWsDetail(wsProbe.detail);
      }
    } catch (e) {
      setHealthStatus("fail");
      setWsStatus("fail");
      const msg = e instanceof Error ? e.message : String(e);
      const statusMatch = msg.match(/^(\d{3})\b/);
      const status = statusMatch ? Number(statusMatch[1]) : 0;
      const detail = status === 401 ? councilAuthHint(401) : msg;
      setHealthDetail(detail);
      setWsDetail(
        status === 401
          ? "REST failed — fix token before WebSocket can connect"
          : "Skipped — REST health failed",
      );
    }
    const gw = getGatewayBase().trim();
    if (!gw) {
      setGatewayStatus("skip");
      setGatewayDetail("No gateway URL configured");
      return;
    }
    try {
      const res = await fetch(`${gw.replace(/\/$/, "")}/health`, {
        cache: "no-store",
      });
      if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
      setGatewayStatus("ok");
      setGatewayDetail("Gateway reachable");
    } catch (e) {
      setGatewayStatus("fail");
      setGatewayDetail(e instanceof Error ? e.message : String(e));
    }
  };

  const pickCouncilBinary = async () => {
    const path = await pickFile();
    if (path) update("councilPath", path);
  };

  return (
    <div className="max-w-2xl mx-auto space-y-6">
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
          onChange={(v) => update("apiBase", v)}
          placeholder="http://127.0.0.1:8765"
        />
        <Field
          label="WebSocket base"
          value={form.wsBase}
          onChange={(v) => update("wsBase", v)}
          placeholder="ws://127.0.0.1:8765"
        />
        <Field
          label="Gateway health base (optional)"
          value={form.gatewayBase}
          onChange={(v) => update("gatewayBase", v)}
          placeholder="http://127.0.0.1:18080"
          hint="Used only by Test connection for a direct Gateway health probe. Watch and Outbox use the authenticated Council API above."
        />
        <Field
          label="Auth token"
          value={form.authToken}
          onChange={(v) => update("authToken", v)}
          type="password"
          placeholder="Bearer token for council --serve"
          hint={
            inTauri
              ? "Authenticates the canonical loopback Council when configured. Installed releases adopt that runtime; debug builds may pass the token to their development sidecar."
              : "Must match COUNCIL_AUTH_TOKEN on the backend, or use COUNCIL_DEV_NO_AUTH=1 for local dev."
          }
        />
        {debugSidecarAvailable && (
          <div>
            <span className="label">Council binary path (debug sidecar only)</span>
            <div className="flex gap-2 mt-1.5">
              <input
                className="input flex-1 font-mono text-xs"
                value={form.councilPath}
                onChange={(e) => update("councilPath", e.target.value)}
                placeholder="Default: target/release/council"
              />
              <button type="button" onClick={pickCouncilBinary} className="btn">
                <FolderOpen className="w-4 h-4" />
              </button>
            </div>
          </div>
        )}
        {debugSidecarAvailable && <div data-testid="settings-council-root">
          <span className="label">Council root (--base-dir)</span>
          <input
            className="input mt-1.5 w-full font-mono text-xs"
            value={form.councilRoot}
            onChange={(e) => update("councilRoot", e.target.value)}
            placeholder="Default: council-rs repo root"
          />
          <p className="text-[10px] text-fg-dim mt-1">
            Debug desktop sidecar only: passed as --base-dir on Connect / start
            debug server. Use absolute paths (no ~).
          </p>
        </div>}
        {debugSidecarAvailable && <div data-testid="settings-librarian-base">
          <span className="label">Librarian base (RAG service)</span>
          <input
            className="input mt-1.5 w-full font-mono text-xs"
            value={form.librarianBase}
            onChange={(e) => update("librarianBase", e.target.value)}
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
            onClick={() => void persist()}
            disabled={saving}
            className="btn btn-primary"
          >
            {saving ? <Loader2 className="w-4 h-4 animate-spin" /> : null}
            Save
          </button>
          <button
            type="button"
            data-testid="settings-test-connection"
            onClick={() => void testConnection()}
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

      {!debugSidecarAvailable && (
        <div
          className="border border-border bg-bg-elevated p-5 space-y-3"
          data-testid="settings-installed-runtime"
        >
          <div className="flex items-center gap-2 border-b border-border pb-3">
            <Shield className="w-3.5 h-3.5 text-fg-dim" />
            <span className="label text-fg">Council runtime ownership</span>
          </div>
          <p className="text-xs font-mono text-fg-dim">
            {!inTauri
              ? "This browser UI connects to the Council API above. Council startup and backend environment are managed outside this page."
              : desktopRuntimeMode === "installed-release"
              ? "This installed app owns the bundled Council process for core War Room (no Rust/Node/Docker required). Gateway is optional and off by default. Missing Docker does not block core War Room."
              : desktopRuntimeMode === "detecting"
                ? "Checking the desktop build mode before enabling development-only sidecar controls…"
                : "Desktop build mode could not be verified, so development-only sidecar controls remain unavailable. Start or restart Council from the IRIN checkout."}
          </p>
        </div>
      )}

      {installedRelease && inTauri && (
        <div
          className="border border-border bg-bg-elevated p-5 space-y-3"
          data-testid="settings-gateway-pack"
        >
          <div className="flex items-center gap-2 border-b border-border pb-3">
            <Network className="w-3.5 h-3.5 text-fg-dim" />
            <span className="label text-fg">Optional Gateway Pack</span>
          </div>
          <p className="text-[10px] font-mono text-fg-dim">
            App-owned Compose project <code className="text-cyan">irin-desktop-gateway</code> only.
            Client key is stored in the macOS Keychain (never in private.json). Watch
            producer/dispatcher stay disarmed. Vertex and Claude/Codex CLI proxies are
            Direct-only / unsupported in v0.1 pack isolation.
          </p>
          <div
            className="text-xs font-mono space-y-1"
            data-testid="settings-gateway-pack-status"
          >
            <div>
              State:{" "}
              <span className="text-cyan">
                {packStatus
                  ? gatewayPackStateLabel(packStatus.state)
                  : "checking…"}
              </span>
            </div>
            {packStatus?.message ? (
              <p className="text-fg-dim whitespace-pre-wrap">{packStatus.message}</p>
            ) : null}
            {packStatus?.key_id ? (
              <div className="text-fg-dim">Key id: {packStatus.key_id}</div>
            ) : null}
            {packStatus?.pack_version ? (
              <div className="text-fg-dim">Pack: {packStatus.pack_version}</div>
            ) : null}
            <div className="text-fg-dim">
              Watch: producer=
              {String(packStatus?.watch_producer_enabled ?? false)} dispatcher=
              {String(packStatus?.watch_dispatcher_enabled ?? false)}
            </div>
          </div>
          <div className="flex flex-wrap gap-2">
            <button
              type="button"
              data-testid="settings-gateway-pack-enable"
              className="btn btn-cyan text-xs"
              disabled={packBusy}
              onClick={async () => {
                setPackBusy(true);
                try {
                  const st = await enableGatewayPack();
                  setPackStatus(st);
                  toast(
                    st.state === "authenticated_ready" ? "success" : "error",
                    st.message,
                  );
                } catch (e) {
                  toast("error", e instanceof Error ? e.message : String(e));
                  void refreshPackStatus();
                } finally {
                  setPackBusy(false);
                }
              }}
            >
              {packBusy ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : null}
              Enable Gateway
            </button>
            <button
              type="button"
              data-testid="settings-gateway-pack-disable"
              className="btn btn-primary text-xs"
              disabled={packBusy}
              onClick={async () => {
                setPackBusy(true);
                try {
                  const st = await disableGatewayPack();
                  setPackStatus(st);
                  toast("success", "Gateway disabled — Direct mode restored");
                } catch (e) {
                  toast("error", e instanceof Error ? e.message : String(e));
                } finally {
                  setPackBusy(false);
                }
              }}
            >
              Disable
            </button>
            <button
              type="button"
              data-testid="settings-gateway-pack-stop"
              className="btn text-xs"
              disabled={packBusy}
              onClick={async () => {
                setPackBusy(true);
                try {
                  const st = await stopGatewayPack();
                  setPackStatus(st);
                  toast("success", "Gateway pack stopped");
                } catch (e) {
                  toast("error", e instanceof Error ? e.message : String(e));
                } finally {
                  setPackBusy(false);
                }
              }}
            >
              Stop pack
            </button>
            <button
              type="button"
              data-testid="settings-gateway-pack-uninstall"
              className="btn text-xs text-red-400"
              disabled={packBusy}
              title="Destructive: removes irin-desktop-gateway volumes, app-owned gateway data, and Keychain client key"
              onClick={async () => {
                if (
                  !window.confirm(
                    "Uninstall the app-owned Gateway Pack? This deletes only irin-desktop-gateway data and the Keychain client key. Canonical Gateway is not touched.",
                  )
                ) {
                  return;
                }
                setPackBusy(true);
                try {
                  const st = await uninstallGatewayPack();
                  setPackStatus(st);
                  toast("success", "Gateway pack uninstalled");
                } catch (e) {
                  toast("error", e instanceof Error ? e.message : String(e));
                } finally {
                  setPackBusy(false);
                }
              }}
            >
              <Trash2 className="w-3.5 h-3.5" />
              Uninstall pack
            </button>
          </div>
        </div>
      )}

      {debugSidecarAvailable && <div className="border border-border bg-bg-elevated p-5 space-y-3" data-testid="settings-gateway-mode">
        <div className="flex items-center gap-2 border-b border-border pb-3">
          <Network className="w-3.5 h-3.5 text-fg-dim" />
          <span className="label text-fg">Gateway mode</span>
        </div>
        <p className="text-[10px] font-mono text-fg-dim">
          Debug desktop sidecar control: restarts <code>council --serve</code> with{" "}
          <code className="text-cyan">COUNCIL_VIA_GATEWAY=1</code> (sets{" "}
          <code className="text-cyan">COUNCIL_VIA_GATEWAY=0</code> when off).
          Per-session routing is available on the Deliberate panel;
          this sets the process-wide default. Requires{" "}
          <code className="text-cyan">GW_API_KEY</code> and a reachable gateway
          — the sidecar exits at startup otherwise (check the log panel below).
          In-flight deliberations and librarian WS streams are dropped on restart.
          Changing librarianBase or councilRoot here will also take effect on restart.
          Installed releases adopt the canonical runtime; change its environment and
          use <code>make runtime-restart</code> from the IRIN checkout instead.
        </p>
        <label className="flex items-center gap-2 text-xs font-mono cursor-pointer">
          <input
            type="checkbox"
            data-testid="settings-gateway-via"
            checked={sidecarViaGateway}
            onChange={(e) => setSidecarViaGateway(e.target.checked)}
            className="rounded border-border"
          />
          Route debug sidecar via gateway
        </label>
        <button
          type="button"
          data-testid="settings-restart-gateway"
          className="btn btn-cyan text-xs"
          disabled={!inTauri || restarting}
          title={
            inTauri
              ? undefined
              : "Desktop (Tauri) only — in the browser, restart council --serve with COUNCIL_VIA_GATEWAY=1 manually"
          }
          onClick={async () => {
            setRestarting(true);
            try {
              const msg = await restartSidecar(
                sidecarViaGateway,
                form.councilRoot || undefined,
                form.librarianBase || undefined,
              );
              toast("success", msg);
              if (showServerLog) void refreshLogs();
            } catch (e) {
              toast("error", e instanceof Error ? e.message : String(e));
            } finally {
              setRestarting(false);
            }
          }}
        >
          {restarting ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : null}
          Restart debug sidecar with gateway {sidecarViaGateway ? "on" : "off"}
        </button>
      </div>}

      {debugSidecarAvailable && (
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
                    form.councilPath || undefined,
                    undefined,
                    form.authToken,
                    form.councilRoot || undefined,
                    form.librarianBase || undefined,
                  );
                  toast("success", msg);
                  if (showServerLog) void refreshLogs();
                } catch (e) {
                  toast("error", e instanceof Error ? e.message : String(e));
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
                  toast("success", msg);
                } catch (e) {
                  toast("error", e instanceof Error ? e.message : String(e));
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
              onChange={(e) => setShowServerLog(e.target.checked)}
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
                  setServerLogs([]);
                }}
              >
                <Trash2 className="w-3 h-3" />
                Clear
              </button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function Field({
  label,
  value,
  onChange,
  placeholder,
  hint,
  type = "text",
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  hint?: string;
  type?: "text" | "password";
}) {
  return (
    <div>
      <span className="label">{label}</span>
      <input
        type={type}
        className="input mt-1.5 w-full font-mono text-xs"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        autoComplete={type === "password" ? "off" : undefined}
      />
      {hint && <p className="text-[10px] text-fg-dim mt-1">{hint}</p>}
    </div>
  );
}

function StatusLine({
  label,
  status,
  detail,
  testId,
}: {
  label: string;
  status: "idle" | "loading" | "ok" | "fail" | "skip";
  detail: string | null;
  testId?: string;
}) {
  return (
    <div
      className="flex items-start gap-2"
      data-testid={testId}
      data-health-state={testId ? status : undefined}
    >
      {status === "loading" && (
        <Loader2 className="w-3.5 h-3.5 animate-spin text-cyan shrink-0 mt-0.5" />
      )}
      {status === "ok" && (
        <CheckCircle2 className="w-3.5 h-3.5 text-success shrink-0 mt-0.5" />
      )}
      {(status === "fail" || status === "skip") && (
        <XCircle
          className={cn(
            "w-3.5 h-3.5 shrink-0 mt-0.5",
            status === "fail" ? "text-danger" : "text-fg-dim",
          )}
        />
      )}
      <div>
        <span className="text-fg">{label}</span>
        {detail && <div className="text-fg-dim">{detail}</div>}
      </div>
    </div>
  );
}
