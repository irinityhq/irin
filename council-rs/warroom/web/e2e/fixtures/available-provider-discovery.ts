import type { Page, Route } from "@playwright/test";

const CORS_HEADERS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET,OPTIONS",
  "access-control-allow-headers": "content-type,authorization",
};

const CABINET_PROVIDER_IDS = [
  "claude_code",
  "codex_cli",
  "gemini_agy",
  "grok_build",
  "grok_hermes",
  "nous",
  "nvidia",
] as const;

async function fulfillDiscovery(route: Route): Promise<void> {
  if (route.request().method() === "OPTIONS") {
    await route.fulfill({ status: 204, headers: CORS_HEADERS });
    return;
  }
  await route.fulfill({
    status: 200,
    contentType: "application/json",
    headers: CORS_HEADERS,
    body: JSON.stringify({
      providers: CABINET_PROVIDER_IDS.map((name) => ({
        name,
        label: name,
        family: "e2e",
        transport: name,
        available: true,
        gateway_supported: true,
        source: "e2e-fixture",
        env_hint: null,
        models: [],
      })),
      log: ["Deterministic E2E provider inventory"],
    }),
  });
}

/**
 * Tests that exercise cabinet launch or save behavior need a deterministic
 * provider inventory. Provider-empty and discovery-failure behavior belongs
 * to the dedicated core-surface state suite.
 */
export async function installAvailableProviderDiscovery(page: Page): Promise<void> {
  await page.route("**/api/discover", fulfillDiscovery);
}

async function fulfillHealth(route: Route): Promise<void> {
  if (route.request().method() === "OPTIONS") {
    await route.fulfill({ status: 204, headers: CORS_HEADERS });
    return;
  }
  await route.fulfill({
    status: 200,
    contentType: "application/json",
    headers: CORS_HEADERS,
    body: JSON.stringify({
      council_version: "e2e",
      stream_version: "1.0.0",
      providers_available: [...CABINET_PROVIDER_IDS],
      providers_missing: [],
      sessions_dir: "/tmp",
      index_path: "/tmp/index.json",
      index_exists: false,
    }),
  });
}

/**
 * Liveness/status companion to installAvailableProviderDiscovery. Convene
 * gating and cabinet runnability read /api/discover (see IdlePanel); the
 * health probe stays liveness-only and deliberately reports host CLI
 * transports as unavailable. Launch-flow specs still install this so header
 * status and health-driven surfaces see a deterministic payload, and so the
 * app never depends on a CI runner's real health probe.
 */
export async function installAvailableProviderHealth(page: Page): Promise<void> {
  await page.route("**/api/health", fulfillHealth);
}
