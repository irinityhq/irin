import { describe, expect, it } from "vitest";
import { safeMarkdownUrl } from "./markdown-url";

describe("safeMarkdownUrl", () => {
  it.each([
    "https://example.com/path",
    "http://localhost:3000/path",
    "mailto:operator@example.com",
    "/relative/path",
    "./relative/path",
    "../relative/path",
    "#anchor",
  ])("allows safe URL %s", (url) => {
    expect(safeMarkdownUrl(url)).toBe(url);
  });

  it.each([
    "javascript:alert(1)",
    "JaVaScRiPt:alert(1)",
    "java\tscript:alert(1)",
    "data:text/html,<script>alert(1)</script>",
    "vbscript:msgbox(1)",
    "file:///etc/passwd",
  ])("blocks unsafe URL %s", (url) => {
    expect(safeMarkdownUrl(url)).toBe("");
  });

  it("trims safe URLs and rejects empty input", () => {
    expect(safeMarkdownUrl("  https://example.com/path  ")).toBe(
      "https://example.com/path",
    );
    expect(safeMarkdownUrl("   ")).toBe("");
    expect(safeMarkdownUrl(undefined)).toBe("");
  });
});
