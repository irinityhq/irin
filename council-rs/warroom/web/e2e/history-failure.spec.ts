import { expect, test, type Route } from "@playwright/test";

const CORS_HEADERS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET,POST,PATCH,DELETE,OPTIONS",
  "access-control-allow-headers": "content-type,authorization,if-match",
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

const ENTRY = {
  id: "history-e2e",
  ts: "2026-07-17T00:00:00Z",
  topic: "History failure recovery fixture",
  keywords: ["recovery"],
  ruling_digest: "Recovered history record",
  confidence: "HIGH",
  cabinet: "starter",
  convergence: 0.8,
  mode: "normal",
  seat_count: 1,
  rounds: 1,
  synthesis_model: "fixture-chair",
  version: "v2",
};

const DETAIL = {
  session_id: ENTRY.id,
  topic: ENTRY.topic,
  cabinet_name: ENTRY.cabinet,
  rounds: [],
  synthesis: "## Recovered ruling\n\nThe selected proceeding loaded on retry.",
  synthesis_model: ENTRY.synthesis_model,
  total_tokens: 42,
  total_latency_ms: 120,
  total_cost_usd: 0,
  mode: "normal",
  precedent_ids: [],
  timestamp: ENTRY.ts,
};

test.describe("History failure honesty", () => {
  test("list failure can retry into a truthful empty state", async ({ page }) => {
    let listAttempts = 0;
    let recover = false;
    await page.route("**/api/sessions?limit=*", async (route) => {
      listAttempts += 1;
      if (!recover) {
        await fulfillJson(route, { error: "Session index temporarily unavailable" }, 503);
      } else {
        await fulfillJson(route, { sessions: [] });
      }
    });
    await page.route("**/api/clusters", (route) =>
      fulfillJson(route, {
        clusters: [],
        method: "kmeans",
        k: 0,
        n_sessions: 0,
        generated_at: "",
      }),
    );

    await page.goto("/");
    await page.getByRole("button", { name: "History", exact: true }).click();

    const failure = page.getByTestId("history-list-error");
    await expect(failure).toContainText("Session index temporarily unavailable", {
      timeout: 15_000,
    });
    await expect(page.getByTestId("history-empty")).toHaveCount(0);

    recover = true;
    await page.getByTestId("history-list-retry").click();
    await expect(page.getByTestId("history-empty")).toContainText(
      "No sessions in the index yet",
    );
    await expect(failure).toHaveCount(0);
    expect(listAttempts).toBeGreaterThanOrEqual(2);
  });

  test("detail failure stays attached to the selected record and recovers", async ({
    page,
  }) => {
    let detailAttempts = 0;
    await page.route("**/api/sessions?limit=*", (route) =>
      fulfillJson(route, { sessions: [ENTRY] }),
    );
    await page.route("**/api/sessions/history-e2e/lineage", (route) =>
      fulfillJson(route, { session_id: ENTRY.id, parent: null, children: [] }),
    );
    await page.route("**/api/sessions/history-e2e", async (route) => {
      detailAttempts += 1;
      if (detailAttempts === 1) {
        await fulfillJson(route, { error: "Proceeding file could not be read" }, 500);
      } else {
        await fulfillJson(route, DETAIL);
      }
    });
    await page.route("**/api/clusters", (route) =>
      fulfillJson(route, {
        clusters: [],
        method: "kmeans",
        k: 0,
        n_sessions: 1,
        generated_at: "",
      }),
    );

    await page.goto("/");
    await page.getByRole("button", { name: "History", exact: true }).click();
    await page
      .getByTestId("session-list")
      .getByRole("button", { name: /History failure recovery fixture/ })
      .click();

    const failure = page.getByTestId("history-detail-error");
    await expect(failure).toContainText("Proceeding file could not be read");
    await expect(failure).toContainText("Proceeding record unavailable");

    await page.getByTestId("history-detail-retry").click();
    await expect(failure).toHaveCount(0);
    await expect(
      page.getByRole("complementary", { name: /council ruling/i }),
    ).toContainText("The selected proceeding loaded on retry.");
    expect(detailAttempts).toBe(2);
  });
});
