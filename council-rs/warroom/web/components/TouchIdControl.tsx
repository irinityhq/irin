"use client";

import { Fingerprint, Loader2 } from "lucide-react";
import { deriveTouchIdView, type TouchIdStatus } from "@/lib/touch-id";

/**
 * Touch ID product control. Presentational: it renders the native projection
 * and calls back. It never invokes a Tauri command itself, holds no ceremony
 * state, and has no timer of its own — every state it shows came from the
 * native host's status projection.
 *
 * Rendered inside the Gateway Pack card so arming sits directly beside the
 * Gateway control the operator already understands.
 */
export default function TouchIdControl({
  status,
  primaryBusy,
  disarmBusy,
  onEnroll,
  onArm,
  onRenew,
  onDisarm,
}: {
  status: TouchIdStatus | null;
  primaryBusy: boolean;
  disarmBusy: boolean;
  onEnroll: () => void;
  onArm: () => void;
  onRenew: () => void;
  onDisarm: () => void;
}) {
  const view = deriveTouchIdView(status);
  const primary =
    view.primaryAction === "enroll"
      ? onEnroll
      : view.primaryAction === "renew"
        ? onRenew
        : onArm;

  return (
    <div
      className="border-t border-border pt-3 space-y-2"
      data-testid="settings-touch-id"
      data-touch-id-state={view.state}
    >
      <div className="flex items-center gap-2">
        <Fingerprint className="w-3.5 h-3.5 text-fg-dim" />
        <span className="label text-fg" data-testid="settings-touch-id-label">
          {view.label}
        </span>
      </div>
      {view.detail ? (
        <p
          className="text-[10px] font-mono text-fg-dim"
          data-testid="settings-touch-id-detail"
        >
          {view.detail}
        </p>
      ) : null}
      <div className="flex flex-wrap gap-2">
        {view.primaryLabel ? (
          <button
            type="button"
            data-testid="settings-touch-id-primary"
            data-touch-id-action={view.primaryAction}
            aria-label={view.primaryLabel}
            aria-busy={primaryBusy}
            className="btn btn-cyan text-xs"
            disabled={primaryBusy || disarmBusy || !view.primaryEnabled}
            onClick={primary}
          >
            {primaryBusy ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : null}
            {view.primaryLabel}
          </button>
        ) : null}
        {view.showDisarm ? (
          <button
            type="button"
            data-testid="settings-touch-id-disarm"
            aria-label="Disarm"
            aria-busy={disarmBusy}
            className="btn text-xs text-red-400"
            // Disarm is the kill switch: an in-flight Touch ID prompt must not
            // hide it. Only a disarm request already in flight disables it.
            disabled={disarmBusy}
            onClick={onDisarm}
          >
            Disarm
          </button>
        ) : null}
      </div>
    </div>
  );
}
