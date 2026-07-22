import { expect, test, type Route } from "@playwright/test";

const CORS_HEADERS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET,POST,OPTIONS",
  "access-control-allow-headers": "content-type,authorization",
};

const POPULATED_PATTERNS = {
  total: 1,
  session_count: 1,
  actions: { continue: 1 },
  by_round: { "1": 1 },
  by_cabinet: { freeride: { continue: 1 } },
  convergence_buckets: {
    "0-20%": 0,
    "20-40%": 0,
    "40-60%": 1,
    "60-80%": 0,
    "80-100%": 0,
  },
  avg_convergence_at_pause: 0.5,
  top_keywords: [["readiness", 1]],
  sequences: [["continue"]],
  multi_intervention_sessions: 0,
  recent: [],
  window_days: 30,
};

const DRIFT_REPORT = {
  name: "drift-2026-07-17.md",
  size: 128,
  mtime: "2026-07-17T12:00:00Z",
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

test.describe("advanced War Room surfaces", () => {
  test("Patterns reports an endpoint failure and retries successfully", async ({ page }) => {
    let attempts = 0;
    await page.route("**/*", async (route) => {
      const pathname = new URL(route.request().url()).pathname;
      if (pathname === "/api/interventions") {
        await fulfillJson(route, { entries: [], total: 0 });
        return;
      }
      if (pathname !== "/api/patterns") {
        await route.continue();
        return;
      }
      if (route.request().method() === "OPTIONS") {
        await fulfillJson(route, {});
        return;
      }
      attempts += 1;
      if (attempts === 1) {
        await fulfillJson(route, { error: "temporarily unavailable" }, 503);
        return;
      }
      await fulfillJson(route, POPULATED_PATTERNS);
    });

    await page.goto("/");
    await page.getByRole("button", { name: "Patterns", exact: true }).click();

    const error = page.locator('[role="alert"]').filter({
      hasText: "Could not load operator patterns",
    });
    await expect(error).toContainText("Could not load operator patterns");
    await expect(page.getByText("Loading patterns…")).toHaveCount(0);

    await error.getByRole("button", { name: "Retry", exact: true }).click();

    await expect.poll(() => attempts).toBe(2);
    await expect(
      page.getByRole("heading", { name: "Operator Patterns", exact: true }),
    ).toBeVisible();
    await expect(page.getByText("No interventions in window.")).toBeVisible();
    expect(attempts).toBe(2);
  });

  test("Intervention log failure is distinct from empty and can recover", async ({ page }) => {
    let attempts = 0;
    await page.route("**/*", async (route) => {
      const pathname = new URL(route.request().url()).pathname;
      if (pathname === "/api/patterns") {
        await fulfillJson(route, POPULATED_PATTERNS);
        return;
      }
      if (pathname !== "/api/interventions") {
        await route.continue();
        return;
      }
      if (route.request().method() === "OPTIONS") {
        await fulfillJson(route, {});
        return;
      }
      attempts += 1;
      if (attempts === 1) {
        await fulfillJson(route, { error: "temporarily unavailable" }, 503);
        return;
      }
      await fulfillJson(route, {
        total: 1,
        entries: [
          {
            session_id: "patterns-session-1",
            action: "continue",
            payload: {},
            round_num: 1,
            convergence_at_pause: 0.5,
            ts: "2026-07-16T12:00:00Z",
            logged_at: "2026-07-16T12:00:00Z",
          },
        ],
      });
    });

    await page.goto("/");
    await page.getByRole("button", { name: "Patterns", exact: true }).click();

    await expect.poll(() => attempts).toBe(1);
    const error = page.locator('[role="alert"]').filter({
      hasText: "Could not load intervention log",
    });
    await expect(error).toContainText("Could not load intervention log");
    await expect(page.getByText("No interventions in window.")).toHaveCount(0);

    await error.getByRole("button", { name: "Retry", exact: true }).click();

    await expect(page.getByText("patterns-session-1", { exact: true })).toBeVisible();
    await expect(error).toHaveCount(0);
    expect(attempts).toBe(2);
  });

  test("Drift reports and runs expose failures and recover on retry", async ({ page }) => {
    let reportListAttempts = 0;
    let runAttempts = 0;
    let postRunStatusFailures = 0;
    await page.route("**/*", async (route) => {
      const pathname = new URL(route.request().url()).pathname;
      if (
        route.request().method() === "OPTIONS" &&
        pathname.startsWith("/api/drift/")
      ) {
        await fulfillJson(route, {});
        return;
      }
      if (pathname === "/api/drift/weekly/history") {
        await fulfillJson(route, { summaries: [] });
        return;
      }
      if (pathname === `/api/drift/reports/${DRIFT_REPORT.name}`) {
        await fulfillJson(route, {
          name: DRIFT_REPORT.name,
          content: "# Recovered drift report",
          mtime: DRIFT_REPORT.mtime,
        });
        return;
      }
      if (pathname === "/api/drift/reports") {
        reportListAttempts += 1;
        if (reportListAttempts === 1) {
          await fulfillJson(route, { error: "drift storage unavailable" }, 503);
          return;
        }
        if (reportListAttempts === 2) {
          await fulfillJson(route, { reports: [], running: false });
          return;
        }
        if (runAttempts === 2 && postRunStatusFailures === 0) {
          postRunStatusFailures += 1;
          await fulfillJson(route, { error: "status temporarily unavailable" }, 503);
          return;
        }
        await fulfillJson(route, { reports: [DRIFT_REPORT], running: false });
        return;
      }
      if (pathname === "/api/drift/run") {
        runAttempts += 1;
        if (runAttempts === 1) {
          await fulfillJson(route, { error: "worker unavailable" }, 503);
          return;
        }
        await fulfillJson(route, { status: "started", window: 7, limit: 8 });
        return;
      }
      await route.continue();
    });

    await page.goto("/");
    await page.getByRole("button", { name: "Drift", exact: true }).click();

    const loadError = page.locator('[role="alert"]').filter({
      hasText: "Could not load drift reports",
    });
    await expect(loadError).toBeVisible();
    await expect(page.getByText("No reports yet.", { exact: false })).toHaveCount(0);

    await loadError.getByRole("button", { name: "Retry reports", exact: true }).click();
    await expect(page.getByText("No reports yet.", { exact: false })).toBeVisible();
    await expect(loadError).toHaveCount(0);

    await page.getByRole("button", { name: "Refresh", exact: true }).click();
    await expect(page.getByRole("heading", { name: "Recovered drift report" })).toBeVisible();
    expect(reportListAttempts).toBe(3);

    await page.getByRole("button", { name: "Run drift now", exact: true }).click();
    const runError = page.locator('[role="alert"]').filter({
      hasText: "Drift run failed",
    });
    await expect(runError).toBeVisible();
    await runError.getByRole("button", { name: "Retry run", exact: true }).click();
    await expect(runError).toHaveCount(0);
    await expect.poll(() => runAttempts).toBe(2);

    const statusUnknown = page.locator('[role="alert"]').filter({
      hasText: "Drift run accepted; status unknown",
    });
    await expect(statusUnknown).toBeVisible();
    await expect(
      page.getByRole("button", { name: "Running…", exact: true }),
    ).toBeDisabled();
    await expect(
      page.getByRole("button", { name: "Run drift now", exact: true }),
    ).toHaveCount(0);

    const attemptsBeforeStatusRetry = reportListAttempts;
    await statusUnknown.getByRole("button", { name: "Retry status", exact: true }).click();
    await expect.poll(() => reportListAttempts).toBeGreaterThan(attemptsBeforeStatusRetry);
    await expect(statusUnknown).toHaveCount(0);
    await expect(
      page.getByRole("button", { name: "Run drift now", exact: true }),
    ).toBeEnabled();
  });

  test("Drift ignores an older list failure after a newer refresh succeeds", async ({ page }) => {
    let attempts = 0;
    let releaseOlder: () => void = () => {};
    let olderCompleted = false;
    const holdOlder = new Promise<void>((resolve) => {
      releaseOlder = resolve;
    });
    await page.route("**/*", async (route) => {
      const pathname = new URL(route.request().url()).pathname;
      if (
        route.request().method() === "OPTIONS" &&
        pathname.startsWith("/api/drift/")
      ) {
        await fulfillJson(route, {});
        return;
      }
      if (pathname === "/api/drift/weekly/history") {
        await fulfillJson(route, { summaries: [] });
        return;
      }
      if (pathname === `/api/drift/reports/${DRIFT_REPORT.name}`) {
        await fulfillJson(route, {
          name: DRIFT_REPORT.name,
          content: "# Newer drift result",
          mtime: DRIFT_REPORT.mtime,
        });
        return;
      }
      if (pathname === "/api/drift/reports") {
        attempts += 1;
        if (attempts === 1) {
          await holdOlder;
          await fulfillJson(route, { error: "stale list failure" }, 503);
          olderCompleted = true;
          return;
        }
        await fulfillJson(route, { reports: [DRIFT_REPORT], running: false });
        releaseOlder();
        return;
      }
      await route.continue();
    });

    await page.goto("/");
    await page.getByRole("button", { name: "Drift", exact: true }).click();
    await expect.poll(() => attempts).toBe(1);
    await page.getByRole("button", { name: "Refresh", exact: true }).click();

    await expect(page.getByRole("heading", { name: "Newer drift result" })).toBeVisible();
    await expect.poll(() => olderCompleted).toBe(true);
    await expect(
      page.locator('[role="alert"]').filter({ hasText: "Could not load drift reports" }),
    ).toHaveCount(0);
    await expect(page.getByRole("heading", { name: "Newer drift result" })).toBeVisible();
  });

  test("Meta-review latest and run failures recover on retry", async ({ page }) => {
    let latestAttempts = 0;
    let runAttempts = 0;
    await page.route("**/*", async (route) => {
      const pathname = new URL(route.request().url()).pathname;
      if (
        route.request().method() === "OPTIONS" &&
        pathname.startsWith("/api/meta-review/")
      ) {
        await fulfillJson(route, {});
        return;
      }
      if (pathname === "/api/meta-review/latest") {
        latestAttempts += 1;
        if (latestAttempts === 1) {
          await fulfillJson(route, { error: "review storage unavailable" }, 503);
          return;
        }
        if (latestAttempts === 2) {
          await fulfillJson(route, { detail: "no meta-review report found" }, 404);
          return;
        }
        await fulfillJson(route, {
          name: "meta-review-2026-07-17.md",
          content: "# Recovered meta-review",
          mtime: "2026-07-17T12:00:00Z",
        });
        return;
      }
      if (pathname === "/api/meta-review/run") {
        runAttempts += 1;
        if (runAttempts === 1) {
          await fulfillJson(route, { error: "review worker unavailable" }, 503);
          return;
        }
        await fulfillJson(route, {
          status: "complete",
          weeks: 4,
          mean_drift: 0.12,
          stability: "stable",
        });
        return;
      }
      await route.continue();
    });

    await page.goto("/");
    await page.getByRole("button", { name: "Meta-review", exact: true }).click();

    const loadError = page.locator('[role="alert"]').filter({
      hasText: "Could not load latest meta-review",
    });
    await expect(loadError).toBeVisible();
    await expect(page.getByText("No meta-review report yet.", { exact: false })).toHaveCount(0);

    await loadError.getByRole("button", { name: "Retry latest", exact: true }).click();
    await expect(page.getByText("No meta-review report yet.", { exact: false })).toBeVisible();
    await expect(loadError).toHaveCount(0);

    await page.getByRole("button", { name: "Refresh", exact: true }).click();
    await expect(page.getByRole("heading", { name: "Recovered meta-review" })).toBeVisible();
    expect(latestAttempts).toBe(3);

    await page.getByRole("button", { name: "Run meta-review", exact: true }).click();
    const runError = page.locator('[role="alert"]').filter({
      hasText: "Meta-review run failed",
    });
    await expect(runError).toBeVisible();
    await runError.getByRole("button", { name: "Retry run", exact: true }).click();
    await expect(runError).toHaveCount(0);
    await expect.poll(() => runAttempts).toBe(2);
    await expect(page.getByText("4 weeks analyzed", { exact: true })).toBeVisible();
  });

  test("Meta-review ignores an older failure after a newer refresh succeeds", async ({ page }) => {
    let attempts = 0;
    let releaseOlder: () => void = () => {};
    let olderCompleted = false;
    const holdOlder = new Promise<void>((resolve) => {
      releaseOlder = resolve;
    });
    await page.route("**/*", async (route) => {
      const pathname = new URL(route.request().url()).pathname;
      if (
        route.request().method() === "OPTIONS" &&
        pathname.startsWith("/api/meta-review/")
      ) {
        await fulfillJson(route, {});
        return;
      }
      if (pathname === "/api/meta-review/latest") {
        attempts += 1;
        if (attempts === 1) {
          await holdOlder;
          await fulfillJson(route, { error: "stale review failure" }, 503);
          olderCompleted = true;
          return;
        }
        await fulfillJson(route, {
          name: "meta-review-newer.md",
          content: "# Newer meta-review result",
          mtime: "2026-07-17T12:00:00Z",
        });
        releaseOlder();
        return;
      }
      await route.continue();
    });

    await page.goto("/");
    await page.getByRole("button", { name: "Meta-review", exact: true }).click();
    await expect.poll(() => attempts).toBe(1);
    await page.getByRole("button", { name: "Refresh", exact: true }).click();

    await expect(
      page.getByRole("heading", { name: "Newer meta-review result" }),
    ).toBeVisible();
    await expect.poll(() => olderCompleted).toBe(true);
    await expect(
      page.locator('[role="alert"]').filter({
        hasText: "Could not load latest meta-review",
      }),
    ).toHaveCount(0);
    await expect(
      page.getByRole("heading", { name: "Newer meta-review result" }),
    ).toBeVisible();
  });
});
