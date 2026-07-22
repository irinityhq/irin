import { describe, expect, it, vi } from "vitest";
import nextConfig from "../next.config";

async function cspHeader(): Promise<string> {
  if (!nextConfig.headers) throw new Error("headers missing");
  const groups = await nextConfig.headers();
  const header = groups
    .flatMap((group) => group.headers)
    .find((entry) => entry.key === "Content-Security-Policy");
  if (!header) throw new Error("CSP header missing");
  return header.value;
}

describe("next CSP", () => {
  it("keeps the Tauri export build out of the hosted server directory", () => {
    expect(nextConfig.distDir).toBe(".next-hosted");
  });

  it("uses a separate build directory for the Tauri export", async () => {
    const previous = process.env.WARROOM_TAURI_EXPORT;
    process.env.WARROOM_TAURI_EXPORT = "1";
    vi.resetModules();
    try {
      const tauriConfig = (await import("../next.config")).default;
      expect(tauriConfig.distDir).toBe(".next-tauri");
    } finally {
      if (previous === undefined) delete process.env.WARROOM_TAURI_EXPORT;
      else process.env.WARROOM_TAURI_EXPORT = previous;
      vi.resetModules();
    }
  });

  it("allows explicit local council runtime ports without loopback wildcards", async () => {
    const prevApi = process.env.NEXT_PUBLIC_API_BASE;
    const prevWs = process.env.NEXT_PUBLIC_WS_BASE;
    process.env.NEXT_PUBLIC_API_BASE = "http://127.0.0.1:8768";
    process.env.NEXT_PUBLIC_WS_BASE = "ws://127.0.0.1:8768";

    try {
      const csp = await cspHeader();
      expect(csp).toContain("http://127.0.0.1:8765");
      expect(csp).toContain("ws://127.0.0.1:8765");
      expect(csp).toContain("http://127.0.0.1:8768");
      expect(csp).toContain("ws://127.0.0.1:8768");
      expect(csp).not.toContain("http://127.0.0.1:*");
      expect(csp).not.toContain("ws://127.0.0.1:*");
    } finally {
      if (prevApi === undefined) delete process.env.NEXT_PUBLIC_API_BASE;
      else process.env.NEXT_PUBLIC_API_BASE = prevApi;
      if (prevWs === undefined) delete process.env.NEXT_PUBLIC_WS_BASE;
      else process.env.NEXT_PUBLIC_WS_BASE = prevWs;
    }
  });
});
