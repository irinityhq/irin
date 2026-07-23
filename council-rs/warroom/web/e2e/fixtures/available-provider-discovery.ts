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
 * Convene gating reads provider availability from /api/health, not
 * /api/discover. Launch-flow tests must not depend on host CLIs: CI runners
 * have none, so an unmocked health probe disables the Convene button.
 */
export async function installAvailableProviderHealth(page: Page): Promise<void> {
  await page.route("**/api/health", fulfillHealth);
}
