import { readFileSync } from "node:fs";
import path from "node:path";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { TouchIdStatus } from "@/lib/touch-id";
import TouchIdControl from "./TouchIdControl";

function status(over: Partial<TouchIdStatus> = {}): TouchIdStatus {
  return {
    state: "ready",
    reason: null,
    armed_exp_at_ms: null,
    armed_expires_in_ms: null,
    stage_expires_in_ms: null,
    enrolled: true,
    allow_real_arm: true,
    can_enroll: false,
    can_arm: true,
    can_renew: false,
    can_disarm: false,
    ...over,
  };
}

function render(
  s: TouchIdStatus | null,
  primaryBusy = false,
  disarmBusy = false,
): string {
  return renderToStaticMarkup(
    <TouchIdControl
      status={s}
      primaryBusy={primaryBusy}
      disarmBusy={disarmBusy}
      onEnroll={() => {}}
      onArm={() => {}}
      onRenew={() => {}}
      onDisarm={() => {}}
    />,
  );
}

describe("Touch ID control rendering", () => {
  it("shows Set up Touch ID when enrollment is absent", () => {
    const html = render(
      status({ state: "setup_required", reason: "enrollment_missing", can_enroll: true }),
    );
    expect(html).toContain("Set up Touch ID");
    expect(html).toContain('data-touch-id-action="enroll"');
    expect(html).not.toContain("Disarm");
  });

  it("shows Touch ID ready with an enabled arm action", () => {
    const html = render(status());
    expect(html).toContain("Touch ID ready");
    expect(html).toContain("Arm with Touch ID");
    expect(html).not.toContain("disabled=");
  });

  it("shows Armed until <time> with Renew and Disarm", () => {
    const exp = new Date(2026, 6, 24, 9, 7, 0).getTime();
    const html = render(
      status({
        state: "armed",
        armed_exp_at_ms: exp,
        can_arm: false,
        can_renew: true,
        can_disarm: true,
      }),
    );
    expect(html).toContain("Armed until 09:07");
    expect(html).toContain("Renew");
    expect(html).toContain("Disarm");
  });

  it("shows Re-enroll Touch ID with the precise incompatibility reason", () => {
    const html = render(
      status({
        state: "reenroll_required",
        reason: "helper_identity_changed",
        can_enroll: true,
        can_arm: false,
      }),
    );
    expect(html).toContain("Re-enroll Touch ID");
    expect(html).toContain("Secure Enclave continuity cannot be proven");
  });

  it("disables the arm action and states why when Gateway is not ready", () => {
    const html = render(
      status({ state: "blocked", reason: "gateway_not_ready", can_arm: false }),
    );
    expect(html).toContain("Arm with Touch ID");
    expect(html).toContain("disabled=");
    expect(html).toContain("Enable Gateway first");
  });

  it("offers no action at all when the helper is missing", () => {
    const html = render(
      status({ state: "unavailable", reason: "helper_missing", can_arm: false }),
    );
    expect(html).toContain("Touch ID unavailable");
    expect(html).not.toContain("Arm with Touch ID");
    expect(html).not.toContain("Disarm");
  });

  it("keeps Disarm available while a Touch ID renewal is in flight", () => {
    const html = render(
      status({
        state: "armed",
        can_arm: false,
        can_renew: true,
        can_disarm: true,
      }),
      true,
    );
    const disarm = html.match(/<button[^>]*data-testid="settings-touch-id-disarm"[^>]*>/)?.[0];
    expect(html).toContain('data-testid="settings-touch-id-primary"');
    expect(html).toContain('aria-busy="true"');
    expect(disarm).toBeDefined();
    expect(disarm).not.toContain("disabled");
    expect(disarm).toContain('aria-busy="false"');
  });

  it("only disables Disarm while disarm itself is in flight", () => {
    const html = render(
      status({ state: "armed", can_renew: true, can_disarm: true }),
      false,
      true,
    );
    const disarm = html.match(/<button[^>]*data-testid="settings-touch-id-disarm"[^>]*>/)?.[0];
    expect(disarm).toContain("disabled");
    expect(disarm).toContain('aria-busy="true"');
  });

  it("never renders ceremony material even if the native side sends it", () => {
    // The redaction guard is fail-closed: rendering must throw rather than
    // paint a challenge or signature into the DOM.
    expect(() =>
      render({ ...status(), challenge: "Zm9v" } as TouchIdStatus),
    ).toThrow(/must not carry/);
  });
});

describe("Touch ID control adjacency", () => {
  /**
   * The brief's product requirement is positional: Touch ID is a control
   * *directly beside* the Gateway control, not a separate screen. Assert it
   * structurally against the Settings source so a later refactor that moves it
   * out of the Gateway Pack card fails here.
   */
  it("renders inside the Gateway Pack card in Settings", () => {
    const source = readFileSync(
      path.join(__dirname, "SettingsPanel.tsx"),
      "utf8",
    );
    const cardStart = source.indexOf('data-testid="settings-gateway-pack"');
    const touchId = source.indexOf("<TouchIdControl");
    const modeCard = source.indexOf('data-testid="settings-gateway-mode"');
    expect(cardStart).toBeGreaterThan(-1);
    expect(touchId).toBeGreaterThan(cardStart);
    // Still inside the pack card: it appears before the next Settings card.
    expect(touchId).toBeLessThan(modeCard);
    expect(source).toContain('import TouchIdControl from "./TouchIdControl"');
  });

  it("fences periodic status writers while a native Touch ID action runs", () => {
    const source = readFileSync(
      path.join(__dirname, "SettingsPanel.tsx"),
      "utf8",
    );
    expect(source).toContain(
      "if (!inTauri || touchIdActionBusy.current) return;",
    );
    expect(source).toContain("touchIdActionBusy.current = true;");
    expect(source).toContain("touchIdActionBusy.current = false;");
    expect(source).toContain(
      "invalidateTouchIdStatusOperations(touchIdOperationFence.current);",
    );
  });
});
