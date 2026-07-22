import { expect, test, type Route } from "@playwright/test";

async function fulfillJson(route: Route, body: unknown, status = 200) {
  await route.fulfill({
    status,
    contentType: "application/json",
    body: JSON.stringify(body),
  });
}

const degradation = {
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
};

test.describe("governed War Room surfaces (zero-provider fixtures)", () => {
  test("Watch renders an explicit disarmed populated snapshot", async ({ page }) => {
    await page.route("**/api/governance/watch", (route) =>
      fulfillJson(route, {
        tenant: "sovereign",
        canary_tenant: "sovereign",
        action_production_armed: false,
        sentinels: [
          {
            name: "public-readiness",
            tier: "observe",
            cooldown_ms: 60_000,
            enabled: true,
            hard_killed_at: null,
            last_fire_at: null,
            fires_last_hour: 0,
          },
        ],
        temperature: {
          value: 0,
          level: "cold",
          fires_last_hour: 0,
          fires_last_24h: 0,
        },
        recent_fires: [],
        budget: { spend_today_usd: 0, spend_cap_usd: 5 },
        degradation,
      }),
    );

    await page.goto("/");
    await page.getByRole("button", { name: "Watch", exact: true }).click();
    await expect(page.getByTestId("watch-view")).toBeVisible();
    await expect(page.getByText("Action production DISARMED", { exact: true })).toBeVisible();
    await expect(page.getByText("public-readiness", { exact: true })).toBeVisible();
    await expect(page.getByText("No degradation counters raised", { exact: true })).toBeVisible();
  });

  test("Watch failure is an honest unavailable state", async ({ page }) => {
    await page.route("**/api/governance/watch", (route) =>
      fulfillJson(route, { error: "gateway unavailable" }, 503),
    );
    await page.goto("/");
    await page.getByRole("button", { name: "Watch", exact: true }).click();
    await expect(page.getByText("Governance snapshot unavailable", { exact: true })).toBeVisible();
    await expect(page.getByText(/503/)).toBeVisible();
  });

  test("Outbox expands a directive and reports exact-byte verification", async ({ page }) => {
    const summary = {
      id: "directive-1",
      status: "pending",
      verdict: "allow",
      authority: "gateway",
      created_at_ms: 1_800_000_000_000,
      signature: { alg: "Ed25519", kid: "test-key", value: "fixture-signature" },
      council_session_id: "session-1",
      council_cost_usd: 0,
      worker_provenance: { status: "verified_exact", fabrication_guard: true },
    };
    await page.route("**/api/governance/outbox", (route) =>
      fulfillJson(route, {
        canary_tenant: "sovereign",
        directives: [summary],
        next_cursor: null,
      }),
    );
    await page.route("**/api/governance/outbox/directive-1", (route) =>
      fulfillJson(route, {
        directive: {
          ...summary,
          in_response_to: "request-1",
          tenant: "sovereign",
          envelope: {},
          envelope_json_canonical: '{"directive":"fixture"}',
        },
        verification: {
          verified: true,
          algorithm: "Ed25519",
          kid: "test-key",
          detail: "verified_exact_canonical_utf8",
        },
      }),
    );

    await page.goto("/");
    await page.getByRole("button", { name: "Outbox", exact: true }).click();
    await page.getByRole("button", { name: /directive-1/ }).click();
    await expect(
      page.getByText("Ed25519 verified over exact canonical UTF-8", { exact: true }),
    ).toBeVisible();
    await expect(page.getByText("verified_exact_canonical_utf8", { exact: true })).toBeVisible();
    await expect(page.getByText("Verified exact", { exact: true })).toBeVisible();
  });

  test("Outbox empty and failure states remain distinguishable", async ({ page }) => {
    let fail = false;
    await page.route("**/api/governance/outbox", (route) =>
      fail
        ? fulfillJson(route, { error: "gateway unavailable" }, 503)
        : fulfillJson(route, {
            canary_tenant: "sovereign",
            directives: [],
            next_cursor: null,
          }),
    );

    await page.goto("/");
    await page.getByRole("button", { name: "Outbox", exact: true }).click();
    await expect(
      page.getByText("No directives found in the configured canary outbox.", { exact: true }),
    ).toBeVisible();

    fail = true;
    await page.reload();
    await page.getByRole("button", { name: "Outbox", exact: true }).click();
    await expect(page.getByText("Gateway Outbox unavailable", { exact: true })).toBeVisible();
    await expect(page.getByText(/503/)).toBeVisible();
  });
});
