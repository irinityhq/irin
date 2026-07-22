import { getAuthToken, getWsBase } from "./runtime-config";

/** Quick WebSocket upgrade probe (subprotocol auth, no deliberation). */
export function probeWsUpgrade(timeoutMs = 4000): Promise<{
  ok: boolean;
  detail: string;
}> {
  if (typeof WebSocket === "undefined") {
    return Promise.resolve({ ok: false, detail: "WebSocket unavailable" });
  }

  return new Promise((resolve) => {
    const token = getAuthToken();
    const base = getWsBase().replace(/\/$/, "");
    const url = `${base}/ws/deliberate`;
    const protocols = token ? ["council", `token.${token}`] : undefined;
    const ws = protocols ? new WebSocket(url, protocols) : new WebSocket(url);
    let settled = false;

    const finish = (ok: boolean, detail: string) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      try {
        ws.close();
      } catch {
        /* noop */
      }
      resolve({ ok, detail });
    };

    const timer = setTimeout(
      () => finish(false, "WebSocket probe timed out (check token and WS base)"),
      timeoutMs,
    );

    ws.onopen = () => {
      if (token && ws.protocol !== "council") {
        finish(
          false,
          `WebSocket opened but negotiated protocol "${ws.protocol || "(none)"}" — expected "council" (server must echo Sec-WebSocket-Protocol)`,
        );
        return;
      }
      finish(true, "WebSocket upgrade OK (subprotocol auth accepted)");
    };

    ws.onerror = () => {
      finish(
        false,
        token
          ? "WebSocket upgrade failed — token mismatch or server unreachable"
          : "WebSocket upgrade failed — server may require auth token in Settings",
      );
    };

    ws.onclose = (ev) => {
      if (settled) return;
      if (ev.code === 1006 || ev.code === 1008) {
        finish(
          false,
          `WebSocket closed before open (code ${ev.code}) — likely 401 on upgrade`,
        );
      }
    };
  });
}