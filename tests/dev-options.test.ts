import { describe, expect, it } from "vitest";
import { devPort, hasDevServer } from "../scripts/dev-options";

describe("development runner options", () => {
  it("accepts explicit and default Vite ports", () => {
    expect(devPort(undefined)).toBe(5173);
    expect(devPort("1")).toBe(1);
    expect(devPort("5180")).toBe(5180);
    expect(devPort("65535")).toBe(65535);
  });

  it("rejects malformed or out-of-range ports", () => {
    for (const value of ["", " ", "0", "-1", "65536", "1.2", "5180junk", "Infinity"]) {
      expect(() => devPort(value)).toThrow("ATTYD_DEV_PORT");
    }
  });

  it("reuses an explicitly configured frontend without confusing option values", () => {
    expect(hasDevServer(["--dev-server", "http://127.0.0.1:5180", "--", "agent"])).toBe(true);
    expect(hasDevServer(["--port", "7334", "--session-unobserved-timeout", "-1", "--dev-server=http://127.0.0.1:5180", "agent"])).toBe(true);
    expect(hasDevServer(["--cwd", "--dev-server", "agent"])).toBe(false);
  });

  it("leaves Agent options alone with explicit and implicit command boundaries", () => {
    expect(hasDevServer(["--", "agent", "--dev-server", "http://example.test"])).toBe(false);
    expect(hasDevServer(["--port", "7334", "agent", "--dev-server=http://example.test"])).toBe(false);
    expect(hasDevServer(["-p7334", "--read-only", "agent", "--dev-server"])).toBe(false);
  });
});
