import { describe, expect, it } from "vitest";
import { classifyAskFailure, isAbortError } from "./librarian-abort";

// feature contract contract: aborting librarian.ask must NOT surface in the error
// banner — the view classifies the rejection and recovers chat state instead.
describe("isAbortError", () => {
  it("matches the DOMException fetch abort rejection", () => {
    expect(isAbortError(new DOMException("The user aborted a request.", "AbortError"))).toBe(true);
  });
  it("matches plain Errors named AbortError (wrapped environments)", () => {
    const e = new Error("aborted");
    e.name = "AbortError";
    expect(isAbortError(e)).toBe(true);
  });
  it("rejects ordinary failures", () => {
    expect(isAbortError(new Error("503: librarian busy"))).toBe(false);
    expect(isAbortError(new DOMException("boom", "NetworkError"))).toBe(false);
    expect(isAbortError("AbortError")).toBe(false);
    expect(isAbortError(null)).toBe(false);
  });
});

describe("classifyAskFailure", () => {
  it("classifies AbortError as aborted regardless of signal state", () => {
    const abort = new DOMException("x", "AbortError");
    expect(classifyAskFailure(abort, false)).toBe("aborted");
    expect(classifyAskFailure(abort, true)).toBe("aborted");
  });
  it("trusts the fired signal even when the error is wrapped", () => {
    expect(classifyAskFailure(new Error("fetch failed"), true)).toBe("aborted");
  });
  it("classifies real failures as error", () => {
    expect(classifyAskFailure(new Error("500: upstream"), false)).toBe("error");
  });
  it("mirrors a real AbortController firing", () => {
    const c = new AbortController();
    c.abort();
    expect(classifyAskFailure(c.signal.reason, c.signal.aborted)).toBe("aborted");
  });
});
