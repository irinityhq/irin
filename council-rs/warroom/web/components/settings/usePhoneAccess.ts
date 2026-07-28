import { useState } from "react";
import {
  disablePhoneAccess,
  enablePhoneAccess,
  type DesktopStatusSnapshot,
} from "@/lib/tauri";
import type { ToastType } from "@/components/Toast";

export function usePhoneAccess(
  applySnapshot: (next: DesktopStatusSnapshot) => void,
  toast: (type: ToastType, message: string) => void,
) {
  const [phoneBusy, setPhoneBusy] = useState(false);

  const onEnable = async () => {
    setPhoneBusy(true);
    try {
      const snap = await enablePhoneAccess();
      applySnapshot(snap);
      toast(
        snap.phone.state === "ready" ? "success" : "error",
        snap.phone.message,
      );
    } catch (e) {
      toast("error", e instanceof Error ? e.message : String(e));
    } finally {
      setPhoneBusy(false);
    }
  };

  const onDisable = async () => {
    setPhoneBusy(true);
    try {
      const snap = await disablePhoneAccess();
      applySnapshot(snap);
      toast(
        snap.phone.state === "off" ? "success" : "error",
        snap.phone.message,
      );
    } catch (e) {
      toast("error", e instanceof Error ? e.message : String(e));
    } finally {
      setPhoneBusy(false);
    }
  };

  return { phoneBusy, onEnable, onDisable };
}
