import { useCallback, useState } from "react";
import {
  disarmTouchId,
  type DesktopStatusSnapshot,
  type GatewayPackStatus,
  type TouchIdStatus,
} from "@/lib/tauri";
import type { ToastType } from "@/components/Toast";

export type GatewayPackAction = (
  action: () => Promise<DesktopStatusSnapshot>,
  onSuccess: (status: GatewayPackStatus) => void,
) => Promise<void>;

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
        const snap = await action();
        applySnapshot(snap);
        window.dispatchEvent(new Event("warroom-config-changed"));
        onSuccess(snap.pack);
      } catch (error) {
        toast("error", error instanceof Error ? error.message : String(error));
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
