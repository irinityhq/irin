import { describe, expect, it } from "vitest";
import { parseApiErrorBody } from "./api";

function mockResponse(
  status: number,
  body: string,
  statusText = "Error",
): Response {
  return new Response(body, { status, statusText });
}

describe("parseApiErrorBody", () => {
  it("reads error field from JSON", async () => {
    const msg = await parseApiErrorBody(
      mockResponse(500, JSON.stringify({ error: "Sessions directory not found" })),
    );
    expect(msg).toBe("Sessions directory not found");
  });

  it("falls back to status line when body empty", async () => {
    const msg = await parseApiErrorBody(mockResponse(500, ""));
    expect(msg).toBe("500 Error");
  });

  it("uses raw text for non-JSON bodies", async () => {
    const msg = await parseApiErrorBody(mockResponse(502, "upstream timeout"));
    expect(msg).toBe("upstream timeout");
  });
});