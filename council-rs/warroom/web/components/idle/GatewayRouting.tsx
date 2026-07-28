import { Network } from "lucide-react";
import { SENSITIVITY_LEVELS } from "@/lib/gateway-mode";
import type { DesktopRuntimeMode } from "@/lib/tauri";
import type { GatewaySensitivity } from "@/lib/ws";
import type { ToastType } from "../Toast";
import { Toggle } from "./Toggle";

export function GatewayRouting({
  governedAllowed,
  desktopMode,
  viaGateway,
  setViaGateway,
  sensitivity,
  setSensitivity,
  toast,
}: {
  governedAllowed: boolean;
  desktopMode: DesktopRuntimeMode | "detecting" | "unavailable";
  viaGateway: boolean;
  setViaGateway: (v: boolean) => void;
  sensitivity: GatewaySensitivity;
  setSensitivity: (v: GatewaySensitivity) => void;
  toast: (type: ToastType, message: string) => void;
}) {
  return (
    <div className="rounded-md border border-border bg-bg-overlay/40 p-3 space-y-3" data-testid="gateway-routing">
      <Toggle
        label="Governed via Gateway"
        sub={
          !governedAllowed && desktopMode === "installed-release"
            ? "Requires authenticated Gateway Pack (Settings → Enable Gateway)"
            : viaGateway
              ? "All model calls fail closed through Gateway"
              : "Direct provider and CLI calls"
        }
        value={viaGateway && governedAllowed}
        onChange={(v) => {
          if (v && !governedAllowed) {
            toast(
              "error",
              "Gateway Pack is not authenticated-ready. Use Settings → Enable Gateway first.",
            );
            return;
          }
          setViaGateway(v);
        }}
        icon={<Network className="w-4 h-4" />}
        tone="cyan"
        testId="gateway-toggle"
      />
      {viaGateway ? (
        <div>
          <span className="label">Sensitivity</span>
          <select
            data-testid="gateway-sensitivity"
            value={sensitivity}
            onChange={(e) => setSensitivity(e.target.value as GatewaySensitivity)}
            className="input mt-1.5 max-w-[180px]"
          >
            {SENSITIVITY_LEVELS.map((level) => (
              <option key={level} value={level}>
                {level.toUpperCase()}
              </option>
            ))}
          </select>
          <p className="text-[10px] text-fg-dim mt-1">
            Sent lowercase on the wire; the server maps it to the gateway&apos;s{" "}
            <code className="text-cyan">X-Sensitivity-Level</code> header.
          </p>
        </div>
      ) : (
        <p className="text-[10px] font-mono text-fg-dim">
          Direct — this proceeding explicitly bypasses Gateway governance.
        </p>
      )}
    </div>
  );
}
