import path from "node:path";
import type { NextConfig } from "next";

const isDev = process.env.NODE_ENV !== "production";
const isTauriExport = process.env.WARROOM_TAURI_EXPORT === "1";

function allowedDevOrigins(): string[] {
  const local = [
    "http://127.0.0.1:3000",
    "http://localhost:3000",
    "http://127.0.0.1:3010",
    "http://localhost:3010",
  ];
  const configured = (process.env.WARROOM_ALLOWED_DEV_ORIGINS || "")
    .split(",")
    .map((origin) => origin.trim())
    .filter(Boolean);
  return Array.from(new Set([...local, ...configured]));
}

/** Gateway host for browser CSP connect-src (Outbox tab). */
function gatewayConnectOrigins(): string[] {
  const raw =
    process.env.GATEWAY_URL ||
    process.env.NEXT_PUBLIC_GATEWAY_BASE ||
    "http://127.0.0.1:18080";
  try {
    const u = new URL(raw);
    const host = u.host;
    const http = `${u.protocol}//${host}`;
    const ws = u.protocol === "https:" ? `wss://${host}` : `ws://${host}`;
    return [http, ws];
  } catch {
    return [];
  }
}

/** Council API/WS origins for browser CSP connect-src. */
function councilConnectOrigins(): string[] {
  const apiRaw = process.env.NEXT_PUBLIC_API_BASE || "http://127.0.0.1:8765";
  const wsRaw = process.env.NEXT_PUBLIC_WS_BASE || "";
  const origins = new Set<string>();
  const addHttpOrigin = (raw: string) => {
    try {
      const u = new URL(raw);
      origins.add(`${u.protocol}//${u.host}`);
      origins.add(
        u.protocol === "https:" ? `wss://${u.host}` : `ws://${u.host}`,
      );
    } catch {
      // Invalid runtime defaults should not widen CSP.
    }
  };

  // Runtime Settings can point the browser at a different local council
  // backend than the build-time default. Keep this explicit: no loopback
  // wildcards, only the local ports used by dev, smoke, and device lanes.
  for (const port of [8765, 8766, 8767, 8768]) {
    addHttpOrigin(`http://127.0.0.1:${port}`);
  }

  try {
    const api = new URL(apiRaw);
    origins.add(`${api.protocol}//${api.host}`);
    origins.add(
      api.protocol === "https:" ? `wss://${api.host}` : `ws://${api.host}`,
    );
  } catch {
    origins.add("http://127.0.0.1:8765");
    origins.add("ws://127.0.0.1:8765");
  }

  if (wsRaw.trim()) {
    try {
      const ws = new URL(wsRaw);
      origins.add(`${ws.protocol}//${ws.host}`);
    } catch {
      // Invalid runtime defaults should not widen CSP.
    }
  }

  return Array.from(origins);
}

/**
 * T17: narrow connect-src (no wildcard loopback). Explicit origins only (gatewayConnect + self).
 * Wildcard allowed local spoof/attack surface for operator holding signing material.
 * Auth material must not live in localStorage (use memory/ephemeral or httpOnly where possible).
 */
const LOOPBACK_CONNECT_ORIGINS: string[] = []; // T17: dropped * wildcards per audit. Use explicit GATEWAY_URL only.

const browserHeaders: NonNullable<NextConfig["headers"]> = async () => [
  {
    source: "/(.*)",
    headers: [
      {
        key: "Content-Security-Policy",
        value: [
          "default-src 'self'",
          `script-src 'self' 'unsafe-inline'${isDev ? " 'unsafe-eval'" : ""}`,
          "style-src 'self' 'unsafe-inline'",
          "img-src 'self' data: blob:",
          "font-src 'self' data:",
          [
            "connect-src 'self'",
            // T17: explicit (no wildcard); include configured council + gateway origins.
            ...councilConnectOrigins(),
            ...LOOPBACK_CONNECT_ORIGINS,
            ...gatewayConnectOrigins(),
          ].join(" "),
          "object-src 'none'",
          "base-uri 'none'",
          "form-action 'self'",
          "frame-ancestors 'none'",
        ].join("; "),
      },
      { key: "X-Content-Type-Options", value: "nosniff" },
      { key: "X-Frame-Options", value: "DENY" },
      { key: "Referrer-Policy", value: "no-referrer" },
      {
        key: "Permissions-Policy",
        value: "camera=(), microphone=(), geolocation=(), payment=()",
      },
    ],
  },
];

const nextConfig: NextConfig = {
  // The hosted Next server keeps its build manifests in memory. A concurrent
  // Tauri export must never replace those assets or the served HTML will point
  // at chunks that no longer exist, leaving remote browsers unhydrated.
  distDir: isTauriExport ? ".next-tauri" : ".next-hosted",
  allowedDevOrigins: allowedDevOrigins(),
  outputFileTracingRoot: path.join(process.cwd(), "../.."),
  poweredByHeader: false,
  reactStrictMode: true,
  typedRoutes: true,
  ...(isTauriExport
    ? {
        output: "export",
        images: { unoptimized: true },
      }
    : { headers: browserHeaders }),
};

export default nextConfig;
