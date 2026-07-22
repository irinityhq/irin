import { test, expect, type Page, type Route } from "@playwright/test";
import { BACKEND } from "./support/ports";
import { installAvailableProviderDiscovery } from "./fixtures/available-provider-discovery";

const failuresByPage = new WeakMap<Page, string[]>();

function isOptionalGatewayConsoleNoise(text: string): boolean {
  if (text.includes("Failed to load resource: net::ERR_FAILED")) return true;
  // Chrome omits the URL in this message — favicon/optional assets in headless CI.
  if (
    text ===
    "Failed to load resource: the server responded with a status of 404 (Not Found)"
  ) {
    return true;
  }
  return (
    /127\.0\.0\.1:18080|127\.0\.0\.1:8080/.test(text) &&
    (/Access-Control-Allow-Origin|ERR_FAILED|404/.test(text) ||
      text.includes("Failed to load resource"))
  );
}

test.beforeEach(async ({ page }) => {
  await installAvailableProviderDiscovery(page);
  const failures: string[] = [];
  failuresByPage.set(page, failures);
  page.on("console", (msg) => {
    if (msg.type() === "error") {
      const text = msg.text();
      if (isOptionalGatewayConsoleNoise(text)) return;
      failures.push(`console error: ${text}`);
    }
  });
  page.on("pageerror", (err) => failures.push(`page error: ${err.message}`));
  page.on("requestfailed", (request) => {
    const url = request.url();
    const errorText = request.failure()?.errorText ?? "";
    // Librarian Stop / WS close aborts in-flight work on purpose.
    if (errorText.includes("ERR_ABORTED")) return;
    if (url.startsWith(BACKEND)) {
      failures.push(`request failed: ${url} ${errorText}`.trim());
    }
  });
});

test.afterEach(async ({ page }) => {
  expect(failuresByPage.get(page) ?? []).toEqual([]);
});

const CORS_HEADERS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET,POST,PATCH,DELETE,OPTIONS",
  "access-control-allow-headers": "content-type,authorization,if-match",
};

async function fulfillJson(route: Route, data: unknown, status = 200) {
  await route.fulfill({
    status,
    contentType: "application/json",
    headers: CORS_HEADERS,
    body: JSON.stringify(data),
  });
}

async function handlePreflight(route: Route): Promise<boolean> {
  if (route.request().method() !== "OPTIONS") return false;
  await route.fulfill({ status: 204, headers: CORS_HEADERS });
  return true;
}

/**
 * Rewrites the deliberate WS start frame to smoke_only so the backend smoke
 * shim drives the synthetic seat + chunk loop (N01) — zero provider spend.
 */
async function installSmokeOnlyWebSocketShim(page: Page) {
  await page.addInitScript(() => {
    const Native = window.WebSocket;
    class SmokeWebSocket extends Native {
      send(data: Parameters<WebSocket["send"]>[0]) {
        if (typeof data === "string") {
          try {
            const parsed = JSON.parse(data) as {
              type?: string;
              payload?: Record<string, unknown>;
            };
            if (parsed.type === "start" && parsed.payload) {
              parsed.payload.frame_check = false;
              parsed.payload.smoke_only = true;
              data = JSON.stringify(parsed);
            }
          } catch {
            /* non-JSON frames pass through */
          }
        }
        return super.send(data);
      }
    }
    window.WebSocket = SmokeWebSocket;
  });
}

test.describe("Phase 9 N01 — seat_chunk token streaming (real WS smoke shim)", () => {
  test("smoke shim emits seat_chunk frames before seat_complete", async ({ page }) => {
    await installSmokeOnlyWebSocketShim(page);
    await page.goto("/");
    await expect(page.getByTestId("cabinet-chip").first()).toBeVisible({
      timeout: 10_000,
    });

    const topicInput = page.getByRole("textbox", { name: /proceeding statement/i });
    await topicInput.fill("N01 streaming round-trip");

    const wsPromise = page.waitForEvent("websocket", {
      predicate: (ws) => ws.url().includes("/ws/deliberate"),
      timeout: 10_000,
    });
    await page.getByRole("button", { name: /convene/i }).click();
    const ws = await wsPromise;

    // Collect frames until done; assert at least one seat_chunk precedes a
    // seat_complete for the same seat.
    const types: string[] = [];
    let sawChunk = false;
    let sawComplete = false;
    const deadline = Date.now() + 20_000;
    while (Date.now() < deadline && !sawComplete) {
      const frame = await ws
        .waitForEvent("framereceived", { timeout: 20_000 })
        .catch(() => null);
      if (!frame) break;
      const data = JSON.parse(frame.payload as string) as { type: string };
      types.push(data.type);
      if (data.type === "seat_chunk") sawChunk = true;
      if (data.type === "seat_complete") sawComplete = true;
      if (data.type === "done") break;
    }

    expect(types, `expected seat_complete; saw ${types.join(",")}`).toContain("seat_complete");
    expect(sawChunk, `expected a seat_chunk before seat_complete; saw ${types.join(",")}`).toBe(true);
  });
});

test.describe("Phase 9 N03 — clusters tile (route-mocked)", () => {
  test("clusters tile renders and filters the session list client-side", async ({ page }) => {
    await page.route("**/api/clusters", async (route) => {
      if (await handlePreflight(route)) return;
      await fulfillJson(route, {
        clusters: [
          { id: 0, size: 2, top_terms: ["risk", "ship"], session_ids: ["s-a", "s-b"] },
          { id: 1, size: 1, top_terms: ["cost"], session_ids: ["s-c"] },
        ],
        method: "kmeans",
        k: 2,
        n_sessions: 3,
        generated_at: "2026-06-06T00:00:00Z",
      });
    });
    await page.route("**/api/sessions**", async (route) => {
      if (await handlePreflight(route)) return;
      await fulfillJson(route, {
        sessions: [
          mockIndexEntry("s-a", "Risk of shipping early"),
          mockIndexEntry("s-b", "Ship behind a flag"),
          mockIndexEntry("s-c", "Cost overrun analysis"),
        ],
      });
    });

    await page.goto("/");
    await page.getByRole("button", { name: "History", exact: true }).click();

    const tile = page.getByTestId("cluster-tile");
    await expect(tile).toBeVisible({ timeout: 10_000 });
    await tile.getByRole("button", { name: /Themes/i }).click();
    const chips = page.getByTestId("cluster-chip");
    await expect(chips).toHaveCount(2);

    // Click the largest cluster (risk/ship, size 2) → only its 2 sessions show.
    // Scope to the session list — theme chips carry sample-topic text too.
    const sessionList = page.locator(".cg-rail-sessions");
    await chips.first().click();
    await expect(sessionList.getByText("Cost overrun analysis")).toHaveCount(0);
    await expect(sessionList.getByText("Risk of shipping early")).toBeVisible();

    // Clear restores the full list.
    await page.getByTestId("cluster-clear").click();
    await expect(sessionList.getByText("Cost overrun analysis")).toBeVisible();
  });
});

test.describe("Phase 9 N06 — PDF export (download event, route-mocked)", () => {
  test("Export PDF triggers a browser download", async ({ page }) => {
    await page.route("**/api/sessions/**/export/pdf", async (route) => {
      if (await handlePreflight(route)) return;
      await route.fulfill({
        status: 200,
        contentType: "application/pdf",
        headers: {
          ...CORS_HEADERS,
          "content-disposition": 'attachment; filename="council_s-a.pdf"',
        },
        body: "%PDF-1.4\n%mock\n",
      });
    });
    await page.route("**/api/sessions**", async (route) => {
      if (await handlePreflight(route)) return;
      const url = route.request().url();
      if (/\/api\/sessions\/s-a(\?|$)/.test(url)) {
        await fulfillJson(route, mockDetail("s-a"));
        return;
      }
      await fulfillJson(route, { sessions: [mockIndexEntry("s-a", "Exportable session")] });
    });
    await page.route("**/api/sessions/s-a/lineage", async (route) => {
      if (await handlePreflight(route)) return;
      await fulfillJson(route, { session_id: "s-a", parent: null, children: [] });
    });
    await page.route("**/api/clusters", async (route) => {
      if (await handlePreflight(route)) return;
      await fulfillJson(route, { clusters: [], method: "kmeans", k: 0, n_sessions: 0, generated_at: "" });
    });

    await page.goto("/");
    await page.getByRole("button", { name: "History", exact: true }).click();
    await page
      .getByTestId("session-list")
      .getByRole("button", { name: /Exportable session/ })
      .click();

    const exportBtn = page.getByTestId("session-export-pdf");
    await expect(exportBtn).toBeVisible({ timeout: 10_000 });
    const downloadPromise = page.waitForEvent("download", { timeout: 10_000 });
    await exportBtn.click();
    const download = await downloadPromise;
    expect(download.suggestedFilename()).toContain(".pdf");
  });
});

function mockIndexEntry(id: string, topic: string) {
  return {
    id,
    ts: "2026-06-06T00:00:00Z",
    topic,
    keywords: [],
    ruling_digest: "",
    confidence: "HIGH",
    cabinet: "warroom",
    convergence: 0.9,
    mode: "normal",
    seat_count: 3,
    rounds: 2,
    synthesis_model: "claude",
    version: "v2",
  };
}

function mockDetail(id: string) {
  return {
    session_id: id,
    topic: "Exportable session",
    cabinet_name: "warroom",
    rounds: [],
    synthesis: "The council ruled.",
    synthesis_model: "claude",
    total_tokens: 100,
    total_latency_ms: 1000,
    total_cost_usd: 0.01,
    mode: "normal",
    precedent_ids: [],
    timestamp: "2026-06-06T00:00:00Z",
  };
}
