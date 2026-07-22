import { test, expect, type Page } from "@playwright/test";
import { BACKEND } from "./support/ports";

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

/** Pinned single-shot event sequence (feature contract wire contract). */
const SINGLE_SHOT_SEQUENCE = [
  "session_started",
  "synthesis_started",
  "synthesis_complete",
  "session_saved",
  "done",
];

test.describe("Direct Fire", () => {
  test("mode picker shows all five modes and premortem kill-criteria banner", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByRole("button", { name: "Direct Fire", exact: true }).click();
    await expect(page.getByTestId("direct-fire-panel")).toBeVisible();

    for (const mode of ["contrarian", "munger", "kiss", "specops", "premortem"]) {
      await expect(page.getByTestId(`direct-fire-mode-${mode}`)).toBeVisible();
    }

    // Non-experimental default — no banner.
    await expect(page.getByTestId("premortem-experimental")).toHaveCount(0);

    await page.getByTestId("direct-fire-mode-premortem").click();
    const banner = page.getByTestId("premortem-experimental");
    await expect(banner).toBeVisible();
    await expect(banner).toContainText(/validate output quality/i);
  });

  test("single-shot flow renders synthesis with no round events", async ({
    page,
  }) => {
    await installSmokeOnlyWebSocketShim(page);
    await page.goto("/");
    const health = await page.evaluate(async (backend) => {
      const resp = await fetch(`${backend}/api/health`, { cache: "no-store" });
      if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}`);
      return resp.json() as Promise<{ ws_smoke_only?: boolean }>;
    }, BACKEND);
    expect(health.ws_smoke_only).toBe(true);

    await page.getByRole("button", { name: "Direct Fire", exact: true }).click();
    await page
      .getByTestId("direct-fire-topic")
      .fill("E2E direct fire single-shot smoke");

    // Attach framereceived inside the websocket handler BEFORE submit.
    // Awaiting waitForEvent("websocket") then registering the listener
    // races the smoke path (session_started…done fires in one burst) and
    // can miss every frame — same flake class as warroom-smoke.spec.ts
    // across repeated CI runs.
    const frames: { type: string }[] = [];
    page.on("websocket", (ws) => {
      if (!ws.url().includes("/ws/deliberate")) return;
      ws.on("framereceived", (frame) => {
        try {
          frames.push(JSON.parse(frame.payload as string) as { type: string });
        } catch {
          // Ignore non-JSON frames.
        }
      });
    });

    await page.getByTestId("direct-fire-submit").click();

    await expect
      .poll(() => frames.some((f) => f.type === "done"), { timeout: 15_000 })
      .toBe(true);

    const types = frames.map((f) => f.type);
    // No council-round events — single shot (feature contract contract).
    expect(types.filter((t) => /^(round_|seat_|convergence)/.test(t))).toEqual([]);
    // The pinned sequence arrives in order (benign info events tolerated).
    expect(types.filter((t) => SINGLE_SHOT_SEQUENCE.includes(t))).toEqual(
      SINGLE_SHOT_SEQUENCE,
    );

    // Synthesis renders in the result panel; firing state and round UI gone.
    await expect(page.getByTestId("direct-fire-result")).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.getByTestId("direct-fire-result")).not.toBeEmpty();
    await expect(page.getByTestId("direct-fire-firing")).toHaveCount(0);
    await expect(page.getByTestId("direct-fire-saved")).toContainText(/Saved →/);
  });

  test("provider failure is explicit and Try again restores a fireable form", async ({
    page,
  }) => {
    let starts = 0;
    await page.routeWebSocket("**/ws/deliberate", (ws) => {
      ws.onMessage((message) => {
        const frame = JSON.parse(String(message)) as { type?: string };
        if (frame.type !== "start") return;
        starts += 1;
        ws.send(
          JSON.stringify({
            type: "error",
            session_id: `direct-fire-failure-${starts}`,
            ts: "2026-07-17T00:00:00Z",
            data: {
              message: "Provider credentials rejected by the configured route",
              fatal: true,
            },
          }),
        );
      });
    });

    await page.goto("/");
    await page.getByRole("button", { name: "Direct Fire", exact: true }).click();
    const topic = page.getByTestId("direct-fire-topic");
    await topic.fill("Exercise the direct-fire provider failure path");
    await page.getByTestId("direct-fire-submit").click();

    const error = page.getByTestId("direct-fire-error");
    await expect(error).toBeVisible();
    await expect(error).toContainText("Provider credentials rejected");
    await page.getByTestId("direct-fire-retry").click();

    await expect(topic).toBeVisible();
    await expect(topic).toHaveValue("Exercise the direct-fire provider failure path");
    await expect(page.getByTestId("direct-fire-submit")).toBeEnabled();

    // The restored form is not decorative: it can dispatch another attempt.
    await page.getByTestId("direct-fire-submit").click();
    await expect.poll(() => starts).toBe(2);
    await expect(error).toContainText("Provider credentials rejected");
  });
});
