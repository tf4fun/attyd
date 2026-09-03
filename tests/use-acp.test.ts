import { describe, expect, it } from "vitest";
import {
  parseGlobalBusinessEvent,
  parseSessionBusinessEvent,
  strongEtag,
  workspaceContextSearchPath,
} from "../web/src/lib/business-api";
import { mostRecentSession } from "../web/src/lib/use-acp";

describe("browser REST and SSE transport", () => {
  it("encodes the authoritative history revision as a strong If-Match value", () => {
    expect(strongEtag("epoch:1:7")).toBe('"epoch:1:7"');
    expect(() => strongEtag("")).toThrow("History revision");
    expect(() => strongEtag('bad"revision')).toThrow("History revision");
  });

  it("encodes context search text as a query parameter", () => {
    expect(workspaceContextSearchPath("src/a.ts & tests"))
      .toBe("/api/v1/context/search?query=src%2Fa.ts+%26+tests");
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

  it("restores the newest timestamped session without depending on Agent ordering", () => {
    expect(mostRecentSession([
      { sessionId: "older", cwd: "/workspace", updatedAt: "2026-08-20T00:00:00Z" },
      { sessionId: "newest", cwd: "/workspace", updatedAt: "2026-08-30T00:00:00Z" },
      { sessionId: "undated", cwd: "/workspace" },
    ])?.sessionId).toBe("newest");
    expect(mostRecentSession([
      { sessionId: "first", cwd: "/workspace" },
      { sessionId: "second", cwd: "/workspace" },
    ])?.sessionId).toBe("first");
    expect(mostRecentSession([])).toBeUndefined();
  });
});
