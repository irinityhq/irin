import { test, expect, type Page, type Route } from "@playwright/test";
import { BACKEND } from "./support/ports";
import { installAvailableProviderDiscovery } from "./fixtures/available-provider-discovery";

const failuresByPage = new WeakMap<Page, string[]>();

/** Optional Gateway fetches from the browser often fail CORS in local smoke — ignore. */
function isOptionalGatewayConsoleNoise(text: string): boolean {
  if (text.includes("Failed to load resource: net::ERR_FAILED")) {
    return true;
  }
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
  page.on("pageerror", (err) => {
    failures.push(`page error: ${err.message}`);
  });
  page.on("requestfailed", (request) => {
    const url = request.url();
    const errorText = request.failure()?.errorText ?? "";
    // feature contract: the librarian Stop button aborts an in-flight POST on purpose.
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

/** Fulfill JSON with permissive CORS so mocks behave for cross-origin fetch. */
async function fulfillJson(route: Route, data: unknown, status = 200) {
  await route.fulfill({
    status,
    contentType: "application/json",
    headers: CORS_HEADERS,
    body: JSON.stringify(data),
  });
}

/** True (and handled) when the route is a CORS preflight. */
async function handlePreflight(route: Route): Promise<boolean> {
  if (route.request().method() !== "OPTIONS") return false;
  await route.fulfill({ status: 204, headers: CORS_HEADERS });
  return true;
}

const IMPORT_YAML = [
  "name: Imported E2E Cabinet",
  "description: phase7 import fixture",
  "rounds: 2",
  "seats:",
  "  - name: Seat A",
  "    provider: grok",
  "    model: grok-4",
  "    system: Argue.",
  "chair:",
  "  name: Chair",
  "  provider: claude",
  "  model: claude-opus-4",
  "",
].join("\n");

test.describe("feature contract cabinet save + import (route-mocked — never writes real cabinets/)", () => {
  test("save-to-council posts {name, yaml} and surfaces the saved path", async ({ page }) => {
    const captured: { body: { name?: string; yaml?: string } | null } = {
      body: null,
    };
    await page.route("**/api/cabinets/save", async (route) => {
      if (await handlePreflight(route)) return;
      const body = route.request().postDataJSON() as { name?: string; yaml?: string };
      captured.body = body;
      await fulfillJson(route, {
        ok: true,
        name: body.name,
        path: `cabinets/${body.name}.yaml`,
      });
    });

    await page.goto("/");
    // Wait until cabinets are loaded so CabinetEditor mounts with a selection.
    await expect(page.getByTestId("cabinet-chip").first()).toBeVisible({
      timeout: 10_000,
    });
    await page.getByRole("button", { name: "Cabinets", exact: true }).click();

    const nameInput = page.getByTestId("cabinet-save-name");
    await expect(nameInput).toBeVisible({ timeout: 10_000 });
    await nameInput.fill("e2e-saved-cabinet");
    await page.getByTestId("cabinet-save-submit").click();

    await expect(page.getByTestId("cabinet-save-success")).toContainText(
      "cabinets/e2e-saved-cabinet.yaml",
      { timeout: 10_000 },
    );
    expect(captured.body?.name).toBe("e2e-saved-cabinet");
    // Serialized draft must be Rust-Cabinet-shaped YAML.
    const yaml = String(captured.body?.yaml ?? "");
    expect(yaml).toContain("seats:");
    expect(yaml).toContain("chair:");
    expect(yaml).toContain("rounds:");
  });

  test("invalid registry name is rejected client-side with NO request", async ({ page }) => {
    let saveRequests = 0;
    await page.route("**/api/cabinets/save", async (route) => {
      if (await handlePreflight(route)) return;
      saveRequests += 1;
      await fulfillJson(route, { error: "should never be reached" }, 400);
    });

    await page.goto("/");
    await expect(page.getByTestId("cabinet-chip").first()).toBeVisible({
      timeout: 10_000,
    });
    await page.getByRole("button", { name: "Cabinets", exact: true }).click();

    const nameInput = page.getByTestId("cabinet-save-name");
    await expect(nameInput).toBeVisible({ timeout: 10_000 });
    await nameInput.fill("Bad Name!");
    await page.getByTestId("cabinet-save-submit").click();

    await expect(page.getByTestId("cabinet-save-error")).toContainText(
      /\^\[a-z0-9\]/,
    );
    expect(saveRequests).toBe(0);
  });

  test("Load YAML imports a file, saves the RAW text, and offers Run", async ({ page }) => {
    const captured: { body: { name?: string; yaml?: string } | null } = {
      body: null,
    };
    await page.route("**/api/cabinets/save", async (route) => {
      if (await handlePreflight(route)) return;
      const body = route.request().postDataJSON() as { name?: string; yaml?: string };
      captured.body = body;
      await fulfillJson(route, {
        ok: true,
        name: body.name,
        path: `cabinets/${body.name}.yaml`,
      });
    });

    await page.goto("/");
    await expect(page.getByTestId("cabinet-chip").first()).toBeVisible({
      timeout: 10_000,
    });
    await page.getByRole("button", { name: "Cabinets", exact: true }).click();
    await expect(page.getByTestId("cabinet-import-button")).toBeVisible();

    await page.getByTestId("cabinet-import-input").setInputFiles({
      name: "imported-e2e.yaml",
      mimeType: "application/yaml",
      buffer: Buffer.from(IMPORT_YAML, "utf-8"),
    });

    const panel = page.getByTestId("cabinet-import-panel");
    await expect(panel).toBeVisible();
    // Lint passes (all required keys present) — no warning shown.
    await expect(page.getByTestId("cabinet-import-lint")).toHaveCount(0);
    // Name suggested from the file stem.
    await expect(page.getByTestId("cabinet-import-name")).toHaveValue(
      "imported-e2e",
    );

    await page.getByTestId("cabinet-import-save").click();
    await expect(page.getByTestId("cabinet-import-run")).toBeVisible({
      timeout: 10_000,
    });
    expect(captured.body?.name).toBe("imported-e2e");
    // feature contract pinned decision: the raw YAML is POSTed untouched.
    expect(captured.body?.yaml).toBe(IMPORT_YAML);

    // Run deliberation navigates to the Deliberate panel.
    await page.getByTestId("cabinet-import-run").click();
    await expect(
      page.getByRole("textbox", { name: /proceeding statement/i }),
    ).toBeVisible({ timeout: 10_000 });
  });

  test("imported YAML missing required keys shows the lint warning", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("cabinet-chip").first()).toBeVisible({
      timeout: 10_000,
    });
    await page.getByRole("button", { name: "Cabinets", exact: true }).click();
    await page.getByTestId("cabinet-import-input").setInputFiles({
      name: "broken.yaml",
      mimeType: "application/yaml",
      buffer: Buffer.from("name: Broken\nrounds: 2\n", "utf-8"),
    });
    await expect(page.getByTestId("cabinet-import-lint")).toContainText(
      /seats, chair/,
    );
  });
});

test.describe("feature contract / R20 librarian Stop button (mocked slow ask — no live spend)", () => {
  test("Stop aborts the in-flight ask and the composer recovers", async ({ page }) => {
    const chat = {
      id: "chat-e2e",
      cabinet: "research-default",
      title: "phase7 stop test",
      created_at: "2026-06-06T12:00:00Z",
      updated_at: "2026-06-06T12:00:00Z",
      schema_version: 1,
      messages: [],
    };

    // R20 made the WS the default in-flight path. Mock the librarian WS so the
    // ask stays in-flight: accept the upgrade and emit only {type:"ask_started"}
    // (never complete). busy stays true → the Stop button renders. Pressing Stop
    // closes the WS (the real R20 cancel path); the server-close handler in
    // LibrarianView recovers state. We do NOT touch the real backend.
    let wsClosedByClient = false;
    await page.routeWebSocket("**/ws/librarian/**", (ws) => {
      // Do not connect to the real server — fully synthesize the librarian WS.
      ws.onMessage((message) => {
        let parsed: { type?: string } | null = null;
        try {
          parsed = JSON.parse(String(message)) as { type?: string };
        } catch {
          parsed = null;
        }
        if (parsed?.type === "ask") {
          // Stream started, then hang — mirrors a slow upstream ask.
          ws.send(JSON.stringify({ type: "ask_started" }));
        }
      });
      ws.onClose(() => {
        // Stop click closes the socket — that's the R20 cancel signal.
        wsClosedByClient = true;
      });
    });

    await page.route("**/api/librarian/health", async (route) => {
      if (await handlePreflight(route)) return;
      await fulfillJson(route, { state: "online", model: "mock-librarian" });
    });
    await page.route("**/api/librarian/cabinets", async (route) => {
      if (await handlePreflight(route)) return;
      await fulfillJson(route, { cabinets: [{ name: "research-default" }] });
    });
    await page.route("**/api/librarian/chats", async (route) => {
      if (await handlePreflight(route)) return;
      if (route.request().method() === "POST") {
        await fulfillJson(route, { id: chat.id });
      } else {
        await fulfillJson(route, { chats: [] });
      }
    });
    await page.route("**/api/librarian/chats/chat-e2e", async (route) => {
      if (await handlePreflight(route)) return;
      // recoverAfterStop() refetches the chat after Stop — return it unchanged.
      await fulfillJson(route, chat);
    });
    await page.route("**/api/librarian/chats/chat-e2e/asks", async (route) => {
      if (await handlePreflight(route)) return;
      // POST is the fallback only; the WS path is the default and should win.
      // If reached, hang past the Stop click so this never resolves first.
      await new Promise((resolve) => setTimeout(resolve, 25_000));
      try {
        await route.abort();
      } catch {
        // Already aborted by the page — expected.
      }
    });

    await page.goto("/");
    await page.getByRole("button", { name: "Librarian", exact: true }).click();
    await expect(page.getByTestId("librarian-shell")).toBeVisible({
      timeout: 10_000,
    });

    const composer = page.getByPlaceholder(/ask the librarian/i);
    await expect(composer).toBeEnabled();
    await composer.fill("What does the vault say about drift?");
    await page.getByTestId("librarian-send").click();

    // In-flight: Send is replaced by Stop, composer disabled.
    const stop = page.getByTestId("librarian-stop");
    await expect(stop).toBeVisible({ timeout: 10_000 });
    await expect(composer).toBeDisabled();

    await stop.click();

    // Recovery: Stop gone, Send back, composer re-enabled, and the abort is
    // NOT surfaced as an error banner.
    await expect(page.getByTestId("librarian-send")).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.getByTestId("librarian-stop")).toHaveCount(0);
    await expect(composer).toBeEnabled();
    await expect(page.getByText(/AbortError/)).toHaveCount(0);
    // R20: Stop closed the WS — the server treats close as cancel.
    await expect.poll(() => wsClosedByClient, { timeout: 5_000 }).toBe(true);
  });
});

test.describe("feature contract synthesis-only toggle (mocked session detail)", () => {
  test("toggle hides round cards and keeps the ruling", async ({ page }) => {
    const entry = {
      id: "e2e-1",
      ts: "2026-06-06T12:00:00Z",
      topic: "Synthesis-only toggle fixture",
      keywords: [],
      ruling_digest: "ship it",
      confidence: "high",
      cabinet: "standard",
      convergence: 0.9,
      mode: "normal",
      seat_count: 1,
      rounds: 1,
      synthesis_model: "claude-opus-4",
      version: "v2",
    };
    const detail = {
      session_id: "e2e-1",
      topic: "Synthesis-only toggle fixture",
      cabinet_name: "standard",
      rounds: [
        {
          round_num: 1,
          convergence_score: 0.9,
          converged: true,
          responses: [
            {
              seat_name: "Seat A",
              provider: "grok",
              model: "grok-4",
              text: "Round one seat text",
              round_num: 1,
              latency_ms: 1200,
              tokens_in: 10,
              tokens_out: 20,
              cached_in: 0,
              cost_usd: 0.001,
            },
          ],
        },
      ],
      synthesis: "## Ruling\n\nShip the toggle.",
      synthesis_model: "claude-opus-4",
      total_tokens: 30,
      total_latency_ms: 1200,
      total_cost_usd: 0.001,
      mode: "normal",
      precedent_ids: [],
      timestamp: "2026-06-06T12:00:00Z",
    };

    await page.route("**/api/sessions?limit=*", async (route) => {
      if (await handlePreflight(route)) return;
      await fulfillJson(route, { sessions: [entry] });
    });
    await page.route("**/api/sessions/e2e-1", async (route) => {
      if (await handlePreflight(route)) return;
      await fulfillJson(route, detail);
    });
    await page.route("**/api/sessions/e2e-1/lineage", async (route) => {
      if (await handlePreflight(route)) return;
      await fulfillJson(route, { session_id: "e2e-1", parent: null, children: [] });
    });

    await page.goto("/");
    await page.getByRole("button", { name: "History", exact: true }).click();
    await page
      .getByTestId("session-list")
      .getByRole("button", { name: /Synthesis-only toggle fixture/ })
      .click();

    // "Round 1" appears in both the phase rail tile and the ledger header.
    const phaseRail = page.getByTestId("phase-rail");
    await expect(phaseRail.getByText("Round 1", { exact: true })).toBeVisible({
      timeout: 10_000,
    });
    const ruling = page.getByRole("complementary", { name: /council ruling/i });
    await expect(ruling).toBeVisible();

    const toggle = page.getByTestId("synthesis-only-toggle");
    await expect(toggle).toContainText(/ruling only/i);
    await toggle.click();

    // Round cards hidden; ruling (export target) still on screen.
    await expect(phaseRail.getByText("Round 1", { exact: true })).toHaveCount(0);
    await expect(ruling).toBeVisible();
    await expect(ruling).toContainText("Ship the toggle.");
    await expect(toggle).toContainText(/full record/i);

    // Toggling back restores the transcript.
    await toggle.click();
    await expect(phaseRail.getByText("Round 1", { exact: true })).toBeVisible();
  });
});

test.describe("feature contract runtime ownership (browser mode)", () => {
  test("Settings hides debug sidecar controls and explains external ownership", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    await expect(page.getByTestId("settings-council-root")).toHaveCount(0);
    await expect(page.getByTestId("settings-gateway-mode")).toHaveCount(0);
    const ownership = page.getByTestId("settings-installed-runtime");
    await expect(ownership).toBeVisible();
    await expect(ownership).toContainText(/managed outside this page/i);
  });
});
