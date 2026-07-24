import { test, expect, type Page } from "@playwright/test";
import { FakeDeliberationHarness } from "./fixtures/fake-deliberation";
import {
  installAvailableProviderDiscovery,
  installAvailableProviderHealth,
} from "./fixtures/available-provider-discovery";

const NOTE =
  "Operator note persisted across KISS pause remount — e2e harness.";

const failuresByPage = new WeakMap<Page, string[]>();

function isOptionalConsoleNoise(text: string): boolean {
  if (text.includes("Failed to load resource: net::ERR_FAILED")) return true;
  if (text.includes("net::ERR_CONNECTION_REFUSED")) return true;
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
  await installAvailableProviderHealth(page);
  const failures: string[] = [];
  failuresByPage.set(page, failures);
  page.on("console", (msg) => {
    if (msg.type() === "error") {
      const text = msg.text();
      if (isOptionalConsoleNoise(text)) return;
      failures.push(`console error: ${text}`);
    }
  });
  page.on("pageerror", (err) => {
    failures.push(`page error: ${err.message}`);
  });
});

test.afterEach(async ({ page }) => {
  expect(failuresByPage.get(page) ?? []).toEqual([]);
});

test.describe("Deliberate flow (controlled fake WS)", () => {
  test.setTimeout(60_000);

  test("convene → pause → KISS pending → note survives → done escape hatch", async ({
    page,
  }) => {
    const harness = new FakeDeliberationHarness({
      timings: { connectingHoldMs: 700, kissPendingMs: 1800 },
    });
    await harness.install(page);

    await page.goto("/");
    await expect(page.getByTestId("deliberate-workspace-idle")).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.getByTestId("cabinet-chip").first()).toBeVisible({
      timeout: 10_000,
    });

    const topic = page.getByRole("textbox", { name: /proceeding statement/i });
    await topic.fill("How often should a security project update packages?");

    const convene = page.getByRole("button", { name: /convene the council/i });
    await expect(convene).toBeEnabled();
    await convene.click();

    // Button-level feedback may be brief, so the connecting status is also valid.
    await expect(
      page.getByRole("button", { name: /convening/i }).or(
        page.getByText(/opening council channel/i),
      ),
    ).toBeVisible({ timeout: 3000 });

    await expect(page.getByTestId("deliberate-workspace")).toBeVisible({
      timeout: 8000,
    });
    await expect(page.getByText(/opening council channel/i)).toBeVisible({
      timeout: 5000,
    });

    // First pause — operator intervention panel.
    await expect(page.getByText(/awaiting your call/i)).toBeVisible({
      timeout: 12_000,
    });
    await expect(
      page.getByText(/steer the next round with a note/i),
    ).toBeVisible();
    await expect(page.getByRole("button", { name: /kiss/i })).toBeVisible();
    await expect(page.getByText(/strip to essentials/i)).toBeVisible();

    const noteArea = page.getByPlaceholder(/operator note/i);
    await noteArea.fill(NOTE);

    await page.getByRole("button", { name: /kiss/i }).click();

    // Pending state while fake server holds before intervention_received.
    await expect(page.getByText(/applying kiss/i)).toBeVisible({
      timeout: 3000,
    });
    await expect(page.getByText(/applying kiss/i)).toBeHidden({
      timeout: 8000,
    });

    // Second pause — note must survive remount; SpecOps result framed.
    await expect(page.getByText(/awaiting your call/i)).toBeVisible({
      timeout: 5000,
    });
    await expect(noteArea).toHaveValue(NOTE);
    await expect(page.getByText(/specops result/i)).toBeVisible();
    await expect(page.getByText(/kiss escalation result/i)).toBeVisible();

    const kissFrame = harness.interventions.find(
      (p) => p.action === "escalate_kiss",
    );
    expect(kissFrame).toBeDefined();
    expect(kissFrame?.text).toBe(NOTE);

    await page.getByRole("button", { name: /^Continue$/i }).click();

    await expect(page.getByText(/chair composing/i)).toBeVisible({
      timeout: 8000,
    });

    await expect(page.getByTestId("new-deliberation-nav")).toBeVisible({
      timeout: 12_000,
    });
    await expect(page.getByTestId("new-deliberation-header")).toBeVisible();
    await expect(page.getByRole("button", { name: /^abort$/i })).toHaveCount(0);
    await expect(page.getByText(/chair ruling/i)).toBeVisible();
    await expect(page.getByText(/patch on severity/i)).toBeVisible();
  });
});
