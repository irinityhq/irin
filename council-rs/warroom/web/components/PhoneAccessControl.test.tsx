import {
  Children,
  isValidElement,
  type ReactElement,
  type ReactNode,
} from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import type { PhoneAccessStatus } from "@/lib/tauri";
import PhoneAccessControl from "./PhoneAccessControl";

function status(over: Partial<PhoneAccessStatus> = {}): PhoneAccessStatus {
  return {
    state: "off",
    message: "Phone access is off",
    tailnet_url: null,
    enabled: false,
    ownership: "none",
    interrupted: false,
    gateway_routes: false,
    funnel_present: false,
    ...over,
  };
}

function props(over: Partial<Parameters<typeof PhoneAccessControl>[0]> = {}) {
  return {
    status: status(),
    busy: false,
    onEnable: vi.fn(),
    onDisable: vi.fn(),
    notify: vi.fn(),
    ...over,
  };
}

function render(over: Partial<Parameters<typeof PhoneAccessControl>[0]> = {}) {
  return renderToStaticMarkup(<PhoneAccessControl {...props(over)} />);
}

type TestElementProps = {
  children?: ReactNode;
  "data-testid"?: string;
  "aria-label"?: string;
  onClick?: () => void;
  disabled?: boolean;
};

function findByTestId(node: ReactNode, testId: string): ReactElement<TestElementProps> {
  if (isValidElement<TestElementProps>(node)) {
    if (node.props["data-testid"] === testId) return node;
    let match: ReactElement<TestElementProps> | null = null;
    Children.forEach(node.props.children, (child) => {
      if (match) return;
      try {
        match = findByTestId(child, testId);
      } catch {
        // Keep walking sibling elements.
      }
    });
    if (match) return match;
  }
  throw new Error(`Missing test id: ${testId}`);
}

function tree(over: Partial<Parameters<typeof PhoneAccessControl>[0]> = {}) {
  return PhoneAccessControl(props(over));
}

describe("Phone access control rendering", () => {
  it("renders the off state with enable available and disable unavailable", () => {
    const html = render();
    expect(html).toContain('data-phone-access-state="off"');
    expect(html).toContain("Phone access is off");
    expect(findByTestId(tree(), "settings-phone-access-enable").props.disabled).toBe(false);
    expect(findByTestId(tree(), "settings-phone-access-disable").props.disabled).toBe(true);
  });

  it("renders a ready address, gateway routing, copy affordance, and refresh action", () => {
    const html = render({
      status: status({
        state: "ready",
        message: "Private access is ready",
        tailnet_url: "https://irin.example.ts.net:8443",
        enabled: true,
        gateway_routes: true,
      }),
    });
    expect(html).toContain('data-phone-access-state="ready"');
    expect(html).toContain("https://irin.example.ts.net:8443");
    expect(html).toContain("Gateway routes: included");
    expect(html).toContain("Refresh iPhone routes");
    expect(html).toContain('aria-label="Copy iPhone address"');
  });

  it("disables both actions while an operation is busy", () => {
    const renderedTree = tree({ busy: true });
    expect(findByTestId(renderedTree, "settings-phone-access-enable").props.disabled).toBe(true);
    expect(findByTestId(renderedTree, "settings-phone-access-disable").props.disabled).toBe(true);
    const html = render({ busy: true });
    expect(html).toContain('aria-busy="true"');
    expect(html).toContain("animate-spin");
  });

  it.each([
    ["command_error", "tailscale command failed"],
    ["funnel_present", "Public Funnel must be removed"],
    ["foreign_unowned", "Existing routes are not owned by IRIN"],
  ] as const)("renders the %s failure state without hiding its explanation", (stateName, message) => {
    const html = render({ status: status({ state: stateName, message }) });
    expect(html).toContain(`data-phone-access-state="${stateName}"`);
    expect(html).toContain(stateName.replaceAll("_", " "));
    expect(html).toContain(message);
    expect(html).toContain("Enable iPhone access");
  });

  it("does not render native-only or unexpected sensitive fields", () => {
    const unsafe = {
      ...status({ state: "ready", enabled: true }),
      ownership: "private-owner-receipt",
      auth_token: "do-not-render-token",
      signature: "do-not-render-signature",
    } as PhoneAccessStatus;
    const html = render({ status: unsafe });
    expect(html).not.toContain("private-owner-receipt");
    expect(html).not.toContain("do-not-render-token");
    expect(html).not.toContain("do-not-render-signature");
  });
});

describe("Phone access control actions", () => {
  it("dispatches enable and disable through the supplied handlers", () => {
    const onEnable = vi.fn();
    const onDisable = vi.fn();
    const offTree = tree({ onEnable });
    findByTestId(offTree, "settings-phone-access-enable").props.onClick?.();
    expect(onEnable).toHaveBeenCalledOnce();

    const readyTree = tree({
      status: status({ state: "ready", enabled: true }),
      onDisable,
    });
    findByTestId(readyTree, "settings-phone-access-disable").props.onClick?.();
    expect(onDisable).toHaveBeenCalledOnce();
  });

  it("offers recovery disable when interrupted with publication not enabled", () => {
    const onDisable = vi.fn();
    const interruptedTree = tree({
      status: status({
        state: "interrupted_change",
        message:
          "A previous phone-access change was interrupted. Disable or re-enable to recover.",
        enabled: false,
        interrupted: true,
      }),
      onDisable,
    });
    const html = render({
      status: status({
        state: "interrupted_change",
        enabled: false,
        interrupted: true,
      }),
    });
    expect(html).toContain('data-phone-access-interrupted="true"');
    expect(html).toContain("Clear interrupted setup");
    expect(html).not.toContain("Refresh iPhone routes");
    const recover = findByTestId(interruptedTree, "settings-phone-access-disable");
    expect(recover.props.disabled).toBe(false);
    expect(recover.props["aria-label"]).toBe("Clear interrupted setup");
    recover.props.onClick?.();
    expect(onDisable).toHaveBeenCalledOnce();
  });

  it("copies only the rendered tailnet address including port and reports success", async () => {
    const copyText = vi.fn().mockResolvedValue(undefined);
    const notify = vi.fn();
    const renderedTree = tree({
      status: status({
        state: "ready",
        enabled: true,
        tailnet_url: "https://irin.example.ts.net:8443",
      }),
      copyText,
      notify,
    });
    findByTestId(renderedTree, "settings-phone-access-copy").props.onClick?.();
    await vi.waitFor(() => {
      expect(copyText).toHaveBeenCalledWith("https://irin.example.ts.net:8443");
      expect(notify).toHaveBeenCalledWith("success", "iPhone address copied");
    });
  });

  it("reports clipboard failure without exposing the address in the error", async () => {
    const copyText = vi.fn().mockRejectedValue(new Error("clipboard denied"));
    const notify = vi.fn();
    const renderedTree = tree({
      status: status({
        state: "ready",
        enabled: true,
        tailnet_url: "https://irin.example.ts.net:8443",
      }),
      copyText,
      notify,
    });
    findByTestId(renderedTree, "settings-phone-access-copy").props.onClick?.();
    await vi.waitFor(() => {
      expect(notify).toHaveBeenCalledWith(
        "error",
        "Could not copy the iPhone address",
      );
    });
  });
});
