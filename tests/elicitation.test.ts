import { describe, expect, it } from "vitest";
import { safeHttpUrl } from "../web/src/lib/safe-url";

describe("URL elicitation safety", () => {
  it("allows HTTP(S) flows and blocks executable or malformed URLs", () => {
    expect(safeHttpUrl("https://example.test/connect")).toBe(
      "https://example.test/connect",
    );
    expect(safeHttpUrl("http://127.0.0.1:8080/device")).toBe(
      "http://127.0.0.1:8080/device",
    );
    expect(safeHttpUrl("javascript:alert(1)")).toBeUndefined();
    expect(safeHttpUrl("not a URL")).toBeUndefined();
  });
});
