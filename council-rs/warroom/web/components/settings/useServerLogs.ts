import { useCallback, useEffect, useState } from "react";
import { getServerLogs, onCouncilLog } from "@/lib/tauri";

export function useServerLogs(inTauri: boolean) {
  const [showServerLog, setShowServerLog] = useState(false);
  const [serverLogs, setServerLogs] = useState<string[]>([]);

  const refreshLogs = useCallback(async () => {
    if (!inTauri) return;
    try {
      setServerLogs(await getServerLogs());
    } catch {
      setServerLogs([]);
    }
  }, [inTauri]);

  useEffect(() => {
    if (!showServerLog || !inTauri) return;
    void refreshLogs();
    let aborted = false;
    let unlisten: (() => void) | undefined;
    void onCouncilLog((line) => {
      setServerLogs((prev) => [...prev.slice(-499), line]);
    }).then((fn) => {
      if (aborted) fn();
      else unlisten = fn;
    });
    return () => {
      aborted = true;
      unlisten?.();
    };
  }, [showServerLog, inTauri, refreshLogs]);

  return { showServerLog, setShowServerLog, serverLogs, setServerLogs, refreshLogs };
}
