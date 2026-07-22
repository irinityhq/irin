import { test, expect } from "@playwright/test";
import { FakeDeliberationHarness } from "./fixtures/fake-deliberation";
import { installAvailableProviderDiscovery } from "./fixtures/available-provider-discovery";

test("record head pins under app chrome on scroll", async ({ page }) => {
  test.setTimeout(60_000);
  await installAvailableProviderDiscovery(page);
  await page.setViewportSize({ width: 1280, height: 520 });

  const harness = new FakeDeliberationHarness({
    timings: { connectingHoldMs: 100, kissPendingMs: 100 },
    topic:
      "Long proceeding for sticky check — how often should a security-critical project update its dependency packages given severity and reachability tradeoffs?",
  });
  await harness.install(page);

  await page.goto("/");
  await expect(page.getByTestId("cabinet-chip").first()).toBeVisible({
    timeout: 10_000,
  });
  const topic = page.getByRole("textbox", { name: /proceeding statement/i });
  await topic.fill("Sticky head verification run");
  await page.getByRole("button", { name: /convene the council/i }).click();

  await expect(page.getByText(/awaiting your call/i)).toBeVisible({
    timeout: 15_000,
  });
  await page.getByRole("button", { name: /^Continue$/i }).click();
  await expect(page.getByTestId("new-deliberation-nav")).toBeVisible({
    timeout: 15_000,
  });

  const head = page.locator(".cg-record-head").first();
  await expect(head).toBeVisible();

  const scrollable = await page.evaluate(
    () => document.documentElement.scrollHeight - window.innerHeight,
  );
  expect(scrollable).toBeGreaterThan(100);

  await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
  await page.waitForTimeout(200);

  const box = await head.boundingBox();
  expect(box).not.toBeNull();
  // Header is 48px (h-12); head must pin flush under it, not slide beneath.
  expect(Math.round(box!.y)).toBe(48);
});
