import type { SessionNotification, SessionUpdate } from "@agentclientprotocol/sdk";
import { describe, expect, it } from "vitest";
import type { BridgeSessionView } from "../web/src/lib/business-api";
import { appReducer, initialState, type AppState } from "../web/src/lib/state";

const sessionId = "efficiency-session";
const prompt = [{ type: "text" as const, text: "Inspect output" }];

function view(updates: SessionUpdate[] = []): BridgeSessionView {
  return {
    bridgeEpoch: "efficiency-epoch", sessionId, sessionIncarnation: 1,
    viewRevision: 1, historyRevision: "history-1", phase: "running", syncError: null,
    timeline: [], activeTurn: { operationId: "op", clientIntentId: "intent", prompt, updates, terminal: null },
    workspace: { cwd: "/work", session: {} }, controls: {},
    interactions: { permissions: {}, elicitations: {}, urlFlows: {} }, operation: null, terminals: {},
  };
}

function hydrate(updates: SessionUpdate[] = []): AppState {
  return appReducer(initialState, { type: "bridge/session_hydrate", view: view(updates) });
}

// This is the real reducer action emitted by useAcp for an accepted
// bridge/session_delta with change.kind=turn_update. The REST/SSE owner and
// ordering fences remain covered by runtime-memory-retention.test.tsx.
function apply(state: AppState, update: SessionUpdate): AppState {
  return appReducer(state, {
    type: "server/event", event: { type: "acp/session_update", notification: { sessionId, update } },
  });
}

function diagnosticBytes(raw: unknown[]): number {
  // Logical UTF-8 diagnostic payload, not browser heap/RSS (strings may share).
  return new TextEncoder().encode(JSON.stringify(raw)).length;
}

function tool(text: string): SessionUpdate {
  return {
    sessionUpdate: "tool_call", toolCallId: "tool", title: "Read", kind: "read",
    status: "in_progress", rawInput: { path: "/work/file" },
    content: [{ type: "content", content: { type: "text", text } }],
  };
}

describe("runtime memory efficiency: current diagnostic state", () => {
  it.each([8, 32, 128])("E06 keeps only the latest diagnostic after %i cumulative tool replacements", (count) => {
    let state = apply(hydrate(), tool(""));
    let latest!: SessionNotification;
    for (let index = 1; index <= count; index++) {
      const text = "字🙂".repeat(8192 * index / count);
      const update: SessionUpdate = {
        sessionUpdate: "tool_call_update", toolCallId: "tool",
        status: index === count ? "completed" : "in_progress",
        rawOutput: { final: index === count, characters: text.length },
        content: [{ type: "content", content: { type: "text", text } }],
      };
      latest = { sessionId, update };
      state = apply(state, update);
    }
    const tools = state.timeline.filter((item) => item.type === "tool");
    expect(tools).toHaveLength(1);
    expect(tools[0].call).toMatchObject({
      title: "Read", kind: "read", status: "completed", rawInput: { path: "/work/file" },
      rawOutput: { final: true, characters: "字🙂".repeat(8192).length },
      content: [{ type: "content", content: { type: "text", text: "字🙂".repeat(8192) } }],
    });
    expect(state.running).toBe(true); // tool completion is not turn completion
    expect(state.pendingPrompt?.blocks).toEqual(prompt);
    expect(tools[0].raw.at(-1)).toEqual(latest);
    expect(diagnosticBytes(tools[0].raw), "diagnostics must not archive earlier cumulative bodies")
      .toBe(diagnosticBytes([latest]));
    expect(tools[0].raw).toHaveLength(1);
  });

  it.each(["agent_message_chunk", "agent_thought_chunk", "user_message_chunk"] as const)(
    "E06 keeps complete %s text while replacing its diagnostic source",
    (sessionUpdate) => {
      let state = hydrate();
      const pieces = Array.from({ length: 32 }, (_, index) => `${index}:片段🙂\n`);
      let latest!: SessionNotification;
      for (const text of pieces) {
        const update: SessionUpdate = { sessionUpdate, messageId: "message", content: { type: "text", text } };
        latest = { sessionId, update };
        state = apply(state, update);
      }
      if (sessionUpdate === "user_message_chunk") {
        const messages = state.timeline.filter((item) => item.type === "message" && item.messageId === "message");
        expect(messages).toHaveLength(1);
        expect(messages[0].blocks).toEqual([{ type: "text", text: pieces.join("") }]);
        expect(messages[0].raw.at(-1)).toEqual(latest);
        expect(messages[0].raw).toHaveLength(1);
      } else {
        const messages = state.timeline.filter((item) => item.type === "assistant");
        expect(messages).toHaveLength(1);
        expect(messages[0].chunks).toHaveLength(1);
        expect(messages[0].chunks[0].blocks).toEqual([{ type: "text", text: pieces.join("") }]);
        expect(messages[0].chunks[0].raw.at(-1)).toEqual(latest);
        expect(messages[0].chunks[0].raw).toHaveLength(1);
      }
    },
  );

  it("E06 retains the latest plan removal and compaction failure without their superseded raw updates", () => {
    let state = hydrate();
    for (let index = 0; index < 32; index++) {
      state = apply(state, { sessionUpdate: "plan_update", plan: { type: "markdown", planId: "plan", content: `plan ${index}` } });
      state = apply(state, { sessionUpdate: "compaction_update", compactionId: "compact", status: "in_progress", summary: [{ type: "text", text: `summary ${index}` }] });
    }
    const removal: SessionUpdate = { sessionUpdate: "plan_removed", planId: "plan" };
    const failure: SessionUpdate = { sessionUpdate: "compaction_update", compactionId: "compact", status: "failed", error: "compaction failed" };
    state = apply(apply(state, removal), failure);
    const plans = state.timeline.filter((item) => item.type === "plan");
    const compactions = state.timeline.filter((item) => item.type === "compaction");
    expect(plans).toHaveLength(1);
    expect(compactions).toHaveLength(1);
    expect(plans[0].update).toEqual(removal);
    expect(compactions[0]).toMatchObject({ status: "failed", error: "compaction failed", blocks: [{ type: "text", text: "summary 31" }] });
    expect(plans[0].raw.at(-1)).toEqual({ sessionId, update: removal });
    expect(compactions[0].raw.at(-1)).toEqual({ sessionId, update: failure });
    expect(plans[0].raw).toHaveLength(1);
    expect(compactions[0].raw).toHaveLength(1);
  });

  it("E06 preserves every optimistic prompt block but only the latest echo source", () => {
    const blocks = [{ type: "text" as const, text: "Read this" }, { type: "resource_link" as const, uri: "file:///work/file", name: "file" }];
    let state = appReducer({ ...initialState, session: { sessionId } }, {
      type: "user/prompt", requestId: "echo-intent", sessionId, blocks,
    });
    for (const content of blocks) {
      state = apply(state, { sessionUpdate: "user_message_chunk", messageId: "echo", content });
    }
    expect(state.timeline).toHaveLength(1);
    const message = state.timeline[0];
    expect(message).toMatchObject({ type: "message", role: "user", messageId: "echo", blocks, echoBlocks: [] });
    if (message.type !== "message") throw new Error("expected the original optimistic prompt");
    expect(message.raw.at(-1)).toMatchObject({ update: { messageId: "echo", content: blocks[1] } });
    expect(message.raw).toHaveLength(1);
  });

  it("E06 appends anonymous assistant chunks without keeping their diagnostic archive", () => {
    let state = hydrate();
    const pieces = Array.from({ length: 32 }, (_, index) => `anonymous ${index}🙂\n`);
    for (const text of pieces) {
      state = apply(state, { sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
    }
    const messages = state.timeline.filter((item) => item.type === "assistant");
    expect(messages).toHaveLength(1);
    expect(messages[0].chunks).toHaveLength(1);
    const chunk = messages[0].chunks[0];
    expect(chunk.blocks).toEqual([{ type: "text", text: pieces.join("") }]);
    expect(chunk.raw.at(-1)).toMatchObject({ update: { content: { text: pieces.at(-1) } } });
    expect(chunk.raw).toHaveLength(1);
  });

  it("E06 promotes the current legacy plan without promoting all diagnostic versions", () => {
    let state = hydrate();
    let latest!: SessionUpdate;
    for (let index = 0; index < 32; index++) {
      latest = { sessionUpdate: "plan", entries: [{ content: `step ${index}`, priority: "high", status: "in_progress" }] };
      state = apply(state, latest);
    }
    expect(state.activePlan?.update).toEqual(latest);
    state = appReducer(state, { type: "bridge/turn_complete", event: {
      type: "bridge/session_turn_complete", bridgeEpoch: "efficiency-epoch", sessionId,
      sessionIncarnation: 1, viewRevision: 2, historyRevision: "history-2", phase: "ready",
      operationId: "op", clientIntentId: "intent", response: { stopReason: "end_turn" },
    } });
    expect(state.activePlan).toBeUndefined();
    const plans = state.timeline.filter((item) => item.type === "plan");
    expect(plans).toHaveLength(1);
    expect(plans[0].update).toEqual(latest);
    expect(plans[0].raw.at(-1)).toEqual({ sessionId, update: latest });
    expect(state.timeline.filter((item) => item.type === "stop")).toHaveLength(1);
    expect(plans[0].raw).toHaveLength(1);
  });

  it("E07 preserves current business input/output, permission and retry data across sparse tool updates", () => {
    let state = apply(hydrate(), tool("complete current output"));
    state = apply(state, { sessionUpdate: "tool_call_update", toolCallId: "tool", rawOutput: { result: "current result" } });
    state = appReducer(state, { type: "server/event", event: {
      type: "acp/permission_request", permissionId: "permission",
      request: { sessionId, toolCall: { toolCallId: "tool", title: "Allow read?", rawInput: { path: "/work/file" } }, options: [{ optionId: "allow", name: "Allow", kind: "allow_once" }] },
    } });
    state = appReducer(state, { type: "permission/respond_start", permissionId: "permission", requestId: "response-in-flight" });
    state = apply(state, { sessionUpdate: "tool_call_update", toolCallId: "tool", status: "in_progress" });
    const current = state.timeline.find((item) => item.type === "tool")!;
    expect(current.call).toMatchObject({ rawInput: { path: "/work/file" }, rawOutput: { result: "current result" }, content: [{ type: "content", content: { type: "text", text: "complete current output" } }] });
    expect(state.permissions).toEqual([{
      permissionId: "permission", responseRequestId: "response-in-flight", responseError: undefined,
      request: { sessionId, toolCall: { toolCallId: "tool", title: "Allow read?", rawInput: { path: "/work/file" } }, options: [{ optionId: "allow", name: "Allow", kind: "allow_once" }] },
    }]);
    state = appReducer(state, { type: "bridge/turn_failed", event: {
      type: "bridge/session_turn_failed", bridgeEpoch: "efficiency-epoch", sessionId,
      sessionIncarnation: 1, viewRevision: 2, historyRevision: "history-2", phase: "ready",
      operationId: "op", clientIntentId: "intent", prompt,
      error: { code: -32603, message: "agent failed", data: { detail: "full error detail" } },
    } });
    expect(state.timeline.filter((item) => item.type === "error")).toEqual([
      expect.objectContaining({ message: "agent failed", retryBlocks: prompt, data: { detail: "full error detail" } }),
    ]);
  });

  it("E07 reconstructs complete current output from a REST view with one diagnostic source", () => {
    const current = tool("full output 🙂".repeat(4096));
    const state = hydrate([current]);
    const tools = state.timeline.filter((item) => item.type === "tool");
    expect(tools).toHaveLength(1);
    expect(tools[0].call).toMatchObject(current);
    expect(tools[0].raw).toEqual([{ sessionId, update: current }]);
    expect(state.pendingPrompt?.blocks).toEqual(prompt);
    expect(state.running).toBe(true);
  });
});
