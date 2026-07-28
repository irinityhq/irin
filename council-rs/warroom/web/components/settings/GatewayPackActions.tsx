import { Loader2, Trash2 } from "lucide-react";
import {
  disableGatewayPack,
  enableGatewayPack,
  stopGatewayPack,
  uninstallGatewayPack,
} from "@/lib/tauri";
import type { ToastType } from "@/components/Toast";
import type { GatewayPackAction } from "./useDesktopActions";

export function GatewayPackActions({
  busy,
  confirmingUninstall,
  onConfirmingUninstall,
  runAction,
  notify,
}: {
  busy: boolean;
  confirmingUninstall: boolean;
  onConfirmingUninstall: (confirming: boolean) => void;
  runAction: GatewayPackAction;
  notify: (type: ToastType, message: string) => void;
}) {
  return (
    <div className="flex flex-wrap gap-2">
      <button
        type="button"
        data-testid="settings-gateway-pack-enable"
        aria-label="Enable Gateway"
        aria-busy={busy}
        className="btn btn-cyan text-xs"
        disabled={busy}
        onClick={() =>
          void runAction(enableGatewayPack, (status) => {
            // Enable proves pack auth (spawn_capable); governed_ready
            // lands only after Council restart.
            const ok = status.spawn_capable === true;
            notify(ok ? "success" : "error", status.message);
          })
        }
      >
        {busy ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : null}
        Enable Gateway
      </button>
      <button
        type="button"
        data-testid="settings-gateway-pack-disable"
        aria-label="Disable"
        aria-busy={busy}
        className="btn btn-primary text-xs"
        disabled={busy}
        onClick={() =>
          void runAction(disableGatewayPack, (status) => {
            notify(
              status.council_governed ? "error" : "success",
              status.message || "Gateway disabled — Direct mode restored",
            );
          })
        }
      >
        Disable
      </button>
      <button
        type="button"
        data-testid="settings-gateway-pack-stop"
        aria-label="Stop pack"
        aria-busy={busy}
        className="btn text-xs"
        disabled={busy}
        onClick={() =>
          void runAction(stopGatewayPack, (status) => {
            notify(
              status.council_governed ? "error" : "success",
              status.message || "Gateway pack stopped",
            );
          })
        }
      >
        Stop pack
      </button>
      <button
        type="button"
        data-testid="settings-gateway-pack-uninstall"
        aria-label="Uninstall pack"
        aria-busy={busy}
        className="btn text-xs text-red-400"
        disabled={busy || confirmingUninstall}
        title="Destructive: removes irin-desktop-gateway volumes, app-owned gateway data, and Keychain client key"
        onClick={() => onConfirmingUninstall(true)}
      >
        <Trash2 className="w-3.5 h-3.5" />
        Uninstall pack
      </button>
      {confirmingUninstall && (
        <>
          <button
            type="button"
            data-testid="settings-gateway-pack-uninstall-confirm"
            aria-label="Confirm uninstall"
            className="btn text-xs text-red-400"
            onClick={() => {
              // Two-step inline confirm — window.confirm is unreliable
              // in the packaged WKWebView, and a dedicated button keeps
              // the destructive action explicit and accessibility-testable.
              onConfirmingUninstall(false);
              void runAction(uninstallGatewayPack, (status) => {
                notify(
                  status.state === "not_installed" || !status.enabled
                    ? "success"
                    : "error",
                  status.message || "Gateway pack uninstalled",
                );
              });
            }}
          >
            Confirm uninstall
          </button>
          <button
            type="button"
            aria-label="Cancel uninstall"
            className="btn text-xs"
            onClick={() => onConfirmingUninstall(false)}
          >
            Cancel
          </button>
        </>
      )}
    </div>
  );
}
