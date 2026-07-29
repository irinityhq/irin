"use client";

import { Loader2, Smartphone } from "lucide-react";
import type { PhoneAccessStatus } from "@/lib/tauri";

export type PhoneAccessNotice = (kind: "success" | "error", message: string) => void;

interface PhoneAccessControlProps {
  status: PhoneAccessStatus | null;
  busy: boolean;
  onEnable: () => void | Promise<void>;
  onDisable: () => void | Promise<void>;
  notify: PhoneAccessNotice;
  copyText?: (text: string) => Promise<void>;
}

export default function PhoneAccessControl({
  status,
  busy,
  onEnable,
  onDisable,
  notify,
  copyText,
}: PhoneAccessControlProps) {
  const enabled = status?.enabled ?? false;
  const interrupted = status?.interrupted ?? false;
  // Recovery disable must remain available after a partial apply that left
  // interrupted=true with enabled=false.
  const canRecoverOrDisable = enabled || interrupted;
  const stateLabel = status?.state.replaceAll("_", " ") ?? "checking…";
  const recoverLabel = "Clear interrupted setup";
  const disableLabel = interrupted && !enabled ? recoverLabel : "Disable phone access";

  const copyAddress = async () => {
    const address = status?.tailnet_url;
    if (!address) return;

    try {
      if (copyText) {
        await copyText(address);
      } else {
        await navigator.clipboard.writeText(address);
      }
      notify("success", "Phone address copied");
    } catch {
      notify("error", "Could not copy the phone address");
    }
  };

  return (
    <div
      className="border border-border bg-bg-elevated p-5 space-y-3"
      data-testid="settings-phone-access"
      data-phone-access-state={status?.state ?? "checking"}
      data-phone-access-interrupted={interrupted ? "true" : "false"}
    >
      <div className="flex items-center gap-2 border-b border-border pb-3">
        <Smartphone className="w-3.5 h-3.5 text-fg-dim" />
        <span className="label text-fg">Private phone access</span>
      </div>
      <p className="text-[10px] font-mono text-fg-dim">
        Publishes this installed app on your private Tailscale network over one
        HTTPS address on port 8443 (other Serve apps on 443 can coexist). IRIN
        uses Tailscale Serve only and never enables public Funnel access. Any
        device on the same tailnet that your Tailscale ACLs or grants allow can
        open the full URL (including the port) in a browser; War Room uses
        same-origin REST and WebSocket on that origin.
      </p>
      <div
        className="text-xs font-mono space-y-1"
        data-testid="settings-phone-access-status"
      >
        <div>
          State: <span className="text-cyan">{stateLabel}</span>
        </div>
        {status?.message ? (
          <p className="text-fg-dim whitespace-pre-wrap">{status.message}</p>
        ) : null}
        {status?.tailnet_url ? (
          <div className="flex flex-wrap items-center gap-2">
            <code className="text-cyan break-all">{status.tailnet_url}</code>
            <button
              type="button"
              className="btn text-[10px]"
              aria-label="Copy phone address"
              data-testid="settings-phone-access-copy"
              onClick={() => void copyAddress()}
            >
              Copy
            </button>
          </div>
        ) : null}
        {enabled ? (
          <div className="text-fg-dim">
            Gateway routes: {status?.gateway_routes ? "included" : "off"}
          </div>
        ) : null}
      </div>
      <div className="flex flex-wrap gap-2">
        <button
          type="button"
          data-testid="settings-phone-access-enable"
          aria-label={enabled ? "Refresh phone routes" : "Enable phone access"}
          aria-busy={busy}
          className="btn btn-cyan text-xs"
          disabled={busy}
          onClick={() => void onEnable()}
        >
          {busy ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : null}
          {enabled ? "Refresh phone routes" : "Enable phone access"}
        </button>
        <button
          type="button"
          data-testid="settings-phone-access-disable"
          aria-label={disableLabel}
          aria-busy={busy}
          className="btn btn-primary text-xs"
          disabled={busy || !canRecoverOrDisable}
          onClick={() => void onDisable()}
        >
          {disableLabel}
        </button>
      </div>
      <p className="text-[10px] font-mono text-fg-dim">
        On the phone browser, open the address above. If Council requires auth,
        set the same token under Settings → Auth token, then Test connection
        (REST health and WebSocket upgrade). The token stays only in this
        browser tab&apos;s session and is never written to durable localStorage.
      </p>
    </div>
  );
}
