import { useState } from "react";
import { api } from "@/lib/api";
import {
  getGatewayBase,
  saveRuntimeConfig,
  type RuntimeConfig,
} from "@/lib/runtime-config";
import { probeWsUpgrade } from "@/lib/ws-probe";
import type { ToastType } from "@/components/Toast";

function councilAuthHint(status: number): string {
  if (status === 401) {
    return (
      "401 Unauthorized — Settings auth token must match COUNCIL_AUTH_TOKEN on " +
      "council --serve (same value on both sides). WebSocket uses Sec-WebSocket-Protocol " +
      "token.<value>. Release Tauri does not use COUNCIL_DEV_NO_AUTH."
    );
  }
  return `${status} error`;
}

export function useConnectionTest(
  form: RuntimeConfig,
  setForm: (next: RuntimeConfig) => void,
  toast: (type: ToastType, message: string) => void,
) {
  const [healthStatus, setHealthStatus] = useState<
    "idle" | "loading" | "ok" | "fail"
  >("idle");
  const [healthDetail, setHealthDetail] = useState<string | null>(null);
  const [gatewayStatus, setGatewayStatus] = useState<
    "idle" | "loading" | "ok" | "fail" | "skip"
  >("idle");
  const [gatewayDetail, setGatewayDetail] = useState<string | null>(null);
  const [wsStatus, setWsStatus] = useState<
    "idle" | "loading" | "ok" | "fail" | "skip"
  >("idle");
  const [wsDetail, setWsDetail] = useState<string | null>(null);

  const testConnection = async () => {
    setHealthStatus("loading");
    setGatewayStatus("loading");
    setHealthDetail(null);
    setGatewayDetail(null);
    setWsStatus("loading");
    setWsDetail(null);
    try {
      const saved = await saveRuntimeConfig(form);
      setForm(saved);
    } catch (e) {
      setHealthStatus("fail");
      setGatewayStatus("fail");
      const msg = e instanceof Error ? e.message : "Save failed";
      setHealthDetail(msg);
      setGatewayDetail(msg);
      toast("error", msg);
      return;
    }
    try {
      const h = await api.health();
      setHealthStatus("ok");
      const missing =
        h.providers_missing?.length > 0
          ? ` · missing: ${h.providers_missing.join(", ")}`
          : "";
      const build = h.build_sha
        ? ` · build ${h.build_sha.slice(0, 12)}${h.build_dirty ? "-dirty" : ""}`
        : " · build unavailable";
      setHealthDetail(
        `council ${h.council_version} · stream ${h.stream_version}` +
          `${build}${missing}`,
      );

      const wsProbe = await probeWsUpgrade();
      if (wsProbe.ok) {
        setWsStatus("ok");
        setWsDetail(wsProbe.detail);
      } else {
        setWsStatus("fail");
        setWsDetail(wsProbe.detail);
      }
    } catch (e) {
      setHealthStatus("fail");
      setWsStatus("fail");
      const msg = e instanceof Error ? e.message : String(e);
      const statusMatch = msg.match(/^(\d{3})\b/);
      const status = statusMatch ? Number(statusMatch[1]) : 0;
      const detail = status === 401 ? councilAuthHint(401) : msg;
      setHealthDetail(detail);
      setWsDetail(
        status === 401
          ? "REST failed — fix token before WebSocket can connect"
          : "Skipped — REST health failed",
      );
    }
    const gw = getGatewayBase().trim();
    if (!gw) {
      setGatewayStatus("skip");
      setGatewayDetail("No gateway URL configured");
      return;
    }
    try {
      const res = await fetch(`${gw.replace(/\/$/, "")}/health`, {
        cache: "no-store",
      });
      if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
      setGatewayStatus("ok");
      setGatewayDetail("Gateway reachable");
    } catch (e) {
      setGatewayStatus("fail");
      setGatewayDetail(e instanceof Error ? e.message : String(e));
    }
  };

  return {
    healthStatus,
    healthDetail,
    gatewayStatus,
    gatewayDetail,
    wsStatus,
    wsDetail,
    testConnection,
  };
}
