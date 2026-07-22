import { test, expect, type Page } from "@playwright/test";
import { BACKEND, WEB_ORIGIN_RE, WS_DELIBERATE_URL } from "./support/ports";
import { installAvailableProviderDiscovery } from "./fixtures/available-provider-discovery";

const failuresByPage = new WeakMap<Page, string[]>();

test.beforeEach(async ({ page }) => {
  await installAvailableProviderDiscovery(page);
  const failures: string[] = [];
  failuresByPage.set(page, failures);

  page.on("console", (msg) => {
    if (msg.type() === "error") {
      const text = msg.text();
      // Chrome omits the URL from this generic optional-resource message.
      if (
        text ===
        "Failed to load resource: the server responded with a status of 404 (Not Found)"
      ) {
        return;
      }
      // Resource errors from other live local services (xmcp :8000, gateway
      // :8080) are environment noise — only the council backend and the app
      // origin are signals here.
      const src = msg.location()?.url ?? "";
      if (
        src &&
        text.startsWith("Failed to load resource") &&
        !src.startsWith(BACKEND) &&
        !WEB_ORIGIN_RE.test(src)
      ) {
        return;
      }
      failures.push(`console error: ${text}`);
    }
  });
  page.on("pageerror", (err) => {
    failures.push(`page error: ${err.message}`);
  });
  page.on("requestfailed", (request) => {
    const url = request.url();
    if (url.startsWith(BACKEND)) {
      failures.push(
        `request failed: ${url} ${request.failure()?.errorText ?? ""}`.trim(),
      );
    }
  });
});

test.afterEach(async ({ page }) => {
  expect(failuresByPage.get(page) ?? []).toEqual([]);
});

async function installSmokeOnlyWebSocketShim(page: Page) {
  await page.addInitScript(() => {
    const NativeWebSocket = window.WebSocket;
    class SmokeWebSocket extends NativeWebSocket {
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
            // Non-JSON WebSocket frames pass through unchanged.
          }
        }
        return super.send(data);
      }
    }
    window.WebSocket = SmokeWebSocket;
  });
}

/**
 * Generic navigation checks should not depend on a live Gateway. The governed
 * surface suite separately pins populated, empty, and 503 behavior.
 */
async function installGovernanceShellFixtures(page: Page) {
  await page.route("**/api/governance/outbox", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        canary_tenant: "ci-shell",
        directives: [],
        next_cursor: null,
      }),
    }),
  );
  await page.route("**/api/governance/watch", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        tenant: "ci-shell",
        canary_tenant: "ci-shell",
        action_production_armed: false,
        sentinels: [],
        temperature: {
          value: 0,
          level: "cold",
          fires_last_hour: 0,
          fires_last_24h: 0,
        },
        recent_fires: [],
        budget: { spend_today_usd: 0, spend_cap_usd: 0 },
        degradation: {
          audit_infra_errors_total: 0,
          persist_failures_total: 0,
          pending_records: 0,
          pending_retry_failures_total: 0,
          pending_oldest_age_ms: 0,
          lease_expired_during_deliberation_total: 0,
          duplicate_charge_alarms_total: 0,
          directive_ttl_expired_total: 0,
          directive_max_delivery_exceeded_total: 0,
          directive_clock_skew_rejected_total: 0,
          recon_divergence_total: 0,
          recon_cap_breach_total: 0,
          settle_ceiling_overshoot_total: 0,
          spend_gauge_read_failures_total: 0,
          kill_switch_drain_timeout_total: 0,
        },
      }),
    }),
  );
}

test.describe("War Room smoke", () => {
  test("renders home page and fetches health from backend", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator("body")).toBeVisible();
    await expect(
      page
        .getByRole("banner")
        .getByText("COUNCIL · WAR ROOM", { exact: true }),
    ).toBeVisible();
    await expect(page.getByTestId("warroom-health-status")).toContainText(
      /gen .*stream/i,
      { timeout: 15_000 },
    );
    await expect(page.getByTestId("backend-connection-error")).toHaveCount(0);
    await expect(page.getByTestId("cabinet-chip").first()).toBeVisible({
      timeout: 5000,
    });
    expect(await page.getByTestId("cabinet-chip").count()).toBeGreaterThan(0);
  });

  test("topic input enables Convene button", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("cabinet-chip").first()).toBeVisible({
      timeout: 10_000,
    });
    const topicInput = page.getByRole("textbox", {
      name: /proceeding statement/i,
    });
    await expect(topicInput).toBeVisible();
    const conveneBtn = page.getByRole("button", { name: /convene/i });
    await expect(conveneBtn).toBeDisabled();
    await topicInput.fill("Should we invest in quantum computing?");
    await expect(conveneBtn).toBeEnabled();
  });

  test("keeps Mapmaker task independent from the proceeding matter", async ({
    page,
  }) => {
    await page.goto("/");

    const matter = page.getByRole("textbox", {
      name: /proceeding statement/i,
    });
    const mapTask = page.getByTestId("mapmaker-task");

    await expect(mapTask).toHaveValue("");
    await matter.fill("What does a test mean?");
    await expect(mapTask).toHaveValue("");

    await mapTask.fill(
      "Trace the test runner and identify the smallest coverage gap.",
    );
    await matter.fill("What should this test prove?");
    await expect(mapTask).toHaveValue(
      "Trace the test runner and identify the smallest coverage gap.",
    );
  });

  test("Convene opens WebSocket and receives session_started", async ({ page }) => {
    await installSmokeOnlyWebSocketShim(page);
    await page.goto("/");
    await expect(page.getByTestId("cabinet-chip").first()).toBeVisible({
      timeout: 10_000,
    });
    const health = await page.evaluate(async (backend) => {
      const resp = await fetch(`${backend}/api/health`, { cache: "no-store" });
      if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}`);
      return resp.json() as Promise<{ ws_smoke_only?: boolean }>;
    }, BACKEND);
    expect(health.ws_smoke_only).toBe(true);

    const topicInput = page.getByRole("textbox", {
      name: /proceeding statement/i,
    });
    await topicInput.fill("E2E deliberation round-trip test");

    // Listen for WebSocket to the configured council /ws/deliberate endpoint.
    // The frame listener must attach synchronously inside the "websocket"
    // handler: awaiting the websocket promise first and then calling
    // ws.waitForEvent("framereceived") can miss session_started when it
    // arrives before the listener is registered (flaked in previous CI run
    // previous CI run — first observed frame was round_started).
    let wsUrl = "";
    const sessionStarted = new Promise<Record<string, unknown>>(
      (resolve, reject) => {
        const timer = setTimeout(
          () => reject(new Error("no session_started frame within 15s")),
          15_000,
        );
        page.on("websocket", (ws) => {
          if (!ws.url().includes("/ws/deliberate")) return;
          wsUrl = ws.url();
          ws.on("framereceived", (frame) => {
            let data: Record<string, unknown>;
            try {
              data = JSON.parse(frame.payload as string);
            } catch {
              return;
            }
            if (data.type === "session_started") {
              clearTimeout(timer);
              resolve(data);
            }
          });
        });
      },
    );

    const conveneBtn = page.getByRole("button", { name: /convene/i });
    await conveneBtn.click();

    const data = await sessionStarted;
    expect(wsUrl).toContain(WS_DELIBERATE_URL);
    expect(data.session_id).toBeTruthy();
    expect(data.session_id).toBe("smoke-session");
  });

  test("every browser nav tab responds and becomes current", async ({ page }) => {
    await installGovernanceShellFixtures(page);
    await page.goto("/");
    const nav = page.locator("nav");
    const tabs = [
      "Deliberate",
      "Direct Fire",
      "History",
      "Cabinets",
      "Discover",
      "Settings",
      "Patterns",
      "Drift",
      "Librarian",
      "Meta-review",
      "Outbox",
      "Watch",
    ];
    for (const tab of tabs) {
      const btn = nav.getByRole("button", { name: tab, exact: true });
      await expect(btn, `${tab} must remain a visible browser destination`).toBeVisible();
      await btn.click();
      await expect(btn, `${tab} click must update the active destination`).toHaveAttribute(
        "aria-current",
        "page",
      );
    }
  });

  test("core readiness endpoints exist and return their current contracts", async ({ request }) => {
    for (const path of ["/api/health", "/api/cabinets", "/api/sessions", "/api/discover"]) {
      const response = await request.get(`${BACKEND}${path}`);
      expect(response.ok(), `${path} returned ${response.status()}`).toBe(true);
    }
  });

  test("browser can fetch health API with CORS", async ({ page }) => {
    await page.goto("/");
    const body = await page.evaluate(async (backend) => {
      const resp = await fetch(`${backend}/api/health`, { cache: "no-store" });
      if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}`);
      return resp.json();
    }, BACKEND);
    expect(body).toHaveProperty("providers_available");
    expect(Array.isArray(body.providers_available)).toBe(true);
    // CORS + shape only — CI has no live provider vault (xmcp unreachable).
  });

  test("browser can fetch sessions API with CORS", async ({ page }) => {
    await page.goto("/");
    const body = await page.evaluate(async (backend) => {
      const resp = await fetch(`${backend}/api/sessions`, { cache: "no-store" });
      if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}`);
      return resp.json();
    }, BACKEND);
    const sessions = Array.isArray(body) ? body : body.sessions || body.entries || [];
    expect(Array.isArray(sessions)).toBe(true);
  });

  test("Settings save and health probes", async ({ page }) => {
    await page.route("http://127.0.0.1:18080/health", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        headers: { "access-control-allow-origin": "*" },
        body: JSON.stringify({ status: "ok", service: "ai-gateway" }),
      }),
    );
    await page.goto("/");
    await page.getByRole("button", { name: "Settings", exact: true }).click();

    await page.getByTestId("settings-save").click();
    await page.getByTestId("settings-test-connection").click();

    const councilProbe = page.getByTestId("settings-health-council");
    await expect(councilProbe).toBeVisible({ timeout: 15_000 });
    await expect(councilProbe).toHaveAttribute("data-health-state", "ok", {
      timeout: 15_000,
    });
    await expect(councilProbe).toContainText(/council/i);
    const gatewayProbe = page.getByTestId("settings-health-gateway");
    await expect(gatewayProbe).toHaveAttribute("data-health-state", "ok", {
      timeout: 15_000,
    });
    await expect(gatewayProbe).toContainText(/Gateway reachable/i);
    await expect(page.getByTestId("settings-health-probes")).toBeVisible();
  });

  test("wargame cabinet shows experimental help callout", async ({
    page,
    request,
  }) => {
    const hasWargame = await request
      .get(`${BACKEND}/api/cabinets`)
      .then(async (res) => {
        if (!res.ok()) return false;
        const body = (await res.json()) as { cabinets?: { name: string }[] };
        return body.cabinets?.some((c) => c.name === "wargame") ?? false;
      })
      .catch(() => false);
    test.skip(!hasWargame, "wargame cabinet not in /api/cabinets");

    await page.goto("/");
    await expect(page.getByTestId("cabinet-chip").first()).toBeVisible({
      timeout: 10_000,
    });
    const wargameChip = page
      .locator('[data-cabinet-name="wargame"]')
      .or(page.getByTestId("cabinet-chip").filter({ hasText: /Wargame/i }));
    await expect(wargameChip.first()).toBeVisible({ timeout: 10_000 });
    await wargameChip.first().click();
    const help = page.getByTestId("wargame-idle-help");
    await expect(help).toBeVisible();
    // Mode hint, seat roles, and experimental warning.
    await expect(help).toContainText(/adversarial/i);
    await expect(help).toContainText(/validate output quality/i);
  });

  test("gateway routing toggle on Deliberate idle", async ({ page }) => {
    await page.goto("/");
    const toggle = page.getByTestId("gateway-toggle");
    await expect(toggle).toBeVisible({ timeout: 5000 });
    await expect(toggle).toContainText(/governed via gateway/i);
    // Sensitivity select appears only when the toggle is on, with the
    // GREEN/YELLOW/RED escalation levels (lowercase wire values).
    await expect(page.getByTestId("gateway-sensitivity")).toHaveCount(0);
    await toggle.click();
    const select = page.getByTestId("gateway-sensitivity");
    await expect(select).toBeVisible();
    await expect(select.locator("option")).toHaveText(["GREEN", "YELLOW", "RED"]);
  });

  test("Discover view lists the exact-source provider inventory", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: "Discover", exact: true }).click();
    await expect(page.getByTestId("discover-view")).toBeVisible();
    await expect(page.getByTestId("discover-provider").first()).toBeVisible({
      timeout: 10_000,
    });
    expect(await page.getByTestId("discover-provider").count()).toBeGreaterThan(0);
  });

  test("frame check toggle on Deliberate idle", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("frame-check-toggle")).toBeVisible({
      timeout: 5000,
    });
    await expect(page.getByTestId("frame-check-toggle")).toContainText(
      /frame check/i,
    );
  });

  test("Librarian tab loads or skips when unreachable", async ({ page }) => {
    await page.goto("/");
    const librarianReachable = await page.evaluate(async (backend) => {
      try {
        const resp = await fetch(`${backend}/api/librarian/health`, {
          cache: "no-store",
        });
        if (!resp.ok) return false;
        const body = (await resp.json()) as { state?: string };
        return body.state !== "offline";
      } catch {
        return false;
      }
    }, BACKEND);
    test.skip(
      !librarianReachable,
      "Librarian backend unreachable or offline — skip shell test",
    );

    await page.getByRole("button", { name: "Librarian", exact: true }).click();
    await expect(page.getByTestId("librarian-shell")).toBeVisible({
      timeout: 10_000,
    });
  });

  test("Outbox tab always renders an honest governed state", async ({ page }) => {
    await installGovernanceShellFixtures(page);
    await page.goto("/");
    await page.getByRole("button", { name: "Outbox", exact: true }).click();
    await expect(page.getByTestId("outbox-view")).toBeVisible({ timeout: 5000 });
    await expect(
      page.getByRole("heading", {
        name: "Gateway Outbox Provenance",
        exact: true,
      }),
    ).toBeVisible();
  });

  test("advanced destinations render their own surface, not only the shared main", async ({ page }) => {
    await installGovernanceShellFixtures(page);
    await page.goto("/");
    const destinations = [
      ["Direct Fire", page.getByTestId("direct-fire-panel")],
      ["History", page.getByTestId("session-list")],
      ["Cabinets", page.getByText("Cabinets", { exact: true }).first()],
      ["Discover", page.getByTestId("discover-view")],
      ["Settings", page.getByTestId("settings-save")],
      ["Drift", page.getByRole("heading", { name: "Drift Reports", exact: true })],
      ["Librarian", page.getByTestId("librarian-shell")],
      ["Meta-review", page.getByRole("heading", { name: "Meta-review", exact: true })],
      ["Outbox", page.getByTestId("outbox-view")],
      ["Watch", page.getByTestId("watch-view")],
    ] as const;
    for (const [name, destination] of destinations) {
      await page.getByRole("button", { name, exact: true }).click();
      await expect(destination, `${name} must render its own surface`).toBeVisible({
        timeout: 5000,
      });
    }
  });
});
