"use client";

import { useCallback, useEffect, useState } from "react";
import { Loader2, Network } from "lucide-react";
import {
  armWithTouchId,
  enrollTouchId,
  getDesktopRuntimeMode,
  getDesktopStatusSnapshot,
  isTauri,
  renewTouchIdArm,
  type DesktopStatusSnapshot,
  type GatewayPackStatus,
  type PhoneAccessStatus,
  type TouchIdStatus,
} from "@/lib/tauri";
import {
  getRuntimeConfig,
  loadRuntimeConfig,
  saveRuntimeConfig,
  type RuntimeConfig,
} from "@/lib/runtime-config";
import { touchIdArmSuccessMessage } from "@/lib/touch-id";
import { mergeIfNewer } from "@/lib/desktop-status";
import TouchIdControl from "./TouchIdControl";
import PhoneAccessControl from "./PhoneAccessControl";
import { useToast } from "./Toast";
import { ConnectionCard } from "./settings/ConnectionCard";
import {
  InstalledRuntimeCard,
  type DesktopRuntimeModeState,
} from "./settings/InstalledRuntimeCard";
import { GatewayPackStatusView } from "./settings/GatewayPackStatusView";
import { GatewayPackActions } from "./settings/GatewayPackActions";
import { DebugSidecarCard } from "./settings/DebugSidecarCard";
import { useConnectionTest } from "./settings/useConnectionTest";
import { useServerLogs } from "./settings/useServerLogs";
import { useDesktopActions } from "./settings/useDesktopActions";
import { usePhoneAccess } from "./settings/usePhoneAccess";
import { useSidecarRestart } from "./settings/useSidecarRestart";

export default function SettingsPanel() {
  const { toast } = useToast();
  const [form, setForm] = useState<RuntimeConfig>(getRuntimeConfig);
  const [saving, setSaving] = useState(false);
  /** Single host-authoritative snapshot for pack / Touch ID / phone. */
  const [desktopStatus, setDesktopStatus] =
    useState<DesktopStatusSnapshot | null>(null);
  const packStatus: GatewayPackStatus | null = desktopStatus?.pack ?? null;
  const phoneStatus: PhoneAccessStatus | null = desktopStatus?.phone ?? null;
  const touchIdStatus: TouchIdStatus | null = desktopStatus?.touch_id ?? null;
  const [confirmingUninstall, setConfirmingUninstall] = useState(false);
  const inTauri = isTauri();
  const [desktopRuntimeMode, setDesktopRuntimeMode] =
    useState<DesktopRuntimeModeState>(inTauri ? "detecting" : "unavailable");
  const debugSidecarAvailable = desktopRuntimeMode === "development";
  const installedRelease = desktopRuntimeMode === "installed-release";

  const applySnapshot = useCallback((next: DesktopStatusSnapshot) => {
    setDesktopStatus((prev) => mergeIfNewer(prev, next));
  }, []);

  const {
    healthStatus,
    healthDetail,
    gatewayStatus,
    gatewayDetail,
    wsStatus,
    wsDetail,
    testConnection,
  } = useConnectionTest(form, setForm, toast);
  const {
    showServerLog,
    setShowServerLog,
    serverLogs,
    setServerLogs,
    refreshLogs,
  } = useServerLogs(inTauri);
  const {
    packBusy,
    touchIdBusy,
    touchIdDisarmBusy,
    runGatewayPackAction,
    runTouchIdAction,
    runTouchIdDisarm,
  } = useDesktopActions(applySnapshot, toast);
  const {
    phoneBusy,
    onEnable: onPhoneAccessEnable,
    onDisable: onPhoneAccessDisable,
  } = usePhoneAccess(applySnapshot, toast);
  const { sidecarViaGateway, setSidecarViaGateway, restarting, restart } =
    useSidecarRestart(form, toast, showServerLog, refreshLogs);

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

  // One snapshot subscription: initial invoke + host `desktop-status` events.
  // No renderer intervals, fences, or poll epochs — the host owns ordering.
  useEffect(() => {
    if (!inTauri || desktopRuntimeMode !== "installed-release") return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void getDesktopStatusSnapshot()
      .then((snap) => {
        if (!cancelled) applySnapshot(snap);
      })
      .catch(() => {
        // Keep the last known projection (or null placeholder) on failure.
      });
    void import("@tauri-apps/api/event")
      .then(({ listen }) =>
        listen<DesktopStatusSnapshot>("desktop-status", (event) => {
          if (!cancelled) applySnapshot(event.payload);
        }),
      )
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      })
      .catch(() => {
        // Event bridge unavailable — initial snapshot still applies.
      });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [inTauri, desktopRuntimeMode, applySnapshot]);

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

  return (
    <div className="max-w-2xl mx-auto space-y-6">
      <ConnectionCard
        form={form}
        onUpdate={update}
        inTauri={inTauri}
        debugSidecarAvailable={debugSidecarAvailable}
        saving={saving}
        onSave={persist}
        onTestConnection={testConnection}
        healthStatus={healthStatus}
        healthDetail={healthDetail}
        wsStatus={wsStatus}
        wsDetail={wsDetail}
        gatewayStatus={gatewayStatus}
        gatewayDetail={gatewayDetail}
      />

      {!debugSidecarAvailable && (
        <InstalledRuntimeCard
          inTauri={inTauri}
          desktopRuntimeMode={desktopRuntimeMode}
        />
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
          <GatewayPackStatusView status={packStatus} />
          <GatewayPackActions
            busy={packBusy}
            confirmingUninstall={confirmingUninstall}
            onConfirmingUninstall={setConfirmingUninstall}
            runAction={runGatewayPackAction}
            notify={toast}
          />
          <TouchIdControl
            status={touchIdStatus}
            primaryBusy={touchIdBusy}
            disarmBusy={touchIdDisarmBusy}
            onEnroll={() =>
              void runTouchIdAction(enrollTouchId, "Touch ID ready")
            }
            onArm={() =>
              void runTouchIdAction(armWithTouchId, touchIdArmSuccessMessage)
            }
            onRenew={() =>
              void runTouchIdAction(renewTouchIdArm, "Lease renewed")
            }
            onDisarm={() => void runTouchIdDisarm()}
          />
        </div>
      )}

      {installedRelease && inTauri && (
        <PhoneAccessControl
          status={phoneStatus}
          busy={phoneBusy}
          notify={toast}
          onEnable={onPhoneAccessEnable}
          onDisable={onPhoneAccessDisable}
        />
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
          Changing librarianBase here will also take effect on restart.
          Installed releases own the bundled Council; change its environment and
          restart the app-owned Council from Settings or relaunch the desktop app.
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
          onClick={() => void restart()}
        >
          {restarting ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : null}
          Restart debug sidecar with gateway {sidecarViaGateway ? "on" : "off"}
        </button>
      </div>}

      {debugSidecarAvailable && (
        <DebugSidecarCard
          form={form}
          notify={toast}
          showServerLog={showServerLog}
          onShowServerLogChange={setShowServerLog}
          serverLogs={serverLogs}
          onServerLogsChange={setServerLogs}
          refreshLogs={refreshLogs}
        />
      )}
    </div>
  );
}
