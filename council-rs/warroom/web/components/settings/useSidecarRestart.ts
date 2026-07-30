import { useState } from "react";
import { restartSidecar } from "@/lib/tauri";
import type { RuntimeConfig } from "@/lib/runtime-config";
import type { ToastType } from "@/components/Toast";

export function useSidecarRestart(
  form: RuntimeConfig,
  toast: (type: ToastType, message: string) => void,
  showServerLog: boolean,
  refreshLogs: () => Promise<void>,
) {
  const [sidecarViaGateway, setSidecarViaGateway] = useState(false);
  const [restarting, setRestarting] = useState(false);

  const restart = async () => {
    setRestarting(true);
    try {
      const msg = await restartSidecar(
        sidecarViaGateway,
        form.librarianBase || undefined,
      );
      toast("success", msg);
      if (showServerLog) void refreshLogs();
    } catch (e) {
      toast("error", e instanceof Error ? e.message : String(e));
    } finally {
      setRestarting(false);
    }
  };

  return { sidecarViaGateway, setSidecarViaGateway, restarting, restart };
}
