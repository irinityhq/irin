import { Shield } from "lucide-react";
import type { DesktopRuntimeMode } from "@/lib/tauri";

export type DesktopRuntimeModeState =
  | DesktopRuntimeMode
  | "detecting"
  | "unavailable";

export function InstalledRuntimeCard({
  inTauri,
  desktopRuntimeMode,
}: {
  inTauri: boolean;
  desktopRuntimeMode: DesktopRuntimeModeState;
}) {
  return (
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
  );
}
