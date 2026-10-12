import { describe, expect, it } from "vitest";
import { parseTerminalSnapshot } from "../shared/bridge";
import { parseSessionBusinessEvent } from "../shared/business-api";

function terminalDelta(terminal: unknown, sessionId = "session") {
  return JSON.stringify({
    type: "bridge/session_delta",
    bridgeEpoch: "epoch",
    sessionId,
    sessionIncarnation: 1,
    fromRevision: 1,
    viewRevision: 2,
    change: { kind: "terminal_update", terminal },
  });
}

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

  it("validates terminal deltas against their session identity", () => {
    const terminal = { sessionId: "session", terminalId: "terminal", output: "x", truncated: false, released: false };
    expect(parseSessionBusinessEvent(terminalDelta(terminal))).toMatchObject({
      change: { kind: "terminal_update", terminal: { terminalId: "terminal" } },
    });
    expect(() => parseSessionBusinessEvent(terminalDelta(terminal, "other")))
      .toThrow("does not match its session identity");
    expect(() => parseSessionBusinessEvent(terminalDelta({ ...terminal, outputBytes: "%%%" })))
      .toThrow("outputBytes");
  });
});
