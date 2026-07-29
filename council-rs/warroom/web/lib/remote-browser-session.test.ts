/**
 * Remote/Tailscale-style same-origin browser session: authenticates through
 * browser runtime-config (not a native iOS wrapper), exercises REST +
 * WebSocket, and fails closed without valid auth.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const TAILNET_ORIGIN = "https://macbook.example.ts.net:8443";
const TAILNET_PAGE = `${TAILNET_ORIGIN}/`;
const AUTH_TOKEN = "test-remote-browser-token";

type FetchCall = {
  url: string;
  headers: Record<string, string>;
};

class MockWebSocket {
  static instances: MockWebSocket[] = [];
  static acceptNext = true;

  url: string;
  protocols: string[] | undefined;
  protocol = "";
  onopen: ((ev?: Event) => void) | null = null;
  onerror: ((ev?: Event) => void) | null = null;
  onclose: ((ev: { code: number }) => void) | null = null;
  readyState = 0;

  constructor(url: string, protocols?: string | string[]) {
    this.url = url;
    this.protocols = protocols
      ? Array.isArray(protocols)
        ? protocols
        : [protocols]
      : undefined;
    MockWebSocket.instances.push(this);
    queueMicrotask(() => {
      const tokenProto = this.protocols?.find((p) => p.startsWith("token."));
      const token = tokenProto?.slice("token.".length) ?? "";
      if (MockWebSocket.acceptNext && token === AUTH_TOKEN) {
        this.protocol = "council";
        this.readyState = 1;
        this.onopen?.(undefined as unknown as Event);
        return;
      }
      this.readyState = 3;
      this.onerror?.(undefined as unknown as Event);
      this.onclose?.({ code: 1008 });
    });
  }

  close(): void {
    this.readyState = 3;
  }
}

function installBrowser(
  pageUrl: string,
  initialStorage: Record<string, string> = {},
): Map<string, string> {
  const store = new Map(Object.entries(initialStorage));
  const page = new URL(pageUrl);
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => {
      store.set(key, value);
    },
    removeItem: (key: string) => {
      store.delete(key);
    },
    clear: () => store.clear(),
  });
  vi.stubGlobal("window", {
    location: {
      href: pageUrl,
      origin: page.origin,
      host: page.host,
      protocol: page.protocol,
    },
    dispatchEvent: () => true,
    addEventListener: () => {},
    __WARROOM_NATIVE_CONFIG__: undefined,
  });
  vi.stubGlobal("WebSocket", MockWebSocket);
  return store;
}

async function loadSessionModules() {
  const runtime = await import("./runtime-config");
  const { api } = await import("./api");
  const { probeWsUpgrade } = await import("./ws-probe");
  return { ...runtime, api, probeWsUpgrade };
}

describe("remote Tailscale-style same-origin browser session", () => {
  const fetchCalls: FetchCall[] = [];

  beforeEach(() => {
    vi.resetModules();
    vi.unstubAllGlobals();
    MockWebSocket.instances = [];
    MockWebSocket.acceptNext = true;
    fetchCalls.length = 0;
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("authenticates via browser config and exercises REST plus WebSocket on same origin", async () => {
    installBrowser(TAILNET_PAGE);
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const headers = Object.fromEntries(
          new Headers(init?.headers).entries(),
        );
        fetchCalls.push({ url, headers });
        if (!headers.authorization || headers.authorization !== `Bearer ${AUTH_TOKEN}`) {
          return new Response(JSON.stringify({ error: "unauthorized" }), {
            status: 401,
            statusText: "Unauthorized",
          });
        }
        return new Response(
          JSON.stringify({
            council_version: "test",
            stream_version: "1",
            providers_missing: [],
            build_sha: "abc123def456",
            build_dirty: false,
          }),
          { status: 200, statusText: "OK" },
        );
      }),
    );

    const {
      saveRuntimeConfig,
      getApiBase,
      getWsBase,
      getAuthToken,
      api,
      probeWsUpgrade,
    } = await loadSessionModules();

    // Browser configuration path: operator pastes token in Settings (localStorage).
    const saved = await saveRuntimeConfig({ authToken: AUTH_TOKEN });
    expect(saved.apiBase).toBe(TAILNET_ORIGIN);
    expect(saved.wsBase).toBe("wss://macbook.example.ts.net:8443");
    expect(saved.gatewayBase).toBe(TAILNET_ORIGIN);
    expect(getApiBase()).toBe(TAILNET_ORIGIN);
    expect(getWsBase()).toBe("wss://macbook.example.ts.net:8443");
    expect(getAuthToken()).toBe(AUTH_TOKEN);

    const health = await api.health();
    expect(health.council_version).toBe("test");
    expect(fetchCalls).toHaveLength(1);
    expect(fetchCalls[0].url).toBe(`${TAILNET_ORIGIN}/api/health`);
    expect(fetchCalls[0].headers.authorization).toBe(`Bearer ${AUTH_TOKEN}`);

    const ws = await probeWsUpgrade(500);
    expect(ws.ok).toBe(true);
    expect(ws.detail).toMatch(/WebSocket upgrade OK/);
    expect(MockWebSocket.instances).toHaveLength(1);
    expect(MockWebSocket.instances[0].url).toBe(
      "wss://macbook.example.ts.net:8443/ws/deliberate",
    );
    expect(MockWebSocket.instances[0].protocols).toEqual([
      "council",
      `token.${AUTH_TOKEN}`,
    ]);
  });

  it("fails closed for REST and WebSocket when auth is missing or invalid", async () => {
    installBrowser(TAILNET_PAGE);
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const headers = Object.fromEntries(
          new Headers(init?.headers).entries(),
        );
        fetchCalls.push({ url, headers });
        return new Response(JSON.stringify({ error: "unauthorized" }), {
          status: 401,
          statusText: "Unauthorized",
        });
      }),
    );

    const { saveRuntimeConfig, getAuthToken, api, probeWsUpgrade } =
      await loadSessionModules();

    // Missing auth: remote same-origin defaults apply, no token configured.
    await saveRuntimeConfig({ authToken: "" });
    expect(getAuthToken()).toBe("");

    await expect(api.health()).rejects.toThrow(/401/);
    expect(fetchCalls[0].headers.authorization).toBeUndefined();

    MockWebSocket.acceptNext = false;
    const missingWs = await probeWsUpgrade(200);
    expect(missingWs.ok).toBe(false);
    expect(missingWs.detail).toMatch(/token|auth|failed|401/i);

    // Invalid auth: wrong token still rejected.
    fetchCalls.length = 0;
    MockWebSocket.instances = [];
    await saveRuntimeConfig({ authToken: "wrong-token" });
    expect(getAuthToken()).toBe("wrong-token");

    await expect(api.health()).rejects.toThrow(/401/);
    expect(fetchCalls[0].headers.authorization).toBe("Bearer wrong-token");

    const invalidWs = await probeWsUpgrade(200);
    expect(invalidWs.ok).toBe(false);
    expect(MockWebSocket.instances[0].protocols).toEqual([
      "council",
      "token.wrong-token",
    ]);
  });
});
