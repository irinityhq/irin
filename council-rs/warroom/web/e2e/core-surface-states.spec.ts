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
    let state: "failure" | "empty" | "populated" = "failure";
    await page.route("**/api/discover", async (route) => {
      if (route.request().method() === "OPTIONS") {
        await fulfillJson(route, {});
        return;
      }
      attempts += 1;
      if (state === "failure") {
        await fulfillJson(route, { error: "discovery temporarily unavailable" }, 503);
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

    await expect(page.getByText(/Discovery failed:/)).toContainText("503");
    state = "empty";
    await page.getByTestId("discover-refresh").click();
    await expect(page.getByText("No providers reported. Check council --serve logs.")).toBeVisible();

    state = "populated";
    await page.getByTestId("discover-refresh").click();
    await expect(page.getByTestId("discover-provider")).toHaveCount(1);
    await expect(page.getByTestId("discover-provider")).toContainText("nvidia");
    expect(attempts).toBeGreaterThanOrEqual(3);
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
