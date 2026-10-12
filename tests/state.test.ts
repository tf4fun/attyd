import { describe, expect, it } from "vitest";
import type { PromptResponse, SessionUpdate } from "@agentclientprotocol/sdk";
import type {
  BridgeSessionView,
  GlobalBusinessEvent,
  RuntimeView,
  SessionOwner,
} from "../shared/business-api";
import { appReducer, initialState, type AppAction, type AppState, type BridgeError } from "../web/src/lib/state";

function requestFailed(error: BridgeError): AppAction {
  return { type: "request/failed", error };
}

function globalEvent(event: GlobalBusinessEvent): AppAction {
  return { type: "global/event", event };
}

function runtime(
  value: NonNullable<RuntimeView["hello"]> | NonNullable<RuntimeView["initialized"]>,
): AppAction {
  return {
    type: "runtime/loaded",
    runtime: {
      generation: 1, connected: true, hello: null, initialized: null, phase: null, error: null,
      ...(value.type === "bridge/hello" ? { hello: value } : { initialized: value }),
    },
  };
}

function owner(sessionId: string, sessionIncarnation = 1): SessionOwner {
  return { bridgeEpoch: "epoch", sessionId, sessionIncarnation };
}

function sessionView(sessionId: string, patch: Partial<BridgeSessionView> = {}): BridgeSessionView {
  return {
    ...owner(sessionId), viewRevision: 1, historyRevision: "revision", phase: "ready", syncError: null,
    timeline: [], activeTurn: null, workspace: { cwd: "/workspace", session: {} }, controls: {},
    interactions: { permissions: {}, elicitations: {}, urlFlows: {} }, operation: null, terminals: {},
    ...patch,
  };
}

function turnComplete(sessionId: string, clientIntentId: string, response: PromptResponse): AppAction {
  return {
    type: "bridge/turn_complete",
    event: {
      type: "bridge/session_turn_complete", ...owner(sessionId), viewRevision: 1, historyRevision: null,
      phase: "ready", operationId: `operation-${clientIntentId}`, clientIntentId, response,
    },
  };
}

describe("ACP UI state", () => {
  it("projects a pending cancellation without ending the turn and accepts late tool results", () => {
    const view: BridgeSessionView = {
      bridgeEpoch: "epoch", sessionId: "s1", sessionIncarnation: 1, viewRevision: 3,
      historyRevision: "revision", phase: "running", syncError: null,
      timeline: [
        { sessionUpdate: "user_message_chunk", content: { type: "text", text: "Earlier" } },
        { sessionUpdate: "tool_call", toolCallId: "earlier", title: "Earlier tool", status: "in_progress" },
      ],
      activeTurn: { operationId: "op", clientIntentId: "intent", cancelRequested: true,
        prompt: [{ type: "text", text: "Run" }], terminal: null,
        updates: [
          { sessionUpdate: "tool_call", toolCallId: "tool", title: "Run", status: "in_progress" },
          { sessionUpdate: "tool_call", toolCallId: "done", title: "Done", status: "completed" },
        ],
      },
      workspace: { cwd: "/workspace", session: {} }, controls: {},
      interactions: { permissions: {}, elicitations: {}, urlFlows: {} }, operation: null, terminals: {},
    };
    let state = appReducer(initialState, { type: "bridge/session_hydrate", view });
    expect(state.running).toBe(true);
    expect(state.pendingPrompt).toMatchObject({ cancelRequested: true });
    expect(state.timeline.some((item) => item.type === "stop")).toBe(false);
    expect(state.timeline.find((item) => item.type === "tool" && item.call.toolCallId === "earlier"))
      .not.toHaveProperty("cancelled", true);
    expect(state.timeline.find((item) => item.type === "tool" && item.call.toolCallId === "tool"))
      .toMatchObject({ cancelled: true, call: { status: "in_progress" } });
    expect(state.timeline.find((item) => item.type === "tool" && item.call.toolCallId === "done"))
      .not.toHaveProperty("cancelled", true);
    state = appReducer(state, ({ type: "session/turn_update", notification: { sessionId: "s1", update: {
      sessionUpdate: "tool_call_update", toolCallId: "tool", status: "completed",
      content: [{ type: "content", content: { type: "text", text: "Late result" } }],
    } } }));
    expect(state.timeline.find((item) => item.type === "tool" && item.call.toolCallId === "tool"))
      .toMatchObject({ call: { status: "completed", content: [{ content: { text: "Late result" } }] } });
    expect(state.timeline.find((item) => item.type === "tool" && item.call.toolCallId === "tool"))
      .not.toHaveProperty("cancelled", true);
    state = appReducer(state, ({ type: "session/turn_update", notification: { sessionId: "s1", update: {
      sessionUpdate: "tool_call", toolCallId: "late", title: "Late tool", status: "pending",
    } } }));
    expect(state.timeline.find((item) => item.type === "tool" && item.call.toolCallId === "late"))
      .toHaveProperty("cancelled", true);
    state = appReducer(state, { type: "bridge/session_hydrate", view: {
      ...view, viewRevision: 4, activeTurn: { ...view.activeTurn!, operationId: "new-op", cancelRequested: false, updates: [] },
    } });
    expect(state.pendingPrompt?.cancelRequested).not.toBe(true);
  });

  it("rebuilds cancelled tool display from completed and reconciling memory views", () => {
    const updates: SessionUpdate[] = [
      { sessionUpdate: "user_message_chunk", content: { type: "text", text: "Run" } },
      { sessionUpdate: "tool_call", toolCallId: "tool", title: "Run", status: "in_progress" },
    ];
    const view: BridgeSessionView = {
      bridgeEpoch: "epoch", sessionId: "s1", sessionIncarnation: 1, viewRevision: 3,
      historyRevision: "revision", phase: "ready", syncError: null,
      timeline: updates, turnOutcomes: [{ operationId: "op", afterUpdate: 2, response: { stopReason: "cancelled" } }],
      activeTurn: null, workspace: { cwd: "/workspace", session: {} }, controls: {},
      interactions: { permissions: {}, elicitations: {}, urlFlows: {} }, operation: null, terminals: {},
    };
    const completed = appReducer(initialState, { type: "bridge/session_hydrate", view });
    expect(completed.timeline.find((item) => item.type === "tool")).toMatchObject({ cancelled: true, call: { status: "in_progress" } });
    const reconciling = appReducer(initialState, { type: "bridge/session_hydrate", view: {
      ...view, phase: "reconciling", timeline: [], turnOutcomes: [], activeTurn: {
        operationId: "op", clientIntentId: "intent", prompt: [{ type: "text", text: "Run" }], updates: updates.slice(1), terminal: { stopReason: "cancelled" },
      },
    } });
    expect(reconciling.timeline.find((item) => item.type === "tool")).toMatchObject({ cancelled: true, call: { status: "in_progress" } });
  });
  it("keeps identified messages in their first position around compaction and preserves annotations", () => {
    let state: AppState = { ...initialState, session: { sessionId: "s1" } };
    for (const update of [
      { sessionUpdate: "agent_message_chunk", messageId: "m", content: { type: "text", text: "A", annotations: { audience: ["user"] } } },
      { sessionUpdate: "compaction_update", compactionId: "c", status: "in_progress" },
      { sessionUpdate: "agent_message_chunk", messageId: "m", content: { type: "text", text: "B", annotations: { audience: ["assistant"] } } },
      { sessionUpdate: "agent_message_chunk", messageId: "m", content: { type: "text", text: "C", annotations: { audience: ["assistant"] } } },
    ] satisfies SessionUpdate[]) {
      state = appReducer(state, ({ type: "session/turn_update", notification: { sessionId: "s1", update } }));
    }
    expect(state.timeline).toHaveLength(2);
    expect(state.timeline[0]).toMatchObject({ type: "assistant", chunks: [{ messageId: "m", blocks: [
      { type: "text", text: "A", annotations: { audience: ["user"] } },
      { type: "text", text: "BC", annotations: { audience: ["assistant"] } },
    ] }] });
    expect(state.timeline[1]).toMatchObject({ type: "compaction", compactionId: "c" });
  });

  it("matches every block of one optimistic prompt echo without duplicating attachments", () => {
    const blocks = [{ type: "text" as const, text: "Inspect" }, { type: "resource_link" as const, uri: "file:///file", name: "file" }];
    let state: AppState = { ...initialState, session: { sessionId: "s1" }, timeline: [{ id: "local", type: "message", role: "user", blocks, raw: [] }] };
    for (const content of blocks) {
      state = appReducer(state, ({ type: "session/turn_update", notification: { sessionId: "s1", update: { sessionUpdate: "user_message_chunk", messageId: "echo", content } } }));
    }
    expect(state.timeline).toHaveLength(1);
    expect(state.timeline[0]).toMatchObject({ role: "user", messageId: "echo", blocks });
    const message = state.timeline[0];
    if (message.type === "message") {
      expect(message.raw.at(-1)).toMatchObject({ update: { messageId: "echo", content: blocks[1] } });
    }
  });

  it("derives cancellation for unfinished tools and accepts later session-scoped completion", () => {
    let state: AppState = { ...initialState, session: { sessionId: "s1" }, pendingPrompt: { requestId: "p", sessionId: "s1", blocks: [] }, running: true };
    state = appReducer(state, ({ type: "session/turn_update", notification: { sessionId: "s1", update: { sessionUpdate: "tool_call", toolCallId: "tool", title: "Run", status: "in_progress" } } }));
    state = appReducer(state, turnComplete("s1", "p", { stopReason: "cancelled" }));
    expect(state.timeline[0]).toMatchObject({ type: "tool", cancelled: true, call: { status: "in_progress" } });
    state = appReducer(state, ({ type: "session/turn_update", notification: { sessionId: "s1", update: { sessionUpdate: "user_message_chunk", content: { type: "text", text: "next" } } } }));
    state = appReducer(state, ({ type: "session/turn_update", notification: { sessionId: "s1", update: { sessionUpdate: "tool_call_update", toolCallId: "tool", status: "completed" } } }));
    expect(state.timeline.filter((item) => item.type === "tool")).toHaveLength(1);
    expect(state.timeline[0]).toMatchObject({ type: "tool", call: { title: "Run", status: "completed" } });
    expect(state.timeline[0]).not.toHaveProperty("cancelled", true);
  });
  it("keeps the active project's cwd when connection metadata refreshes", () => {
    const state = appReducer({
      ...initialState, session: { sessionId: "other" }, cwd: "/projects/other",
    }, runtime({
      type: "bridge/hello", transport: "stdio", command: ["goose", "acp"],
      cwd: "/projects/default", readOnly: false, additionalDirectories: [], mcpServers: [],
    }));
    expect(state.defaultCwd).toBe("/projects/default");
    expect(state.cwd).toBe("/projects/other");
  });

  it("keeps an unlisted running session available when navigating to projects", () => {
    const active: AppState = {
      ...initialState,
      session: { sessionId: "new-session" },
      cwd: "/workspace/project",
      title: "Work in progress",
      running: true,
      pendingPrompt: { requestId: "in-flight", blocks: [{ type: "text", text: "Continue" }] },
    };
    const projects = appReducer(active, { type: "session/deselect" });
    expect(projects.session).toBeUndefined();
    expect(projects.running).toBe(false);
    expect(projects.sessions).toEqual([{
      sessionId: "new-session", cwd: "/workspace/project", title: "Work in progress",
    }]);
    expect(projects.cachedSessions.get("new-session")).toMatchObject({
      running: true, cwd: "/workspace/project", pendingPrompt: active.pendingPrompt,
    });
  });

  it("rebuilds bridge-owned turn outcomes at their persisted turn boundaries", () => {
    const base: AppState = {
      ...initialState,
      phase: "ready",
      session: { sessionId: "session" },
      running: true,
      pendingPrompt: {
        requestId: "browser-intent-id",
        blocks: [{ type: "text", text: "Retry me" }],
      },
    };
    const failure = {
      type: "bridge/turn_failed" as const,
      event: {
        type: "bridge/session_turn_failed" as const,
        bridgeEpoch: "epoch",
        sessionId: "session",
        sessionIncarnation: 1,
        viewRevision: 3,
        historyRevision: "epoch:1:3",
        phase: "ready" as const,
        operationId: "turn-1",
        clientIntentId: "browser-intent-id",
        prompt: [{ type: "text" as const, text: "Retry me" }],
        error: {
          code: -32603,
          message: "Synthetic failure",
          data: { retry: true },
        },
      },
    };
    const failed = appReducer(base, failure);
    expect(failed.timeline).toEqual([expect.objectContaining({
      id: "bridge-turn-outcome:turn-1",
      type: "error",
      code: -32603,
      retryBlocks: [{ type: "text", text: "Retry me" }],
    })]);
    expect(failed.running).toBe(false);
    expect(failed.pendingPrompt).toBeUndefined();
    expect(appReducer(failed, failure)).toBe(failed);

    const hydrated = appReducer(failed, {
      type: "bridge/session_hydrate",
      view: {
        bridgeEpoch: "epoch",
        sessionId: "session",
        sessionIncarnation: 1,
        viewRevision: 3,
        historyRevision: "epoch:1:3",
        phase: "ready",
        syncError: null,
        timeline: [],
        activeTurn: null,
        workspace: { cwd: "/workspace", session: {} },
        controls: {},
        interactions: { permissions: {}, elicitations: {}, urlFlows: {} },
        operation: null,
        terminals: {},
      },
    });
    expect(hydrated.timeline).toEqual([expect.objectContaining({
      id: "bridge-turn-outcome:turn-1",
      type: "error",
    })]);

    const completed = appReducer(hydrated, {
      type: "bridge/turn_complete",
      event: {
        type: "bridge/session_turn_complete",
        bridgeEpoch: "epoch",
        sessionId: "session",
        sessionIncarnation: 1,
        viewRevision: 4,
        historyRevision: "epoch:1:4",
        phase: "ready",
        operationId: "turn-2",
        clientIntentId: "browser-intent-id-2",
        response: { stopReason: "end_turn" },
      },
    });
    expect(completed.timeline.at(-1)).toEqual({
      id: "bridge-turn-outcome:turn-2",
      type: "stop",
      response: { stopReason: "end_turn" },
    });

    const reopenedView = {
      bridgeEpoch: "epoch",
      sessionId: "session",
      sessionIncarnation: 1,
      viewRevision: 5,
      historyRevision: "epoch:1:5",
      phase: "ready" as const,
      syncError: null,
      timeline: [
        {
          sessionUpdate: "user_message_chunk" as const,
          content: { type: "text" as const, text: "First" },
          _meta: { attyd: { turnOperationId: "turn-1" } },
        },
        {
          sessionUpdate: "agent_message_chunk" as const,
          messageId: "answer-1",
          content: { type: "text" as const, text: "One" },
        },
        {
          sessionUpdate: "user_message_chunk" as const,
          content: { type: "text" as const, text: "Second" },
          _meta: { attyd: { turnOperationId: "turn-2" } },
        },
        {
          sessionUpdate: "agent_message_chunk" as const,
          messageId: "answer-2",
          content: { type: "text" as const, text: "Two" },
        },
      ],
      turnOutcomes: [
        {
          operationId: "turn-1",
          afterUpdate: 2,
          response: { stopReason: "end_turn" as const },
        },
        {
          operationId: "turn-2",
          afterUpdate: 4,
          response: {
            stopReason: "max_tokens" as const,
            usage: { totalTokens: 21, inputTokens: 13, outputTokens: 8 },
          },
        },
      ],
      activeTurn: null,
      workspace: { cwd: "/workspace", session: {} },
      controls: {},
      interactions: { permissions: {}, elicitations: {}, urlFlows: {} },
      operation: null,
      terminals: {},
    };
    const reopened = appReducer(initialState, {
      type: "bridge/session_hydrate",
      view: reopenedView,
    });
    expect(reopened.timeline.map((item) =>
      item.type === "stop" ? `stop:${item.response.stopReason}` : item.type
    )).toEqual([
      "message",
      "assistant",
      "stop:end_turn",
      "message",
      "assistant",
      "stop:max_tokens",
    ]);
    const rehydrated = appReducer(reopened, {
      type: "bridge/session_hydrate",
      view: reopenedView,
    });
    expect(rehydrated.timeline.map((item) =>
      item.type === "stop" ? `${item.id}:${item.response.stopReason}` : item.type
    )).toEqual([
      "message",
      "assistant",
      "bridge-turn-outcome:turn-1:end_turn",
      "message",
      "assistant",
      "bridge-turn-outcome:turn-2:max_tokens",
    ]);
  });

  it("does not let a late prior-turn outcome settle the active turn", () => {
    const active: AppState = {
      ...initialState,
      phase: "ready",
      session: { sessionId: "session" },
      sessionSyncPhase: "running",
      running: true,
      pendingPrompt: {
        requestId: "intent-2",
        sessionId: "session",
        blocks: [{ type: "text", text: "Second turn" }],
      },
    };
    const next = appReducer(active, {
      type: "bridge/turn_complete",
      event: {
        type: "bridge/session_turn_complete",
        bridgeEpoch: "epoch",
        sessionId: "session",
        sessionIncarnation: 1,
        viewRevision: 7,
        historyRevision: "epoch:1:3",
        phase: "ready",
        operationId: "turn-1",
        clientIntentId: "intent-1",
        response: { stopReason: "end_turn" },
      },
    });

    expect(next.running).toBe(true);
    expect(next.sessionSyncPhase).toBe("running");
    expect(next.pendingPrompt?.requestId).toBe("intent-2");
    expect(next.timeline.at(-1)).toMatchObject({
      id: "bridge-turn-outcome:turn-1",
      type: "stop",
    });
  });

  it("turns a bridge-owned ACP auth failure into a sign-in state", () => {
    const state = appReducer({
      ...initialState,
      phase: "ready",
      initialized: {
        protocolVersion: 1,
        authMethods: [{ id: "agent-login", name: "Agent login" }],
      },
      session: { sessionId: "session" },
      sessionSyncPhase: "running",
      running: true,
      pendingPrompt: {
        requestId: "intent",
        sessionId: "session",
        blocks: [{ type: "text", text: "Continue" }],
      },
    }, {
      type: "bridge/turn_failed",
      event: {
        type: "bridge/session_turn_failed",
        bridgeEpoch: "epoch",
        sessionId: "session",
        sessionIncarnation: 1,
        viewRevision: 7,
        historyRevision: "epoch:1:3",
        phase: "blocked",
        operationId: "turn-1",
        clientIntentId: "intent",
        prompt: [{ type: "text", text: "Continue" }],
        error: { code: -32_000, message: "Authentication required" },
      },
    });

    expect(state.authStatus).toBe("required");
    expect(state.running).toBe(false);
    expect(state.timeline.at(-1)).toMatchObject({
      type: "error",
      code: -32_000,
      message: "Authentication required",
    });
  });

  it("tracks a remote session workspace and clears it when the thread closes", () => {
    let state = appReducer(initialState, runtime({
      type: "bridge/hello",
      transport: "ws",
      command: ["ws://agent.example/acp"],
      cwd: "",
      readOnly: false,
      additionalDirectories: [],
      mcpServers: [],
    }));
    state = appReducer(state, {
      type: "session/transition_start",
      kind: "new",
      requestId: "new",
      cwd: "/home/agent/project",
    });
    expect(state.cwd).toBe("/home/agent/project");

    state = appReducer(state, {
      type: "bridge/session_hydrate",
      view: sessionView("session", { workspace: { cwd: "/home/agent/project", session: {} } }),
    });
    expect(state.cwd).toBe("/home/agent/project");

    state = appReducer(state, {
      type: "session/transition_start",
      kind: "close",
      requestId: "close",
      sessionId: "session",
    });
    state = appReducer(state, {
      type: "session/management_complete",
      requestId: "close",
      sessionId: "session",
      owner: owner("session"),
      deleted: false,
    });
    expect(state.cwd).toBe("");
  });

  it("groups shared thought/answer IDs into one ordered assistant entry", () => {
    let state: AppState = { ...initialState, session: { sessionId: "session" } };
    const updates = [
      { sessionUpdate: "agent_thought_chunk" as const, content: { type: "text" as const, text: "Thought" } },
      { sessionUpdate: "agent_message_chunk" as const, content: { type: "text" as const, text: "Answer" } },
      { sessionUpdate: "user_message_chunk" as const, content: { type: "text" as const, text: "Echo" } },
    ];
    for (const update of updates) {
      state = appReducer(state, ({ type: "session/turn_update", notification: {
          sessionId: "session",
          update: { ...update, messageId: "server-owned-id" },
        },
      }));
    }

    expect(state.timeline).toHaveLength(2);
    const assistant = state.timeline[0];
    expect(assistant.type).toBe("assistant");
    if (assistant.type === "assistant") {
      expect(assistant.chunks.map(({ role }) => role)).toEqual(["thought", "agent"]);
      expect(assistant.chunks.every(({ messageId }) => messageId === "server-owned-id")).toBe(true);
    }
    expect(state.timeline[1]).toMatchObject({
      type: "message",
      role: "protocol-user",
      messageId: "server-owned-id",
    });
  });

  it("keeps identical bridge-memory prompts in separate turn entries", () => {
    let state: AppState = { ...initialState, session: { sessionId: "session" } };
    for (const turnOperationId of ["turn-1", "turn-2"]) {
      state = appReducer(state, ({ type: "session/turn_update", notification: {
          sessionId: "session",
          update: {
            sessionUpdate: "user_message_chunk",
            content: { type: "text", text: "same prompt" },
            _meta: { attyd: { turnOperationId } },
          },
        },
      }));
    }

    expect(state.timeline).toHaveLength(2);
    expect(state.timeline).toEqual([
      expect.objectContaining({
        type: "message",
        role: "protocol-user",
        blocks: [{ type: "text", text: "same prompt" }],
        turnOperationId: "turn-1",
      }),
      expect.objectContaining({
        type: "message",
        role: "protocol-user",
        blocks: [{ type: "text", text: "same prompt" }],
        turnOperationId: "turn-2",
      }),
    ]);
  });

  it("replaces a failed session page instead of restoring its previous error", () => {
    let state: AppState = {
      ...initialState,
      sessions: [{ sessionId: "saved", cwd: "/workspace" }],
    };
    state = appReducer(state, {
      type: "session/transition_start",
      kind: "attach",
      requestId: "first-load",
      sessionId: "saved",
      title: "Saved",
    });
    state = appReducer(state, requestFailed({
      requestId: "first-load",
      operation: "session/load",
      message: "First load failure",
    }));
    expect(state.timeline).toHaveLength(1);

    state = appReducer(state, {
      type: "session/transition_start",
      kind: "attach",
      requestId: "second-load",
      sessionId: "saved",
      title: "Saved",
    });
    state = appReducer(state, requestFailed({
      requestId: "second-load",
      operation: "session/load",
      message: "Second load failure",
    }));
    expect(state.timeline).toHaveLength(1);
    expect(state.timeline[0]).toMatchObject({
      type: "error",
      message: "Second load failure",
    });
  });

  it("tracks the live ACP activity that drives automatic thinking disclosure", () => {
    let state = appReducer({
      ...initialState,
      phase: "ready",
      session: { sessionId: "session" },
    }, {
      type: "user/prompt",
      requestId: "prompt",
      sessionId: "session",
      blocks: [{ type: "text", text: "Inspect" }],
    });
    expect(state.agentActivity).toEqual({ kind: "waiting" });

    state = appReducer(state, ({ type: "session/turn_update", notification: {
        sessionId: "session",
        update: {
          sessionUpdate: "agent_thought_chunk",
          messageId: "thought",
          content: { type: "text", text: "Reasoning" },
        },
      },
    }));
    const assistant = state.timeline.find((item) => item.type === "assistant");
    const thought = assistant?.type === "assistant"
      ? assistant.chunks.find((chunk) => chunk.messageId === "thought")
      : undefined;
    expect(state.agentActivity).toEqual({ kind: "thinking", timelineId: thought?.id });

    state = appReducer(state, ({ type: "session/turn_update", notification: {
        sessionId: "session",
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "inspect",
          title: "Inspect workspace",
          status: "in_progress",
        },
      },
    }));
    expect(state.agentActivity).toEqual({
      kind: "tool",
      toolCallId: "inspect",
      title: "Inspect workspace",
    });

    state = appReducer(state, ({ type: "session/turn_update", notification: {
        sessionId: "session",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "answer",
          content: { type: "text", text: "Done" },
        },
      },
    }));
    expect(state.agentActivity).toEqual({ kind: "responding" });

    state = appReducer(state, turnComplete("session", "prompt", { stopReason: "end_turn" }));
    expect(state.agentActivity).toBeUndefined();
    expect(state.running).toBe(false);
  });

  it("renders browser transport failures as timeline errors", () => {
    const state = appReducer(initialState, {
      type: "client/error",
      message: "Cannot send session/new: ACP WebSocket is not open",
    });
    expect(state.timeline).toEqual([
      expect.objectContaining({
        type: "error",
        message: "Cannot send session/new: ACP WebSocket is not open",
      }),
    ]);
  });

  it("preserves structured ACP prompt errors and the exact retry ContentBlocks", () => {
    const blocks = [
      { type: "text" as const, text: "Retry this turn" },
      {
        type: "resource_link" as const,
        name: "Context",
        uri: "https://example.com/context",
      },
    ];
    let state = appReducer({
      ...initialState,
      session: { sessionId: "session" },
    }, {
      type: "user/prompt",
      requestId: "prompt-error",
      sessionId: "session",
      blocks,
    });
    state = appReducer(state, requestFailed({
      requestId: "prompt-error",
      operation: "session/prompt",
      code: -32_603,
      message: "ACP error -32603: Provider unavailable",
      data: { retryAfterMs: 250, provider: "Agent-owned" },
      dataBytes: 48,
    }));

    expect(state.running).toBe(false);
    expect(state.pendingPrompt).toBeUndefined();
    expect(state.timeline.at(-1)).toMatchObject({
      type: "error",
      requestId: "prompt-error",
      operation: "session/prompt",
      code: -32_603,
      data: { retryAfterMs: 250, provider: "Agent-owned" },
      retryBlocks: blocks,
    });
  });

  it("tracks negotiated Agent authentication without turning it into chat state", () => {
    let state = appReducer(initialState, runtime({
      type: "acp/initialized",
      response: {
        protocolVersion: 1,
        authMethods: [{ id: "agent-login", name: "Agent login" }],
        agentCapabilities: { auth: { logout: {} } },
      },
    }));
    expect(state.authStatus).toBe("available");

    state = appReducer(state, {
      type: "auth/start",
      kind: "authenticate",
      requestId: "auth-request",
      methodId: "agent-login",
    });
    state = appReducer(state, globalEvent({
      type: "acp/authenticated",
      requestId: "auth-request",
      methodId: "agent-login",
      response: { _meta: { account: "Agent owned" } },
    }));
    expect(state.authStatus).toBe("authenticated");
    expect(state.pendingAuth).toBeUndefined();
    expect(state.lastAuthResponse).toEqual({
      kind: "authenticate",
      response: { _meta: { account: "Agent owned" } },
    });
    expect(state.timeline).toEqual([]);

    state = appReducer(state, {
      type: "auth/start",
      kind: "logout",
      requestId: "logout-request",
    });
    state = appReducer(state, globalEvent({
      type: "acp/logged_out",
      requestId: "logout-request",
      response: {},
    }));
    expect(state.authStatus).toBe("logged_out");
    expect(state.timeline).toEqual([]);
  });

  it("exposes auth controls for logout without assuming external sign-in state", () => {
    const state = appReducer(initialState, runtime({
      type: "acp/initialized",
      response: { protocolVersion: 1, authMethods: [], agentCapabilities: { auth: { logout: {} } } },
    }));
    expect(state.authStatus).toBe("available");
    expect(state.initialized?.authMethods).toEqual([]);
    expect(state.lastAuthResponse).toBeUndefined();
  });

  it("turns auth_required into a recoverable Agent sign-in state", () => {
    let state = appReducer({
      ...initialState,
      phase: "ready",
      initialized: {
        protocolVersion: 1,
        authMethods: [{ id: "agent-login", name: "Agent login" }],
      },
    }, {
      type: "session/transition_start",
      kind: "new",
      requestId: "new-session",
    });
    state = appReducer(state, requestFailed({
      requestId: "new-session",
      operation: "session/new",
      code: -32_000,
      message: "ACP error -32000: Authentication required",
    }));

    expect(state.authStatus).toBe("required");
    expect(state.sessionTransition).toBeUndefined();
    expect(state.timeline).toEqual([]);

    state = appReducer(state, {
      type: "auth/start",
      kind: "authenticate",
      requestId: "failed-auth",
      methodId: "agent-login",
    });
    state = appReducer(state, requestFailed({
      requestId: "failed-auth",
      operation: "auth/authenticate",
      code: -32_000,
      message: "Agent login was cancelled",
    }));
    expect(state.authStatus).toBe("required");
    expect(state.pendingAuth).toBeUndefined();
    expect(state.authError).toBe("Agent login was cancelled");
  });

  it("tracks request-scoped terminal authentication outside the conversation", () => {
    let state = appReducer({
      ...initialState,
      authStatus: "required",
    }, {
      type: "auth/start",
      kind: "terminal",
      requestId: "terminal-auth",
      methodId: "terminal-login",
    });
    expect(state.authTerminal).toMatchObject({
      status: "starting",
      output: "",
      methodId: "terminal-login",
    });
    state = appReducer(state, globalEvent({
      type: "bridge/auth_terminal_started",
      requestId: "terminal-auth",
      methodId: "terminal-login",
    }));
    state = appReducer(state, globalEvent({
      type: "bridge/auth_terminal_output",
      requestId: "terminal-auth",
      data: "Enter code: ",
    }));
    expect(state.authTerminal).toMatchObject({
      status: "running",
      output: "Enter code: ",
    });
    state = appReducer(state, requestFailed({
      requestId: "terminal-auth",
      operation: "auth/terminal_resize",
      message: "Resize was rejected",
    }));
    expect(state.pendingAuth).toMatchObject({ kind: "terminal", requestId: "terminal-auth" });
    expect(state.authTerminal).toMatchObject({
      status: "running",
      message: "Resize was rejected",
    });
    state = appReducer(state, globalEvent({
      type: "bridge/auth_terminal_exited",
      requestId: "terminal-auth",
      methodId: "terminal-login",
      status: "succeeded",
      exitCode: 0,
    }));
    expect(state.authStatus).toBe("authenticated");
    expect(state.pendingAuth).toBeUndefined();
    expect(state.authTerminal).toMatchObject({ status: "succeeded", exitCode: 0 });
    expect(state.timeline).toEqual([]);
  });

  it("reconstructs an in-flight terminal authentication from Bridge runtime events", () => {
    let state = appReducer(initialState, globalEvent({
      type: "bridge/auth_terminal_started",
      requestId: "terminal-auth",
      methodId: "terminal-login",
    }));
    state = appReducer(state, globalEvent({
      type: "bridge/auth_terminal_output",
      requestId: "terminal-auth",
      data: "Enter code: ",
    }));
    state = appReducer(state, runtime({
      type: "acp/initialized",
      response: {
        protocolVersion: 1,
        authMethods: [{ id: "terminal-login", name: "Terminal login" }],
      },
    }));

    expect(state.pendingAuth).toEqual({
      kind: "terminal",
      requestId: "terminal-auth",
      methodId: "terminal-login",
    });
    expect(state.authTerminal).toMatchObject({
      status: "running",
      output: "Enter code: ",
    });
  });

  it("isolates, replaces, preserves, and releases terminal snapshots", () => {
    let state = {
      ...initialState,
      session: { sessionId: "current" },
    };
    state = appReducer(state, ({ type: "session/terminal_update", terminal: {
        sessionId: "current",
        terminalId: "terminal-0",
        output: "first",
        truncated: false,
        released: false,
      },
    }));
    state = appReducer(state, ({ type: "session/terminal_update", terminal: {
        sessionId: "current",
        terminalId: "terminal-0",
        output: "-append",
        outputAppend: true,
        retainedBytes: 12,
        truncated: false,
        released: false,
      },
    }));
    expect(state.terminalSnapshots[0].output).toBe("first-append");
    state = appReducer(state, ({ type: "session/terminal_update", terminal: {
        sessionId: "current",
        terminalId: "terminal-0",
        output: "second",
        truncated: true,
        exitStatus: { exitCode: 0 },
        released: true,
      },
    }));
    expect(state.terminalSnapshots).toEqual([expect.objectContaining({
      terminalId: "terminal-0",
      output: "second",
      truncated: true,
      released: true,
    })]);

    state = appReducer(state, ({ type: "session/terminal_update", terminal: {
        sessionId: "other",
        terminalId: "cross-session",
        output: "CROSS_SESSION_TERMINAL",
        truncated: false,
        released: false,
      },
    }));
    expect(JSON.stringify(state.terminalSnapshots)).not.toContain("CROSS_SESSION_TERMINAL");
    expect(JSON.stringify(state.backgroundEvents)).toContain("CROSS_SESSION_TERMINAL");

    for (let index = 1; index <= 70; index += 1) {
      state = appReducer(state, ({ type: "session/terminal_update", terminal: {
          sessionId: "current",
          terminalId: `terminal-${index}`,
          output: String(index),
          truncated: false,
          released: false,
        },
      }));
    }
    expect(state.terminalSnapshots).toHaveLength(71);
    expect(state.terminalSnapshots[0].terminalId).toBe("terminal-0");

    state = appReducer(state, { type: "socket/closed" });
    expect(state.terminalSnapshots.every(({ released }) => released)).toBe(true);
  });

  it("preserves terminal results from authoritative history during live updates", () => {
    const retained = Array.from({ length: 70 }, (_, index) => ({
      sessionId: "current", terminalId: `old-${index}`, output: `Result ${index}`,
      truncated: false, released: true,
    }));
    const state = appReducer({
      ...initialState,
      session: { sessionId: "current" },
      sessionSyncPhase: "running",
      terminalSnapshots: retained,
    }, ({ type: "session/terminal_update", terminal: {
        sessionId: "current", terminalId: "live", output: "Current output",
        truncated: false, released: false, outputAppend: false,
      },
    }));
    expect(state.terminalSnapshots).toHaveLength(71);
    expect(state.terminalSnapshots.slice(0, 70)).toEqual(retained);
  });

  it("uses the Agent's authoritative config response instead of the requested value", () => {
    const pending = appReducer({
      ...initialState,
      session: { sessionId: "session" },
      sessionOwner: owner("session"),
      configOptions: [
        { type: "boolean", id: "verbose", name: "Verbose", currentValue: false },
      ],
    }, {
      type: "session/control_start",
      kind: "config",
      requestId: "config",
      sessionId: "session",
    });
    const state = appReducer(pending, {
      type: "bridge/session_live_update",
      settledControl: "config",
      view: sessionView("session", { workspace: { cwd: "/workspace", session: {
        configOptions: [
          { type: "boolean", id: "verbose", name: "Verbose", currentValue: false },
          {
            type: "select",
            id: "model",
            name: "Model",
            currentValue: "agent-default",
            options: [{ value: "agent-default", name: "Agent default" }],
          },
        ],
      } } }),
    });
    expect(state.pendingSessionControl).toBeUndefined();
    expect(state.configOptions).toHaveLength(2);
    expect(state.configOptions![0]).toMatchObject({ id: "verbose", currentValue: false });
    expect(state.configOptions![1]).toMatchObject({ id: "model", currentValue: "agent-default" });
  });

  it("commits a new session only through its matching transition", () => {
    const pending = appReducer(initialState, {
      type: "session/transition_start",
      kind: "new",
      requestId: "new-current",
    });
    const committed = appReducer(pending, { type: "bridge/session_hydrate", view: sessionView("created") });
    expect(committed.session?.sessionId).toBe("created");
    expect(committed.sessionTransition).toBeUndefined();
  });

  it("commits only the matching session control response and unlocks failures", () => {
    const active = {
      ...initialState,
      session: { sessionId: "controls" },
      sessionOwner: owner("controls"),
      modeId: "build",
      configOptions: [
        { type: "boolean" as const, id: "verbose", name: "Verbose", currentValue: false },
      ],
    };
    const pendingMode = appReducer(active, {
      type: "session/control_start",
      kind: "mode",
      requestId: "mode-current",
      sessionId: "controls",
    });
    expect(pendingMode.pendingSessionControl).toEqual({
      kind: "mode",
      requestId: "mode-current",
      sessionId: "controls",
    });

    const failed = appReducer(pendingMode, requestFailed({
      requestId: "mode-current",
      operation: "session/set_mode",
      message: "Synthetic control failure",
    }));
    expect(failed.modeId).toBe("build");
    expect(failed.pendingSessionControl).toBeUndefined();
    expect(failed.timeline.at(-1)).toMatchObject({
      type: "error",
      message: "Synthetic control failure",
    });

    const retrying = appReducer(failed, {
      type: "session/control_start",
      kind: "config",
      requestId: "config-retry",
      sessionId: "controls",
    });
    const blockedTransition = appReducer(retrying, {
      type: "session/transition_start",
      kind: "close",
      requestId: "close-during-control",
      sessionId: "controls",
    });
    expect(blockedTransition).toBe(retrying);

    const committed = appReducer(retrying, {
      type: "bridge/session_live_update",
      settledControl: "config",
      view: sessionView("controls", { workspace: { cwd: "/workspace", session: {
        modes: { currentModeId: "build", availableModes: [] },
        configOptions: [
          { type: "boolean", id: "verbose", name: "Verbose", currentValue: true },
        ],
      } } }),
    });
    expect(committed.pendingSessionControl).toBeUndefined();
    expect(committed.configOptions).toEqual([
      expect.objectContaining({ id: "verbose", currentValue: true }),
    ]);
    expect(committed.modeId).toBe("build");

    const recovered = appReducer(active, {
      type: "bridge/session_live_update",
      settledControl: "mode",
      view: sessionView("controls", { workspace: { cwd: "/workspace", session: {
        modes: { currentModeId: "plan", availableModes: [] },
      } } }),
    });
    expect(recovered.modeId).toBe("plan");
  });

  it("settles authoritative interactions globally while correlating response errors exactly", () => {
    const active = {
      ...initialState,
      session: { sessionId: "session" },
      sessionOwner: owner("session"),
      permissions: [{
        permissionId: "permission",
        request: {
          sessionId: "session",
          toolCall: { toolCallId: "tool", title: "Run tool" },
          options: [{ optionId: "allow", name: "Allow once", kind: "allow_once" as const }],
        },
      }],
      elicitations: [{
        elicitationId: "form",
        request: {
          sessionId: "session",
          mode: "form" as const,
          message: "Configure",
          requestedSchema: { type: "object" as const, properties: {} },
        },
      }],
    };
    let state = appReducer(active, {
      type: "permission/respond_start",
      permissionId: "permission",
      requestId: "permission-response",
    });
    state = appReducer(state, {
      type: "elicitation/respond_start",
      elicitationId: "form",
      requestId: "form-response",
    });
    expect(state.permissions[0].responseRequestId).toBe("permission-response");
    expect(state.elicitations[0].responseRequestId).toBe("form-response");

    const responding = state;
    const pendingView = (permissions: boolean, elicitations: boolean) => ({
      type: "bridge/session_live_update" as const,
      view: sessionView("session", { interactions: {
        permissions: permissions
          ? { permission: { interactionId: "permission", request: active.permissions[0].request } }
          : {},
        elicitations: elicitations
          ? { form: { interactionId: "form", request: active.elicitations[0].request } }
          : {},
        urlFlows: {},
      } }),
    });
    const unrelated = appReducer(responding, pendingView(true, true));
    expect(unrelated.permissions).toEqual(responding.permissions);
    expect(unrelated.elicitations).toEqual(responding.elicitations);

    const resolvedElsewhere = appReducer(responding, pendingView(false, true));
    expect(resolvedElsewhere.permissions).toEqual([]);

    state = appReducer(responding, requestFailed({
      requestId: "permission-response",
      operation: "permission/respond",
      message: "Permission response failed",
    }));
    expect(state.permissions[0]).toMatchObject({
      permissionId: "permission",
      responseError: "Permission response failed",
    });
    expect(state.permissions[0].responseRequestId).toBeUndefined();
    expect(state.timeline).toEqual([]);

    state = appReducer(state, {
      type: "permission/respond_start",
      permissionId: "permission",
      requestId: "permission-retry",
    });
    state = appReducer(state, pendingView(false, true));
    expect(state.permissions).toEqual([]);

    state = appReducer(state, pendingView(false, false));
    expect(state.elicitations).toEqual([]);
  });

  it("applies partial session metadata updates and honors explicit null clears", () => {
    const active = {
      ...initialState,
      session: { sessionId: "session" },
      title: "Old title",
      sessions: [{
        sessionId: "session",
        cwd: "/workspace",
        title: "Old title",
        updatedAt: "2026-08-30T08:00:00.000Z",
      }],
    };
    const timestampOnly = appReducer(active, ({ type: "session/turn_update", notification: {
        sessionId: "session",
        update: {
          sessionUpdate: "session_info_update",
          updatedAt: "2026-08-30T09:00:00.000Z",
        },
      },
    }));
    expect(timestampOnly.title).toBe("Old title");
    expect(timestampOnly.sessions[0]).toMatchObject({
      title: "Old title",
      updatedAt: "2026-08-30T09:00:00.000Z",
    });

    const cleared = appReducer(timestampOnly, ({ type: "session/turn_update", notification: {
        sessionId: "session",
        update: {
          sessionUpdate: "session_info_update",
          title: null,
        },
      },
    }));
    expect(cleared.title).toBeUndefined();
    expect(cleared.sessions[0].title).toBeNull();
  });

  it("clears connection-scoped interactions when the socket or active session ends", () => {
    const active = {
      ...initialState,
      socketOpen: true,
      session: { sessionId: "session" },
      permissions: [{
        permissionId: "permission",
        request: {
          sessionId: "session",
          toolCall: { toolCallId: "tool", title: "Tool" },
          options: [{ optionId: "allow", name: "Allow", kind: "allow_once" as const }],
        },
      }],
      elicitations: [{
        elicitationId: "form",
        request: {
          sessionId: "session",
          mode: "form" as const,
          message: "Input",
          requestedSchema: { type: "object" as const, properties: {} },
        },
      }],
      externalFlows: [{
        elicitationId: "external",
        message: "Connect",
        status: "waiting" as const,
      }],
    };
    const disconnected = appReducer(active, { type: "socket/closed" });
    expect(disconnected.permissions).toEqual([]);
    expect(disconnected.elicitations).toEqual([]);
    expect(disconnected.externalFlows).toEqual([]);

    const reset = appReducer(active, { type: "session/reset" });
    expect(reset.externalFlows).toEqual(active.externalFlows);
  });

  it("turns bridge error phases into a complete UI terminal transaction", () => {
    const active = {
      ...initialState,
      phase: "ready" as const,
      socketOpen: true,
      session: { sessionId: "current" },
      timeline: [{ id: "kept", type: "error" as const, message: "visible history" }],
      terminalSnapshots: [{
        sessionId: "current",
        terminalId: "terminal",
        output: "output",
        truncated: false,
        released: false,
      }],
    };
    const transitioning = appReducer(active, {
      type: "session/transition_start",
      kind: "new",
      requestId: "pending-new",
    });
    const failed = appReducer({
      ...transitioning,
      pendingSessionDeletions: [{
        requestId: "delete",
        sessionId: "saved",
      }],
      pendingSessionControl: {
        requestId: "control",
        sessionId: "current",
        kind: "mode" as const,
      },
      permissions: [{
        permissionId: "permission",
        request: {
          sessionId: "current",
          toolCall: { toolCallId: "tool", title: "Tool" },
          options: [{ optionId: "allow", name: "Allow", kind: "allow_once" as const }],
        },
      }],
    }, globalEvent({ type: "bridge/connection", phase: "error" }));

    expect(failed.phase).toBe("error");
    expect(failed.socketOpen).toBe(true);
    expect(failed.session?.sessionId).toBe("current");
    expect(failed.timeline).toEqual(active.timeline);
    expect(failed.sessionTransition).toBeUndefined();
    expect(failed.pendingSessionDeletions).toEqual([]);
    expect(failed.pendingSessionControl).toBeUndefined();
    expect(failed.permissions).toEqual([]);
    expect(failed.terminalSnapshots[0]?.released).toBe(true);
  });

  it("isolates session-scoped elicitations while preserving request-scoped interactions", () => {
    const withRequestScope: AppState = {
      ...initialState,
      session: { sessionId: "current" },
      elicitations: [{
        elicitationId: "current-form",
        request: {
          sessionId: "current",
          mode: "form",
          message: "Current session",
          requestedSchema: { type: "object", properties: {} },
        },
      }, {
        elicitationId: "global-form",
        request: {
          requestId: "outer-request",
          mode: "form",
          message: "Connection-scoped request",
          requestedSchema: { type: "object", properties: {} },
        },
      }],
    };
    const reset = appReducer(withRequestScope, { type: "session/reset" });
    expect(reset.elicitations.map(({ elicitationId }) => elicitationId))
      .toEqual(["global-form"]);
  });

  it("rolls back a failed session attachment without mixing replayed history", () => {
    const previous = {
      ...initialState,
      phase: "ready" as const,
      socketOpen: true,
      session: { sessionId: "current" },
      title: "Current session",
      availableCommands: [{ name: "old", description: "Old command" }],
      timeline: [{
        id: "old-message",
        type: "assistant" as const,
        chunks: [{
          id: "old-message-chunk",
          role: "agent" as const,
          blocks: [{ type: "text" as const, text: "Keep me" }],
          raw: [],
        }],
      }],
    };
    let pending = appReducer(previous, {
      type: "session/transition_start",
      kind: "attach",
      requestId: "attach-request",
      sessionId: "saved",
      title: "Saved session",
    });
    expect(pending.session?.sessionId).toBe("current");
    expect(pending.pendingSessionId).toBe("saved");
    expect(pending.timeline).toEqual(previous.timeline);

    pending = appReducer(pending, ({ type: "session/turn_update", notification: {
        sessionId: "saved",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "replayed",
          content: { type: "text", text: "Loaded history" },
        },
      },
    }));
    expect(pending.timeline).toEqual(previous.timeline);
    expect(JSON.stringify(pending.timeline)).not.toContain("Loaded history");

    const restored = appReducer(pending, requestFailed({
      requestId: "attach-request",
      operation: "session/load",
      message: "Agent refused the load",
    }));
    expect(restored.session?.sessionId).toBe("current");
    expect(restored.title).toBe("Current session");
    expect(restored.availableCommands).toEqual(previous.availableCommands);
    expect(restored.timeline).toHaveLength(2);
    expect(restored.timeline[0]).toEqual(previous.timeline[0]);
    expect(restored.timeline[1]).toMatchObject({
      type: "error",
      message: "Agent refused the load",
    });
    expect(restored.sessionTransition).toBeUndefined();
  });

  it("commits a loaded session only from its authoritative view", () => {
    let state = appReducer({
      ...initialState,
      session: { sessionId: "current" },
      timeline: [{
        id: "current-message",
        type: "assistant" as const,
        chunks: [{
          id: "current-chunk",
          role: "agent" as const,
          blocks: [{ type: "text" as const, text: "Current" }],
          raw: [],
        }],
      }],
    }, {
      type: "session/transition_start",
      kind: "attach",
      requestId: "load-request",
      sessionId: "saved",
      title: "Saved",
    });
    state = appReducer(state, {
      type: "bridge/session_hydrate",
      view: sessionView("saved", { timeline: [{
        sessionUpdate: "agent_message_chunk",
        messageId: "loaded",
        content: { type: "text", text: "Loaded" },
      }] }),
    });
    expect(state.session?.sessionId).toBe("saved");
    expect(state.timeline).toHaveLength(1);
    expect(state.timeline[0]).toMatchObject({
      type: "assistant",
      chunks: [{ messageId: "loaded" }],
    });
    expect(state.sessionTransition).toBeUndefined();
  });

  it("keeps load updates hidden until the loaded view hydrates, then replaces atomically", () => {
    const original: AppState = {
      ...initialState,
      phase: "ready",
      socketOpen: true,
      session: { sessionId: "current" },
      timeline: [{
        id: "current-answer",
        type: "assistant",
        chunks: [{
          id: "current-answer-chunk",
          role: "agent",
          blocks: [{ type: "text", text: "Stable current projection" }],
          raw: [],
        }],
      }],
    };

    let loading = appReducer(original, {
      type: "session/transition_start",
      kind: "attach",
      requestId: "load-saved",
      sessionId: "saved",
      title: "Saved",
    });
    loading = appReducer(loading, ({ type: "session/turn_update", notification: {
        sessionId: "saved",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "loaded-answer",
          content: { type: "text", text: "Hidden loaded projection" },
        },
      },
    }));

    expect(loading.session?.sessionId).toBe("current");
    expect(JSON.stringify(loading.timeline)).toContain("Stable current projection");
    expect(JSON.stringify(loading.timeline)).not.toContain("Hidden loaded projection");

    const attached = appReducer(loading, {
      type: "bridge/session_hydrate",
      view: sessionView("saved", { timeline: [{
        sessionUpdate: "agent_message_chunk",
        messageId: "loaded-answer",
        content: { type: "text", text: "Hidden loaded projection" },
      }] }),
    });

    expect(attached.session?.sessionId).toBe("saved");
    expect(JSON.stringify(attached.timeline)).toContain("Hidden loaded projection");
    expect(JSON.stringify(attached.timeline)).not.toContain("Stable current projection");
    expect(attached.sessionTransition).toBeUndefined();
  });

  it("never exposes a failed session/load candidate and retains the prior projection", () => {
    const original: AppState = {
      ...initialState,
      phase: "ready",
      socketOpen: true,
      session: { sessionId: "current" },
      timeline: [{
        id: "current-answer",
        type: "assistant",
        chunks: [{
          id: "current-answer-chunk",
          role: "agent",
          blocks: [{ type: "text", text: "Stable current projection" }],
          raw: [],
        }],
      }],
    };
    let loading = appReducer(original, {
      type: "session/transition_start",
      kind: "attach",
      requestId: "load-saved",
      sessionId: "saved",
    });
    loading = appReducer(loading, ({ type: "session/turn_update", notification: {
        sessionId: "saved",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "partial-answer",
          content: { type: "text", text: "Must remain hidden" },
        },
      },
    }));
    const failed = appReducer(loading, requestFailed({
      requestId: "load-saved",
      operation: "session/load",
      message: "Agent rejected load",
    }));

    expect(failed.session?.sessionId).toBe("current");
    expect(JSON.stringify(failed.timeline)).toContain("Stable current projection");
    expect(JSON.stringify(failed.timeline)).not.toContain("Must remain hidden");
    expect(failed.timeline.at(-1)).toMatchObject({
      type: "error",
      message: "Agent rejected load",
    });
    expect(failed.sessionTransition).toBeUndefined();
  });

  it("drops a session/load candidate on disconnect without exposing it", () => {
    const original: AppState = {
      ...initialState,
      phase: "ready",
      socketOpen: true,
      session: { sessionId: "current" },
      timeline: [{
        id: "current-answer",
        type: "assistant",
        chunks: [{
          id: "current-answer-chunk",
          role: "agent",
          blocks: [{ type: "text", text: "Stable current projection" }],
          raw: [],
        }],
      }],
    };
    let loading = appReducer(original, {
      type: "session/transition_start",
      kind: "attach",
      requestId: "load-saved",
      sessionId: "saved",
    });
    loading = appReducer(loading, ({ type: "session/turn_update", notification: {
        sessionId: "saved",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "partial-answer",
          content: { type: "text", text: "Must remain hidden" },
        },
      },
    }));
    const disconnected = appReducer(loading, { type: "socket/closed" });

    expect(disconnected.phase).toBe("stopped");
    expect(disconnected.session?.sessionId).toBe("current");
    expect(JSON.stringify(disconnected.timeline)).toContain("Stable current projection");
    expect(JSON.stringify(disconnected.timeline)).not.toContain("Must remain hidden");
    expect(disconnected.sessionTransition).toBeUndefined();
  });

  it("coalesces text chunks only when they belong to the same message", () => {
    const first = appReducer(
      { ...initialState, session: { sessionId: "s1" } },
      ({ type: "session/turn_update", notification: {
          sessionId: "s1",
          update: {
            sessionUpdate: "agent_message_chunk",
            messageId: "m1",
            content: { type: "text", text: "hello " },
          },
        },
      }),
    );
    const second = appReducer(
      first,
      ({ type: "session/turn_update", notification: {
          sessionId: "s1",
          update: {
            sessionUpdate: "agent_message_chunk",
            messageId: "m1",
            content: { type: "text", text: "world" },
          },
        },
      }),
    );

    expect(second.timeline).toHaveLength(1);
    const item = second.timeline[0];
    expect(item.type).toBe("assistant");
    if (item.type === "assistant") {
      expect(item.chunks).toHaveLength(1);
      expect(item.chunks[0]?.blocks).toEqual([{ type: "text", text: "hello world" }]);
    }
  });

  it("keeps a message ID at its first position across an interleaved tool event", () => {
    let state = appReducer(
      { ...initialState, session: { sessionId: "s1" } },
      ({ type: "session/turn_update", notification: {
          sessionId: "s1",
          update: {
            sessionUpdate: "agent_message_chunk",
            messageId: "m1",
            content: { type: "text", text: "before " },
          },
        },
      }),
    );
    state = appReducer(state, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "tool-1",
          title: "Interleaved",
        },
      },
    }));
    state = appReducer(state, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "m1",
          content: { type: "text", text: "after" },
        },
      },
    }));

    expect(state.timeline).toHaveLength(2);
    expect(state.timeline[0]).toMatchObject({
      type: "assistant",
      chunks: [{ messageId: "m1", blocks: [{ type: "text", text: "before after" }] }],
    });
    expect(state.timeline[1]).toMatchObject({ type: "tool" });
  });

  it("merges anonymous adjacent chunks with an identified message like Zed", () => {
    let state = appReducer(
      { ...initialState, session: { sessionId: "s1" } },
      ({ type: "session/turn_update", notification: {
          sessionId: "s1",
          update: {
            sessionUpdate: "agent_message_chunk",
            messageId: "identified",
            content: { type: "text", text: "identified" },
          },
        },
      }),
    );
    state = appReducer(state, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "anonymous" },
        },
      },
    }));
    expect(state.timeline).toHaveLength(1);
    expect(state.timeline[0]).toMatchObject({
      type: "assistant",
      chunks: [{
        messageId: "identified",
        blocks: [{ type: "text", text: "identifiedanonymous" }],
      }],
    });
  });

  it("applies sparse tool updates without losing prior fields", () => {
    const created = appReducer(
      { ...initialState, session: { sessionId: "s1" } },
      ({ type: "session/turn_update", notification: {
          sessionId: "s1",
          update: {
            sessionUpdate: "tool_call",
            toolCallId: "t1",
            title: "Read file",
            kind: "read",
            status: "pending",
          },
        },
      }),
    );
    const updated = appReducer(
      created,
      ({ type: "session/turn_update", notification: {
          sessionId: "s1",
          update: { sessionUpdate: "tool_call_update", toolCallId: "t1", status: "completed" },
        },
      }),
    );

    const item = updated.timeline[0];
    expect(item.type).toBe("tool");
    if (item.type === "tool") {
      expect(item.call).toMatchObject({ title: "Read file", kind: "read", status: "completed" });
      expect(item.raw.at(-1)).toMatchObject({ update: { toolCallId: "t1", status: "completed" } });
    }
  });

  it("preserves earlier diffs when a later turn reuses a tool call ID", () => {
    let state: AppState = { ...initialState, session: { sessionId: "s1" } };
    const create = (newText: string) => ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "reused",
          title: "Edit file",
          status: "pending",
          content: [{ type: "diff", path: "app.ts", oldText: null, newText }],
        },
      },
    });
    state = appReducer(state, create("first turn"));
    const firstTool = state.timeline[0];
    state = {
      ...state,
      timeline: [
        ...state.timeline,
        { id: "stop", type: "stop", response: { stopReason: "end_turn" } },
        { id: "next", type: "message", role: "user", blocks: [{ type: "text", text: "Continue" }], raw: [] },
      ],
    };
    state = appReducer(state, create("second turn"));
    state = appReducer(state, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: { sessionUpdate: "tool_call_update", toolCallId: "reused", status: "completed" },
      },
    }));

    const tools = state.timeline.filter((item) => item.type === "tool");
    expect(tools).toHaveLength(2);
    expect(tools[0]).toBe(firstTool);
    expect(tools[0].id).not.toBe(tools[1].id);
    expect(tools[1].call).toMatchObject({
      status: "completed",
      content: [{ type: "diff", newText: "second turn" }],
    });
    expect(tools[1].raw.at(-1)).toMatchObject({ update: { toolCallId: "reused", status: "completed" } });
  });

  it("applies an update to the session's tool even after another prompt starts", () => {
    const state = appReducer({
      ...initialState,
      session: { sessionId: "s1" },
      timeline: [
        { id: "old-tool", type: "tool", call: { toolCallId: "reused", title: "Previous edit" }, raw: [] },
        { id: "new-prompt", type: "message", role: "protocol-user", blocks: [], raw: [] },
      ],
    }, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: { sessionUpdate: "tool_call_update", toolCallId: "reused", status: "completed" },
      },
    }));

    expect(state.timeline).toHaveLength(2);
    expect(state.timeline[0]).toMatchObject({ call: { title: "Previous edit", status: "completed" } });
  });

  it("renders a failed placeholder for a tool update without a creation", () => {
    const state = appReducer(
      { ...initialState, session: { sessionId: "s1" } },
      ({ type: "session/turn_update", notification: {
          sessionId: "s1",
          update: {
            sessionUpdate: "tool_call_update",
            toolCallId: "missing",
            status: "completed",
          },
        },
      }),
    );
    expect(state.timeline).toHaveLength(1);
    expect(state.timeline[0]).toMatchObject({
      type: "tool",
      call: {
        toolCallId: "missing",
        title: "Tool call not found",
        status: "failed",
      },
    });
  });

  it("keeps session history agent-owned and attaches loaded sessions", () => {
    const listed = appReducer(initialState, {
      type: "sessions/listed",
      response: {
        sessions: [{ sessionId: "saved", cwd: "/workspace", title: "Saved" }],
        nextCursor: "next",
      },
    });
    const pending = appReducer(listed, {
      type: "session/transition_start",
      kind: "attach",
      requestId: "load-1",
      sessionId: "saved",
      title: "Saved",
    });
    const attached = appReducer(pending, { type: "bridge/session_hydrate", view: sessionView("saved") });

    expect(attached.session?.sessionId).toBe("saved");
    expect(attached.title).toBe("Saved");
    expect(attached.nextSessionCursor).toBe("next");
  });

  it("keeps fork context visible and commits only the matching transaction", () => {
    const active = {
      ...initialState,
      session: { sessionId: "source" },
      timeline: [
        {
          id: "message:source",
          type: "assistant" as const,
          chunks: [{
            id: "message:source-chunk",
            role: "agent" as const,
            blocks: [{ type: "text" as const, text: "inherited context" }],
            raw: [],
          }],
        },
      ],
      running: false,
    };
    const pending = appReducer(active, {
      type: "session/transition_start",
      kind: "fork",
      requestId: "fork-current",
      sessionId: "source",
    });
    expect(pending.session?.sessionId).toBe("source");
    expect(pending.timeline).toEqual(active.timeline);
    expect(pending.sessionTransition).toMatchObject({
      kind: "fork",
      requestId: "fork-current",
    });

    const forked = appReducer(pending, {
      type: "bridge/session_hydrate",
      view: sessionView("forked", {
        workspace: { cwd: "/workspace", session: {
          modes: {
            currentModeId: "plan",
            availableModes: [{ id: "plan", name: "Plan" }],
          },
        } },
        timeline: [{
          sessionUpdate: "agent_message_chunk",
          messageId: "early-fork-message",
          content: { type: "text", text: "Fork ready" },
        }],
        controls: {
          available_commands_update: {
            sessionUpdate: "available_commands_update",
            availableCommands: [{ name: "fork-status", description: "Inspect fork" }],
          },
        },
      }),
    });
    expect(forked.session?.sessionId).toBe("forked");
    expect(forked.modeId).toBe("plan");
    expect(forked.availableCommands).toEqual([
      { name: "fork-status", description: "Inspect fork" },
    ]);
    expect(JSON.stringify(forked.timeline)).toContain("early-fork-message");
    expect(forked.running).toBe(false);
    expect(forked.sessionTransition).toBeUndefined();

    const failed = appReducer(pending, requestFailed({
      requestId: "fork-current",
      operation: "session/fork",
      message: "Agent refused to fork",
    }));
    expect(failed.session?.sessionId).toBe("source");
    expect(failed.timeline[0]).toEqual(active.timeline[0]);
    expect(failed.timeline.at(-1)).toMatchObject({
      type: "error",
      message: "Agent refused to fork",
    });
    expect(failed.sessionTransition).toBeUndefined();
  });

  it("keeps close context visible until commit and rolls back failures", () => {
    const active = {
      ...initialState,
      session: { sessionId: "closing" },
      sessionOwner: owner("closing"),
      title: "Keep until closed",
      timeline: [{
        id: "close-context",
        type: "assistant" as const,
        chunks: [{
          id: "close-context-chunk",
          role: "agent" as const,
          blocks: [{ type: "text" as const, text: "Still visible" }],
          raw: [],
        }],
      }],
    };
    const pending = appReducer(active, {
      type: "session/transition_start",
      kind: "close",
      requestId: "close-current",
      sessionId: "closing",
    });
    expect(pending.session).toEqual(active.session);
    expect(pending.timeline).toEqual(active.timeline);
    expect(pending.sessionTransition?.kind).toBe("close");

    const closedElsewhere = appReducer(pending, {
      type: "session/retired",
      owner: owner("closing"),
      deleted: false,
    });
    expect(closedElsewhere.session).toBeUndefined();
    expect(closedElsewhere.timeline).toEqual([]);
    expect(closedElsewhere.sessionTransition).toBeUndefined();

    const repeated = appReducer(closedElsewhere, {
      type: "session/management_complete",
      requestId: "close-current",
      sessionId: "closing",
      owner: owner("closing"),
      deleted: false,
    });
    expect(repeated.session).toBeUndefined();
    expect(repeated.timeline).toEqual([]);
    expect(repeated.sessionTransition).toBeUndefined();

    const failed = appReducer(pending, requestFailed({
      requestId: "close-current",
      operation: "session/close",
      message: "Synthetic close failure",
    }));
    expect(failed.session?.sessionId).toBe("closing");
    expect(failed.title).toBe(active.title);
    expect(failed.timeline[0]).toEqual(active.timeline[0]);
    expect(failed.timeline.at(-1)).toMatchObject({
      type: "error",
      message: "Synthetic close failure",
    });
    expect(failed.sessionTransition).toBeUndefined();
  });

  it("commits authoritative session deletions globally and unlocks failures", () => {
    const listed = {
      ...initialState,
      sessions: [{
        sessionId: "saved",
        cwd: "/workspace",
        title: "Saved",
      }],
    };
    const pending = appReducer(listed, {
      type: "session/delete_start",
      requestId: "delete-current",
      sessionId: "saved",
    });
    expect(pending.pendingSessionDeletions).toEqual([
      { requestId: "delete-current", sessionId: "saved" },
    ]);

    const deletedElsewhere = appReducer(pending, {
      type: "session/retired",
      owner: owner("saved"),
      deleted: true,
    });
    expect(deletedElsewhere.sessions).toEqual([]);
    expect(deletedElsewhere.pendingSessionDeletions).toEqual([]);

    const failed = appReducer(pending, requestFailed({
      requestId: "delete-current",
      operation: "session/delete",
      message: "Synthetic delete failure",
    }));
    expect(failed.sessions).toEqual(listed.sessions);
    expect(failed.pendingSessionDeletions).toEqual([]);
    expect(failed.timeline.at(-1)).toMatchObject({
      type: "error",
      message: "Synthetic delete failure",
    });

    const retrying = appReducer(failed, {
      type: "session/delete_start",
      requestId: "delete-retry",
      sessionId: "saved",
    });
    const deleted = appReducer(retrying, {
      type: "session/management_complete",
      requestId: "delete-retry",
      sessionId: "saved",
      deleted: true,
    });
    expect(deleted.sessions).toEqual([]);
    expect(deleted.pendingSessionDeletions).toEqual([]);
  });

  it("keeps the current session selected when its deletion fails", () => {
    const active = {
      ...initialState,
      session: { sessionId: "current" },
      title: "Current thread",
      cwd: "/workspace/current",
      timeline: [{ id: "answer", type: "agent_message" as const, text: "Recover me" }],
    };
    const closing = appReducer(active, {
      type: "session/delete_start",
      requestId: "close-current-for-delete",
      sessionId: "current",
    });
    const failed = appReducer(closing, requestFailed({
      requestId: "close-current-for-delete",
      operation: "session/close",
      message: "Synthetic close failure",
    }));
    expect(failed.session).toEqual(active.session);
    expect(failed.pendingSessionDeletions).toEqual([]);
    expect(failed.timeline.slice(0, -1)).toEqual(active.timeline);
    expect(failed.timeline.at(-1)).toMatchObject({
      type: "error",
      message: "Synthetic close failure",
    });
  });

  it("tracks URL consent and completion as separate states", () => {
    const request = {
      sessionId: "s1",
      mode: "url" as const,
      message: "Connect account",
      elicitationId: "agent-flow",
      url: "https://example.test/connect",
    };
    const flowView = (status: "waiting" | "completed") => sessionView("s1", { interactions: {
      permissions: {}, elicitations: {},
      urlFlows: { "agent-flow": { elicitationId: "agent-flow", request, status } },
    } });
    const accepted = appReducer(initialState, { type: "bridge/session_hydrate", view: flowView("waiting") });
    expect(accepted.externalFlows).toEqual([
      expect.objectContaining({ elicitationId: "agent-flow", sessionId: "s1", status: "waiting" }),
    ]);

    const switched = appReducer(accepted, { type: "session/reset" });
    expect(switched.externalFlows).toEqual([
      expect.objectContaining({ elicitationId: "agent-flow", status: "waiting" }),
    ]);

    const completed = appReducer(switched, { type: "bridge/session_hydrate", view: flowView("completed") });
    expect(completed.externalFlows[0]?.status).toBe("completed");
  });

  it("does not end a prompt when an unrelated bridge operation fails", () => {
    const running = appReducer({
      ...initialState,
      session: { sessionId: "current" },
    }, {
      type: "user/prompt",
      requestId: "prompt-current",
      sessionId: "current",
      blocks: [{ type: "text", text: "working" }],
    });
    expect(running.pendingPrompt).toEqual({
      requestId: "prompt-current",
      sessionId: "current",
      blocks: [{ type: "text", text: "working" }],
    });
    const listFailed = appReducer(
      running,
      requestFailed({
        requestId: "list",
        operation: "session/list",
        message: "list failed",
      }),
    );
    expect(listFailed.running).toBe(true);
    expect(listFailed.pendingPrompt).toEqual(running.pendingPrompt);

    const duplicatePromptFailed = appReducer(
      running,
      requestFailed({
        requestId: "prompt-duplicate",
        operation: "session/prompt",
        message: "A prompt is already running",
      }),
    );
    expect(duplicatePromptFailed.running).toBe(true);
    expect(duplicatePromptFailed.pendingPrompt).toEqual(running.pendingPrompt);

    const promptFailed = appReducer(
      running,
      requestFailed({
        requestId: "prompt-current",
        operation: "session/prompt",
        message: "prompt failed",
      }),
    );
    expect(promptFailed.running).toBe(false);
    expect(promptFailed.pendingPrompt).toBeUndefined();

    const recoverableProtocolError = appReducer(
      running,
      requestFailed({
        message: "Invalid Agent update was dropped",
      }),
    );
    expect(recoverableProtocolError.running).toBe(true);

    const stopped = appReducer(
      recoverableProtocolError,
      globalEvent({ type: "bridge/connection", phase: "error" }),
    );
    expect(stopped.running).toBe(false);
    expect(stopped.pendingPrompt).toBeUndefined();

    const concurrentTransition = appReducer(running, {
      type: "session/transition_start",
      kind: "new",
      requestId: "new-during-prompt",
    });
    expect(concurrentTransition.session).toBeUndefined();
    expect(concurrentTransition.sessionTransition).toMatchObject({
      kind: "new",
      requestId: "new-during-prompt",
    });
    expect(concurrentTransition.cachedSessions.get("current")?.running).toBe(true);
    expect(concurrentTransition.cachedSessions.get("current")?.pendingPrompt)
      .toEqual(running.pendingPrompt);
  });

  it("isolates late events from a non-current session", () => {
    const active = appReducer({
      ...initialState,
      session: { sessionId: "current" },
    }, {
      type: "user/prompt",
      requestId: "current-prompt",
      sessionId: "current",
      blocks: [{ type: "text", text: "working" }],
    });
    const lateUpdate = appReducer(
      active,
      ({ type: "session/turn_update", notification: {
          sessionId: "old",
          update: {
            sessionUpdate: "agent_message_chunk",
            content: { type: "text", text: "late" },
          },
        },
      }),
    );
    const lateStop = appReducer(lateUpdate, turnComplete("old", "old-prompt", { stopReason: "end_turn" }));
    expect(lateStop).toBe(lateUpdate);

    const staleCurrentStop = appReducer(
      lateStop,
      turnComplete("current", "stale-current-prompt", { stopReason: "end_turn" }),
    );
    expect(staleCurrentStop.timeline[0]).toMatchObject({ role: "user" });
    expect(staleCurrentStop.running).toBe(true);
    expect(staleCurrentStop.pendingPrompt?.requestId).toBe("current-prompt");

    const completed = appReducer(
      staleCurrentStop,
      turnComplete("current", "current-prompt", { stopReason: "end_turn" }),
    );
    expect(completed.timeline.at(-1)).toMatchObject({ type: "stop" });
    expect(completed.running).toBe(false);
    expect(completed.pendingPrompt).toBeUndefined();

    expect(lateStop.running).toBe(true);
    expect(JSON.stringify(staleCurrentStop.timeline)).not.toContain("late");
    expect(staleCurrentStop.backgroundEvents).toHaveLength(1);
  });

  it("updates ID-addressed plans in place and preserves their timeline position", () => {
    const active = { ...initialState, session: { sessionId: "s1" } };
    const first = appReducer(active, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "plan_update",
          plan: { type: "markdown", planId: "p1", content: "first" },
        },
      },
    }));
    const withMessage = appReducer(first, {
      type: "user/prompt",
      requestId: "plan-prompt",
      sessionId: "s1",
      blocks: [{ type: "text", text: "next" }],
    });
    const updated = appReducer(withMessage, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "plan_update",
          plan: { type: "markdown", planId: "p1", content: "second" },
        },
      },
    }));

    expect(updated.timeline).toHaveLength(2);
    const plan = updated.timeline[0];
    expect(plan.type).toBe("plan");
    if (plan.type === "plan" && plan.update.sessionUpdate === "plan_update") {
      expect(plan.update.plan).toMatchObject({ content: "second" });
      expect(plan.raw.at(-1)).toMatchObject({ update: { plan: { planId: "p1", content: "second" } } });
    }

    const removed = appReducer(updated, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: { sessionUpdate: "plan_removed", planId: "p1" },
      },
    }));
    expect(removed.timeline).toHaveLength(2);
    expect(removed.timeline[0]).toMatchObject({
      id: "plan:p1",
      type: "plan",
      update: { sessionUpdate: "plan_removed", planId: "p1" },
      raw: expect.any(Array),
    });
    if (removed.timeline[0].type === "plan") {
      expect(removed.timeline[0].raw.at(-1)).toMatchObject({ update: { sessionUpdate: "plan_removed", planId: "p1" } });
    }

    const orphanRemoval = appReducer(active, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: { sessionUpdate: "plan_removed", planId: "unknown" },
      },
    }));
    expect(orphanRemoval.timeline[0]).toMatchObject({ type: "protocol" });
  });

  it("keeps the live ACP plan beside the composer and snapshots it after completion", () => {
    let state = appReducer(
      { ...initialState, session: { sessionId: "s1" } },
      {
        type: "user/prompt",
        requestId: "plan-turn",
        sessionId: "s1",
        blocks: [{ type: "text", text: "Do the work" }],
      },
    );
    state = appReducer(state, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "plan",
          entries: [
            { content: "Inspect", priority: "high", status: "completed" },
            { content: "Fix", priority: "high", status: "in_progress" },
          ],
        },
      },
    }));
    expect(state.activePlan?.update).toMatchObject({ sessionUpdate: "plan" });
    expect(state.timeline.some(({ type }) => type === "plan")).toBe(false);

    state = appReducer(state, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "plan",
          entries: [
            { content: "Inspect", priority: "high", status: "completed" },
            { content: "Fix", priority: "high", status: "completed" },
          ],
        },
      },
    }));
    state = appReducer(state, turnComplete("s1", "plan-turn", { stopReason: "end_turn" }));

    expect(state.activePlan).toBeUndefined();
    expect(state.timeline.map(({ type }) => type)).toEqual(["message", "plan", "stop"]);
  });

  it("applies compaction patch and streaming-summary semantics in place", () => {
    const active = { ...initialState, session: { sessionId: "s1" } };
    const started = appReducer(active, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "compaction_update",
          compactionId: "c1",
          status: "in_progress",
        },
      },
    }));
    const chunked = appReducer(started, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "compaction_summary_chunk",
          compactionId: "c1",
          content: { type: "text", text: "summary" },
        },
      },
    }));
    const completed = appReducer(chunked, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "compaction_update",
          compactionId: "c1",
          status: "completed",
        },
      },
    }));

    expect(completed.timeline).toHaveLength(1);
    const compaction = completed.timeline[0];
    expect(compaction.type).toBe("compaction");
    if (compaction.type === "compaction") {
      expect(compaction.status).toBe("completed");
      expect(compaction.blocks).toEqual([{ type: "text", text: "summary" }]);
      expect(compaction.raw.at(-1)).toMatchObject({ update: { compactionId: "c1", status: "completed" } });
    }
  });

  it("preserves cumulative cost when a usage update only changes context tokens", () => {
    const active = { ...initialState, session: { sessionId: "s1" } };
    const withCost = appReducer(active, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "usage_update",
          used: 40,
          size: 100,
          cost: { amount: 0.25, currency: "USD" },
        },
      },
    }));
    const tokensOnly = appReducer(withCost, ({ type: "session/turn_update", notification: {
        sessionId: "s1",
        update: { sessionUpdate: "usage_update", used: 60, size: 100 },
      },
    }));

    expect(tokensOnly.usage).toEqual({
      used: 60,
      size: 100,
      cost: { amount: 0.25, currency: "USD" },
    });
  });

});
