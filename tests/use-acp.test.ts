// @vitest-environment happy-dom

import { describe, expect, it } from "vitest";
import {
  parseGlobalBusinessEvent,
  parseSessionBusinessEvent,
  strongEtag,
} from "../shared/business-api";
import { workspaceContextSearchPath } from "../web/src/lib/api-client";
import type { TerminalSnapshot } from "../shared/bridge";

function terminalDelta(terminal: Record<string, unknown>, fromRevision = 1, overrides: Record<string, unknown> = {}) {
  return {
    type: "bridge/session_delta", bridgeEpoch: "epoch", sessionId: "alpha-one",
    sessionIncarnation: 1, fromRevision, viewRevision: fromRevision + 1,
    change: { kind: "terminal_update", terminal },
    ...overrides,
  };
}

function terminalSnapshot(overrides: Partial<TerminalSnapshot> = {}): TerminalSnapshot {
  return {
    sessionId: "alpha-one", terminalId: "terminal-1", output: "", truncated: false, released: false,
    ...overrides,
  };
}

describe("browser REST and SSE transport", () => {
  it("encodes the authoritative history revision as a strong If-Match value", () => {
    expect(strongEtag("epoch:1:7")).toBe('"epoch:1:7"');
    expect(() => strongEtag("")).toThrow("History revision");
    expect(() => strongEtag('bad"revision')).toThrow("History revision");
  });

  it("encodes context search text and session identity in the v2 files path", () => {
    expect(workspaceContextSearchPath("src/a.ts & tests", "project/session"))
      .toBe("/api/v2/sessions/project%2Fsession/files?query=src%2Fa.ts+%26+tests");
  });

  it("accepts only the global business event vocabulary", () => {
    expect(parseGlobalBusinessEvent(JSON.stringify({
      type: "bridge/connection",
      phase: "ready",
    }))).toEqual({ type: "bridge/connection", phase: "ready" });
    expect(() => parseGlobalBusinessEvent(JSON.stringify({
      type: "acp/session_update",
    }))).toThrow("Unknown global event");
  });

  it("rejects malformed or raw ACP events on the session stream", () => {
    expect(parseSessionBusinessEvent(JSON.stringify({
      type: "bridge/session_delta",
      bridgeEpoch: "epoch",
      sessionId: "session",
      sessionIncarnation: 2,
      fromRevision: 6,
      viewRevision: 7,
      change: { kind: "turn_update", update: { sessionUpdate: "agent_message_chunk" } },
    }))).toMatchObject({ viewRevision: 7 });
    expect(() => parseSessionBusinessEvent(JSON.stringify({
      type: "acp/session_update",
      notification: {},
    }))).toThrow("Unknown session event");
    expect(() => parseSessionBusinessEvent(JSON.stringify({
      type: "bridge/session_reset",
      bridgeEpoch: "epoch",
      sessionId: "session",
      sessionIncarnation: 1,
      viewRevision: "7",
    }))).toThrow("invalid identity or revision");
    expect(parseSessionBusinessEvent(JSON.stringify({
      type: "bridge/session_turn_failed",
      bridgeEpoch: "epoch",
      sessionId: "session",
      sessionIncarnation: 2,
      viewRevision: 8,
      historyRevision: "epoch:2:8",
      phase: "ready",
      operationId: "turn-1",
      clientIntentId: "intent-1",
      prompt: [{ type: "text", text: "Retry me" }],
      error: { code: -32603, message: "Agent failed", data: { retry: true } },
    }))).toMatchObject({
      type: "bridge/session_turn_failed",
      error: { code: -32603 },
    });
    expect(() => parseSessionBusinessEvent(JSON.stringify({
      type: "bridge/session_turn_complete",
      bridgeEpoch: "epoch",
      sessionId: "session",
      sessionIncarnation: 2,
      viewRevision: 8,
      historyRevision: "epoch:2:8",
      phase: "ready",
      operationId: "turn-1",
      clientIntentId: "intent-1",
      response: "invalid",
    }))).toThrow("invalid payload");
  });

  it("validates terminal delta payloads and requires the envelope session identity", () => {
    const terminal = terminalSnapshot({ output: "streamed", outputAppend: true, retainedBytes: 8 });
    expect(parseSessionBusinessEvent(JSON.stringify(terminalDelta({ ...terminal }))))
      .toMatchObject({ change: { kind: "terminal_update", terminal } });
    for (const invalid of [
      { ...terminal, sessionId: "beta-one" },
      { ...terminal, terminalId: null },
      { ...terminal, output: 1 },
      { ...terminal, released: undefined },
      { ...terminal, outputAppend: "yes" },
      { ...terminal, retainedBytes: -1 },
      { ...terminal, exitStatus: { exitCode: -1 } },
    ]) {
      expect(() => parseSessionBusinessEvent(JSON.stringify(terminalDelta(invalid)))).toThrow();
    }
  });

});

