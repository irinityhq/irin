import { expect, test, type Route } from "@playwright/test";

const CORS_HEADERS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET,OPTIONS",
  "access-control-allow-headers": "content-type,authorization",
};

async function fulfillJson(route: Route, data: unknown, status = 200) {
  if (route.request().method() === "OPTIONS") {
    await route.fulfill({ status: 204, headers: CORS_HEADERS });
    return;
  }
  await route.fulfill({
    status,
    contentType: "application/json",
    headers: CORS_HEADERS,
    body: JSON.stringify(data),
  });
}

test.describe("core War Room surface states", () => {
  test("Discover distinguishes failure, empty, and populated rescans", async ({ page }) => {
    let attempts = 0;
    // Terminal (non-retryable) HTTP failure — transient 502/503/network races
    // are covered by unit tests with deterministic backoff.
    let state: "failure" | "empty" | "populated" = "failure";
    await page.route("**/api/discover", async (route) => {
      if (route.request().method() === "OPTIONS") {
        await fulfillJson(route, {});
        return;
      }
      attempts += 1;
      if (state === "failure") {
        await fulfillJson(route, { error: "discovery auth required" }, 401);
        return;
      }
      if (state === "empty") {
        await fulfillJson(route, { providers: [], log: [] });
        return;
      }
      await fulfillJson(route, {
        providers: [
          {
            name: "nvidia",
            available: true,
            source: "env",
            env_hint: "NVIDIA_API_KEY",
            models: ["nvidia/free-model"],
          },
        ],
        log: ["NVIDIA_API_KEY detected"],
      });
    });

    await page.goto("/");
    await page.getByRole("button", { name: "Discover", exact: true }).click();

    await expect(page.getByText(/Discovery failed:/)).toContainText("401");
    state = "empty";
    await page.getByTestId("discover-refresh").click();
    await expect(page.getByText("No providers reported. Check council --serve logs.")).toBeVisible();

    state = "populated";
    await page.getByTestId("discover-refresh").click();
    await expect(page.getByTestId("discover-provider")).toHaveCount(1);
    await expect(page.getByTestId("discover-provider")).toContainText("nvidia");
    expect(attempts).toBeGreaterThanOrEqual(3);
  });

  test("Discover recovers from an initial transient failure without Rescan", async ({ page }) => {
    let attempts = 0;
    await page.route("**/api/discover", async (route) => {
      if (route.request().method() === "OPTIONS") {
        await fulfillJson(route, {});
        return;
      }
      attempts += 1;
      if (attempts === 1) {
        await fulfillJson(route, { error: "warming up" }, 503);
        return;
      }
      await fulfillJson(route, {
        providers: [
          {
            name: "nvidia",
            available: true,
            source: "env",
            env_hint: "NVIDIA_API_KEY",
            models: ["nvidia/free-model"],
          },
        ],
        log: ["NVIDIA_API_KEY detected after cold start"],
      });
    });

    await page.goto("/");
    await page.getByRole("button", { name: "Discover", exact: true }).click();
    await expect(page.getByTestId("discover-provider")).toBeVisible({
      timeout: 15_000,
    });
    await expect(page.getByTestId("discover-provider")).toContainText("nvidia");
    await expect(page.getByText(/Discovery failed:/)).toHaveCount(0);
    expect(attempts).toBeGreaterThanOrEqual(2);
  });

  test("Deliberate auto-selects a runnable cabinet and mutes unavailable need text", async ({
    page,
  }) => {
    await page.route("**/api/health", async (route) => {
      if (route.request().method() === "OPTIONS") {
        await fulfillJson(route, {});
        return;
      }
      await fulfillJson(route, {
        council_version: "e2e",
        stream_version: "1.0.0",
        providers_available: ["nvidia"],
        providers_missing: ["grok_hermes", "gemini_agy", "claude_code"],
        sessions_dir: "/tmp",
        index_path: "/tmp/index.json",
        index_exists: false,
      });
    });
    await page.route("**/api/cabinets", async (route) => {
      if (route.request().method() === "OPTIONS") {
        await fulfillJson(route, {});
        return;
      }
      // Contract: GET /api/cabinets → { cabinets: Cabinet[] }
      await fulfillJson(route, {
        cabinets: [
          {
            name: "standard",
            label: "Standard Council",
            description: "",
            seats: [
              { name: "a", provider: "grok_hermes", model: "m", system: "" },
              { name: "b", provider: "gemini_agy", model: "m", system: "" },
            ],
            chair: { provider: "claude_code", model: "m" },
            rounds: 2,
            is_triad: false,
          },
          {
            name: "starter-nvidia",
            label: "starter-nvidia",
            description: "",
            seats: [{ name: "n", provider: "nvidia", model: "m", system: "" }],
            chair: { provider: "nvidia", model: "m" },
            rounds: 1,
            is_triad: false,
          },
        ],
      });
    });
    await page.route("**/api/discover", async (route) => {
      if (route.request().method() === "OPTIONS") {
        await fulfillJson(route, {});
        return;
      }
      await fulfillJson(route, {
        providers: [
          {
            name: "nvidia",
            available: true,
            source: "env",
            env_hint: "NVIDIA_API_KEY",
            models: ["nvidia/free-model"],
          },
          {
            name: "grok_hermes",
            available: false,
            source: "",
            env_hint: null,
            models: [],
          },
          {
            name: "gemini_agy",
            available: false,
            source: "",
            env_hint: null,
            models: [],
          },
          {
            name: "claude_code",
            available: false,
            source: "",
            env_hint: null,
            models: [],
          },
        ],
        log: [],
      });
    });

    await page.goto("/");
    await expect(page.getByTestId("deliberate-workspace-idle")).toBeVisible({
      timeout: 10_000,
    });

    const starter = page.locator('[data-testid="cabinet-chip"][data-cabinet-name="starter-nvidia"]');
    const standard = page.locator('[data-testid="cabinet-chip"][data-cabinet-name="standard"]');
    await expect(starter).toHaveAttribute("data-cabinet-available", "true", {
      timeout: 10_000,
    });
    await expect(standard).toHaveAttribute("data-cabinet-available", "false");

    // Auto-select moved off blocked standard onto runnable starter-nvidia.
    await expect(starter).toHaveClass(/border-amber/);
    // Need prose stays muted (no danger class on the need label).
    const need = standard.getByTestId("cabinet-need");
    await expect(need).toBeVisible();
    await expect(need).toHaveClass(/text-fg-muted/);
    await expect(need).not.toHaveClass(/text-danger/);
  });

  test("Settings persist across reload", async ({ page }) => {
    const persistedApi = "http://127.0.0.1:9876";

    await page.goto("/");
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    const apiBase = page.getByPlaceholder("http://127.0.0.1:8765");
    await apiBase.fill(persistedApi);
    await page.getByTestId("settings-save").click();

    await page.reload();
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    await expect(page.getByPlaceholder("http://127.0.0.1:8765")).toHaveValue(persistedApi);
  });

  test("Settings explain an authentication failure", async ({ page }) => {
    await page.route("**/api/health", (route) =>
      fulfillJson(route, { error: "unauthorized" }, 401),
    );

    await page.goto("/");
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    await page.getByPlaceholder("http://127.0.0.1:18080").fill("");
    await page.getByTestId("settings-test-connection").click();

    const councilProbe = page.getByTestId("settings-health-council");
    await expect(councilProbe).toHaveAttribute("data-health-state", "fail");
    await expect(councilProbe).toContainText("401 Unauthorized");
    await expect(councilProbe).toContainText("COUNCIL_AUTH_TOKEN");
  });
});
