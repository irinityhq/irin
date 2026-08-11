import { useCallback, useState } from "react";
import {
  disarmTouchId,
  type DesktopStatusSnapshot,
  type GatewayPackStatus,
  type TouchIdStatus,
} from "@/lib/tauri";
import { emitWarroomConfigChanged } from "@/lib/runtime-config";
import type { ToastType } from "@/components/Toast";

export type GatewayPackAction = (
  action: () => Promise<DesktopStatusSnapshot>,
  onSuccess: (status: GatewayPackStatus) => void,
) => Promise<void>;

/**
 * Pack success path: update snapshot, then emit exactly one config-change
 * signal so War Room re-probes Council. Failure emits zero signals.
 * Extracted for unit characterization without a React renderer.
 */
export async function runGatewayPackActionOnce(
  action: () => Promise<DesktopStatusSnapshot>,
  applySnapshot: (next: DesktopStatusSnapshot) => void,
  onSuccess: (status: GatewayPackStatus) => void,
  onError: (message: string) => void,
  emit: () => void = emitWarroomConfigChanged,
): Promise<"ok" | "error"> {
  try {
    const snap = await action();
    applySnapshot(snap);
    emit();
    onSuccess(snap.pack);
    return "ok";
  } catch (error) {
    onError(error instanceof Error ? error.message : String(error));
    return "error";
  }
}

export function useDesktopActions(
  applySnapshot: (next: DesktopStatusSnapshot) => void,
  toast: (type: ToastType, message: string) => void,
) {
  const [packBusy, setPackBusy] = useState(false);
  const [touchIdBusy, setTouchIdBusy] = useState(false);
  const [touchIdDisarmBusy, setTouchIdDisarmBusy] = useState(false);

  const runGatewayPackAction = useCallback(
    async (
      action: () => Promise<DesktopStatusSnapshot>,
      onSuccess: (status: GatewayPackStatus) => void,
    ) => {
      setPackBusy(true);
      try {
        await runGatewayPackActionOnce(
          action,
          applySnapshot,
          onSuccess,
          (message) => toast("error", message),
        );
      } finally {
        setPackBusy(false);
      }
    },
    [applySnapshot, toast],
  );

  const runTouchIdAction = useCallback(
    async (
      action: () => Promise<DesktopStatusSnapshot>,
      successMessage: string | ((status: TouchIdStatus) => string),
    ) => {
      setTouchIdBusy(true);
      try {
        const snap = await action();
        applySnapshot(snap);
        const message =
          typeof successMessage === "function"
            ? successMessage(snap.touch_id)
            : successMessage;
        toast("success", message);
      } catch (error) {
        toast("error", error instanceof Error ? error.message : String(error));
      } finally {
        setTouchIdBusy(false);
      }
    },
    [applySnapshot, toast],
  );

  const runTouchIdDisarm = useCallback(async () => {
    setTouchIdDisarmBusy(true);
    try {
      const snap = await disarmTouchId();
      applySnapshot(snap);
      toast("success", "Disarmed");
    } catch (e) {
      toast("error", e instanceof Error ? e.message : String(e));
    } finally {
      setTouchIdDisarmBusy(false);
    }
  }, [applySnapshot, toast]);

  return {
    packBusy,
    touchIdBusy,
    touchIdDisarmBusy,
    runGatewayPackAction,
    runTouchIdAction,
    runTouchIdDisarm,
  };
}
