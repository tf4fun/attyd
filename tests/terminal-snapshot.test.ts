import { describe, expect, it } from "vitest";
import { parseTerminalSnapshot } from "../shared/bridge";

describe("terminal snapshot validation", () => {
  it("accepts a complete terminal snapshot", () => {
    expect(parseTerminalSnapshot({
      sessionId: "session",
      terminalId: "terminal",
      output: "done",
      outputBytes: "ZG9uZQ==",
      truncated: false,
      exitStatus: { exitCode: 0, signal: null },
      released: true,
    })).toMatchObject({ terminalId: "terminal", released: true });
  });

  it("rejects malformed terminal fields", () => {
    const base = { sessionId: "session", terminalId: "terminal", output: "", truncated: false, released: false };
    for (const outputBytes of ["%%%", "a===", "abcd\n", 5]) {
      expect(() => parseTerminalSnapshot({ ...base, outputBytes })).toThrow("outputBytes");
    }
    for (const exitCode of [-1, 0x1_0000_0000]) {
      expect(() => parseTerminalSnapshot({ ...base, exitStatus: { exitCode, signal: null } })).toThrow("uint32");
    }
    expect(() => parseTerminalSnapshot({ ...base, truncated: "no" })).toThrow("requires truncated");
    expect(() => parseTerminalSnapshot({ ...base, released: undefined })).toThrow("requires released");
    expect(() => parseTerminalSnapshot({ ...base, terminalId: "" })).toThrow("terminalId");
    expect(() => parseTerminalSnapshot({ ...base, retainedBytes: -1 })).toThrow("retainedBytes");
    expect(() => parseTerminalSnapshot(null)).toThrow("must be an object");
  });
});
