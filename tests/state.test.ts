import { describe, expect, it } from "vitest";
import type { ServerEvent } from "../shared/bridge";
import { appReducer, initialState, type AppState } from "../web/src/lib/state";

function event(value: ServerEvent) {
  return { type: "server/event" as const, event: value };
}

describe("ACP UI state", () => {
  it("tracks a remote session workspace and clears it when the thread closes", () => {
    let state = appReducer(initialState, event({
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

    state = appReducer(state, event({
      type: "acp/session_created",
      requestId: "new",
      cwd: "/home/agent/project",
      response: { sessionId: "session" },
    }));
    expect(state.cwd).toBe("/home/agent/project");

    state = appReducer(state, {
      type: "session/transition_start",
      kind: "close",
      requestId: "close",
      sessionId: "session",
    });
    state = appReducer(state, event({
      type: "acp/session_closed",
      requestId: "close",
      sessionId: "session",
    }));
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
      state = appReducer(state, event({
        type: "acp/session_update",
        notification: {
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
    state = appReducer(state, event({
      type: "bridge/error",
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
    state = appReducer(state, event({
      type: "bridge/error",
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

    state = appReducer(state, event({
      type: "acp/session_update",
      notification: {
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

    state = appReducer(state, event({
      type: "acp/session_update",
      notification: {
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

    state = appReducer(state, event({
      type: "acp/session_update",
      notification: {
        sessionId: "session",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "answer",
          content: { type: "text", text: "Done" },
        },
      },
    }));
    expect(state.agentActivity).toEqual({ kind: "responding" });

    state = appReducer(state, event({
      type: "acp/prompt_complete",
      requestId: "prompt",
      sessionId: "session",
      response: { stopReason: "end_turn" },
    }));
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
    state = appReducer(state, event({
      type: "bridge/error",
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
    let state = appReducer(initialState, event({
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
    state = appReducer(state, event({
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
    state = appReducer(state, event({
      type: "acp/logged_out",
      requestId: "logout-request",
      response: {},
    }));
    expect(state.authStatus).toBe("logged_out");
    expect(state.timeline).toEqual([]);
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
    state = appReducer(state, event({
      type: "bridge/error",
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
    state = appReducer(state, event({
      type: "bridge/error",
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
    state = appReducer(state, event({
      type: "bridge/auth_terminal_started",
      requestId: "terminal-auth",
      methodId: "terminal-login",
    }));
    state = appReducer(state, event({
      type: "bridge/auth_terminal_output",
      requestId: "terminal-auth",
      data: "Enter code: ",
    }));
    expect(state.authTerminal).toMatchObject({
      status: "running",
      output: "Enter code: ",
    });
    state = appReducer(state, event({
      type: "bridge/error",
      requestId: "terminal-auth",
      operation: "auth/terminal_resize",
      message: "Resize was rejected",
    }));
    expect(state.pendingAuth).toMatchObject({ kind: "terminal", requestId: "terminal-auth" });
    expect(state.authTerminal).toMatchObject({
      status: "running",
      message: "Resize was rejected",
    });
    state = appReducer(state, event({
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
    let state = appReducer(initialState, event({
      type: "bridge/runtime_replay_started",
      sessionCount: 0,
    }));
    state = appReducer(state, event({
      type: "bridge/auth_terminal_started",
      requestId: "terminal-auth",
      methodId: "terminal-login",
    }));
    state = appReducer(state, event({
      type: "bridge/auth_terminal_output",
      requestId: "terminal-auth",
      data: "Enter code: ",
    }));
    state = appReducer(state, event({
      type: "bridge/runtime_replay_complete",
      sessionIds: [],
    }));
    state = appReducer(state, { type: "runtime/replay_complete" });
    state = appReducer(state, event({
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

  it("tracks ephemeral MCP connections and bounds raw transport activity", () => {
    let state = appReducer(initialState, event({
      type: "acp/mcp_connection",
      action: "connected",
      serverId: "tools",
      connectionId: "connection-1",
      name: "Tools",
    }));
    for (let index = 0; index < 140; index += 1) {
      state = appReducer(state, event({
        type: "acp/mcp_message",
        direction: index % 2 === 0 ? "agent-to-server" : "server-to-agent",
        connectionId: "connection-1",
        method: `fixture/${index}`,
        kind: index % 3 === 0 ? "notification" : "request",
        params: { index },
      }));
    }
    expect(state.mcpConnections).toEqual([{
      serverId: "tools",
      connectionId: "connection-1",
      name: "Tools",
    }]);
    expect(state.mcpActivity).toHaveLength(100);
    expect(state.mcpActivity[0].method).toBe("fixture/40");

    state = appReducer(state, event({
      type: "acp/mcp_connection",
      action: "disconnected",
      serverId: "tools",
      connectionId: "connection-1",
      name: "Tools",
    }));
    expect(state.mcpConnections).toEqual([]);
  });

  it("isolates, replaces, bounds, and releases terminal snapshots", () => {
    let state = {
      ...initialState,
      session: { sessionId: "current" },
    };
    state = appReducer(state, event({
      type: "acp/terminal_state",
      terminal: {
        sessionId: "current",
        terminalId: "terminal-0",
        output: "first",
        truncated: false,
        released: false,
      },
    }));
    state = appReducer(state, event({
      type: "acp/terminal_state",
      terminal: {
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

    state = appReducer(state, event({
      type: "acp/terminal_state",
      terminal: {
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
      state = appReducer(state, event({
        type: "acp/terminal_state",
        terminal: {
          sessionId: "current",
          terminalId: `terminal-${index}`,
          output: String(index),
          truncated: false,
          released: false,
        },
      }));
    }
    expect(state.terminalSnapshots).toHaveLength(64);
    expect(state.terminalSnapshots[0].terminalId).toBe("terminal-7");

    state = appReducer(state, { type: "socket/closed" });
    expect(state.terminalSnapshots.every(({ released }) => released)).toBe(true);
  });

  it("uses the Agent's authoritative config response instead of the requested value", () => {
    const pending = appReducer({
      ...initialState,
      session: { sessionId: "session" },
      configOptions: [
        { type: "boolean", id: "verbose", name: "Verbose", currentValue: false },
      ],
    }, {
      type: "session/control_start",
      kind: "config",
      requestId: "config",
      sessionId: "session",
    });
    const state = appReducer(pending, event({
      type: "acp/config_changed",
      requestId: "config",
      sessionId: "session",
      configId: "verbose",
      value: true,
      response: {
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
      },
    }));
    expect(state.configOptions).toHaveLength(2);
    expect(state.configOptions[0]).toMatchObject({ id: "verbose", currentValue: false });
    expect(state.configOptions[1]).toMatchObject({ id: "model", currentValue: "agent-default" });
  });

  it("accepts Bridge-owned lifecycle events while isolating unmatched controls", () => {
    const active = {
      ...initialState,
      session: { sessionId: "current" },
      modeId: "build",
      configOptions: [
        { type: "boolean" as const, id: "verbose", name: "Verbose", currentValue: false },
      ],
      sessions: [{ sessionId: "saved", cwd: "/workspace", title: "Saved" }],
    };
    const acknowledgements: ServerEvent[] = [
      {
        type: "acp/session_created",
        requestId: "unsolicited-new",
        response: { sessionId: "created" },
      },
      {
        type: "acp/session_attached",
        requestId: "unsolicited-attach",
        method: "load",
        sessionId: "saved",
        response: {},
      },
      {
        type: "acp/session_forked",
        requestId: "unsolicited-fork",
        sourceSessionId: "current",
        response: { sessionId: "forked" },
      },
      {
        type: "acp/session_closed",
        requestId: "unsolicited-close",
        sessionId: "current",
      },
      {
        type: "acp/session_deleted",
        requestId: "unsolicited-delete",
        sessionId: "saved",
      },
      {
        type: "acp/mode_changed",
        requestId: "unsolicited-mode",
        sessionId: "current",
        modeId: "plan",
      },
      {
        type: "acp/config_changed",
        requestId: "unsolicited-config",
        sessionId: "current",
        configId: "verbose",
        value: true,
        response: {
          configOptions: [
            { type: "boolean", id: "verbose", name: "Verbose", currentValue: true },
          ],
        },
      },
    ];
    const isolated = acknowledgements.reduce(
      (state, acknowledgement) => appReducer(state, event(acknowledgement)),
      active,
    );
    expect(isolated.session).toBeUndefined();
    expect(isolated.modeId).toBeUndefined();
    expect(isolated.configOptions).toEqual([]);
    expect(isolated.sessions.map(({ sessionId }) => sessionId).sort())
      .toEqual(["created", "forked"]);
    expect(isolated.cachedSessions.has("created")).toBe(true);
    expect(isolated.cachedSessions.has("forked")).toBe(true);
    expect(isolated.cachedSessions.has("saved")).toBe(false);
    expect(isolated.backgroundEvents).toEqual(acknowledgements.slice(-2));

    const pendingAttach = appReducer(isolated, {
      type: "session/transition_start",
      kind: "attach",
      requestId: "attach-current",
      sessionId: "saved",
    });
    const wrongTarget = appReducer(pendingAttach, event({
      type: "acp/session_attached",
      requestId: "attach-current",
      method: "load",
      sessionId: "other",
      response: {},
    }));
    expect(wrongTarget.pendingSessionId).toBe("saved");
    expect(wrongTarget.sessionTransition?.requestId).toBe("attach-current");
    expect(wrongTarget.backgroundEvents.at(-1)).toMatchObject({
      type: "acp/session_attached",
      sessionId: "other",
    });
  });

  it("commits a new session only through its matching transition", () => {
    const pending = appReducer(initialState, {
      type: "session/transition_start",
      kind: "new",
      requestId: "new-current",
    });
    const committed = appReducer(pending, event({
      type: "acp/session_created",
      requestId: "new-current",
      response: { sessionId: "created" },
    }));
    expect(committed.session?.sessionId).toBe("created");
    expect(committed.sessionTransition).toBeUndefined();
  });

  it("commits only the matching session control response and unlocks failures", () => {
    const active = {
      ...initialState,
      session: { sessionId: "controls" },
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

    const stale = appReducer(pendingMode, event({
      type: "acp/mode_changed",
      requestId: "mode-stale",
      sessionId: "controls",
      modeId: "plan",
    }));
    expect(stale.modeId).toBe("build");
    expect(stale.pendingSessionControl).toEqual(pendingMode.pendingSessionControl);
    expect(stale.backgroundEvents).toHaveLength(1);

    const failed = appReducer(stale, event({
      type: "bridge/error",
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

    const committed = appReducer(retrying, event({
      type: "acp/config_changed",
      requestId: "config-retry",
      sessionId: "controls",
      configId: "verbose",
      value: true,
      response: {
        configOptions: [
          { type: "boolean", id: "verbose", name: "Verbose", currentValue: true },
        ],
      },
    }));
    expect(committed.pendingSessionControl).toBeUndefined();
    expect(committed.configOptions).toEqual([
      expect.objectContaining({ id: "verbose", currentValue: true }),
    ]);

    const recovered = appReducer(active, event({
      type: "acp/mode_changed",
      requestId: "mode-from-disconnected-browser",
      sessionId: "controls",
      modeId: "plan",
    }));
    expect(recovered.modeId).toBe("plan");
  });

  it("settles authoritative interactions globally while correlating response errors exactly", () => {
    const active = {
      ...initialState,
      session: { sessionId: "session" },
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
    const unrelated = appReducer(responding, event({
      type: "acp/permission_resolved",
      permissionId: "unknown-permission",
      requestId: "stale-response",
    }));
    expect(unrelated.permissions).toHaveLength(1);
    expect(unrelated.backgroundEvents).toHaveLength(1);

    const resolvedElsewhere = appReducer(responding, event({
      type: "acp/permission_resolved",
      permissionId: "permission",
      requestId: "another-browser-response",
    }));
    expect(resolvedElsewhere.permissions).toEqual([]);

    state = appReducer(responding, event({
      type: "bridge/error",
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
    state = appReducer(state, event({
      type: "acp/permission_resolved",
      permissionId: "permission",
      requestId: "permission-retry",
    }));
    expect(state.permissions).toEqual([]);

    state = appReducer(state, event({
      type: "acp/elicitation_resolved",
      elicitationId: "form",
      response: { action: "cancel" },
    }));
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
    const timestampOnly = appReducer(active, event({
      type: "acp/session_update",
      notification: {
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

    const cleared = appReducer(timestampOnly, event({
      type: "acp/session_update",
      notification: {
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
      nesSuggestions: [{
        kind: "jump" as const,
        id: "jump",
        uri: "file:///workspace/file.ts",
        position: { line: 0, character: 0 },
      }],
    };
    const disconnected = appReducer(active, { type: "socket/closed" });
    expect(disconnected.permissions).toEqual([]);
    expect(disconnected.elicitations).toEqual([]);
    expect(disconnected.externalFlows).toEqual([]);
    expect(disconnected.nesSuggestions).toEqual([]);

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
        stage: "deleting" as const,
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
      nesSessionId: "nes",
      nesDocuments: [{
        sessionId: "nes",
        path: "/workspace/file.ts",
        uri: "file:///workspace/file.ts",
        languageId: "typescript",
        version: 1,
        text: "draft",
      }],
      nesDrafts: { "file:///workspace/file.ts": "newer draft" },
      activeNesUri: "file:///workspace/file.ts",
      nesSuggestions: [{
        kind: "jump" as const,
        id: "jump",
        uri: "file:///workspace/file.ts",
        position: { line: 0, character: 0 },
      }],
      mcpConnections: [{ serverId: "tools", connectionId: "connection", name: "Tools" }],
    }, event({ type: "bridge/phase", phase: "error" }));

    expect(failed.phase).toBe("error");
    expect(failed.socketOpen).toBe(true);
    expect(failed.session?.sessionId).toBe("current");
    expect(failed.timeline).toEqual(active.timeline);
    expect(failed.sessionTransition).toBeUndefined();
    expect(failed.pendingSessionDeletions).toEqual([]);
    expect(failed.pendingSessionControl).toBeUndefined();
    expect(failed.permissions).toEqual([]);
    expect(failed.mcpConnections).toEqual([]);
    expect(failed.nesSessionId).toBeUndefined();
    expect(failed.nesDocuments).toEqual([]);
    expect(failed.nesDrafts).toEqual({});
    expect(failed.terminalSnapshots[0]?.released).toBe(true);
  });

  it("isolates session-scoped elicitations while preserving request-scoped interactions", () => {
    const active = {
      ...initialState,
      session: { sessionId: "current" },
    };
    const background = appReducer(active, event({
      type: "acp/elicitation_request",
      elicitationId: "other-form",
      request: {
        sessionId: "other",
        mode: "form",
        message: "Wrong session",
        requestedSchema: { type: "object", properties: {} },
      },
    }));
    expect(background.elicitations).toEqual([]);
    expect(background.backgroundEvents).toHaveLength(1);

    const withCurrent = appReducer(background, event({
      type: "acp/elicitation_request",
      elicitationId: "current-form",
      request: {
        sessionId: "current",
        mode: "form",
        message: "Current session",
        requestedSchema: { type: "object", properties: {} },
      },
    }));
    const withRequestScope = appReducer(withCurrent, event({
      type: "acp/elicitation_request",
      elicitationId: "global-form",
      request: {
        requestId: "outer-request",
        mode: "form",
        message: "Connection-scoped request",
        requestedSchema: { type: "object", properties: {} },
      },
    }));
    expect(withRequestScope.elicitations.map(({ elicitationId }) => elicitationId))
      .toEqual(["current-form", "global-form"]);

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
    expect(pending.session).toBeUndefined();
    expect(pending.pendingSessionId).toBe("saved");
    expect(pending.timeline).toEqual([]);

    pending = appReducer(pending, event({
      type: "acp/session_update",
      notification: {
        sessionId: "saved",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "replayed",
          content: { type: "text", text: "Loaded history" },
        },
      },
    }));
    expect(pending.timeline).toHaveLength(1);

    const restored = appReducer(pending, event({
      type: "bridge/error",
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

  it("switches back to an already-open thread locally and keeps its background updates", () => {
    const first = {
      ...initialState,
      phase: "ready" as const,
      session: { sessionId: "first" },
      cwd: "/workspace/first",
      title: "First",
      timeline: [{
        id: "first-answer",
        type: "assistant" as const,
        chunks: [{
          id: "first-answer-chunk",
          role: "agent" as const,
          blocks: [{ type: "text" as const, text: "First answer" }],
          raw: [],
        }],
      }],
    };
    let state = appReducer(first, {
      type: "session/transition_start",
      kind: "attach",
      requestId: "load-second",
      sessionId: "second",
      cwd: "/workspace/second",
      title: "Second",
    });
    state = appReducer(state, event({
      type: "acp/session_attached",
      requestId: "load-second",
      method: "load",
      sessionId: "second",
      cwd: "/workspace/second",
      response: {},
    }));
    state = appReducer(state, event({
      type: "acp/session_update",
      notification: {
        sessionId: "second",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "second-answer",
          content: { type: "text", text: "Second answer" },
        },
      },
    }));

    state = appReducer(state, { type: "session/activate_cached", sessionId: "first" });
    expect(state.session?.sessionId).toBe("first");
    expect(JSON.stringify(state.timeline)).toContain("First answer");
    expect(state.cachedSessions.has("second")).toBe(true);

    state = appReducer(state, event({
      type: "acp/session_update",
      notification: {
        sessionId: "second",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "second-late-answer",
          content: { type: "text", text: "Late background update" },
        },
      },
    }));
    expect(state.backgroundEvents).toHaveLength(0);

    state = appReducer(state, { type: "session/activate_cached", sessionId: "second" });
    expect(state.session?.sessionId).toBe("second");
    expect(JSON.stringify(state.timeline)).toContain("Second answer");
    expect(JSON.stringify(state.timeline)).toContain("Late background update");
    expect(state.cachedSessions.has("first")).toBe(true);
  });

  it("replays and independently tracks concurrent in-memory session turns", () => {
    let state = appReducer(initialState, event({
      type: "bridge/runtime_replay_started",
      sessionCount: 2,
    }));
    for (const [sessionId, cwd] of [["first", "/workspace/first"], ["second", "/workspace/second"]]) {
      state = appReducer(state, event({
        type: "bridge/runtime_session",
        sessionId,
        cwd,
        session: { sessionId },
        truncated: false,
      }));
      state = appReducer(state, event({
        type: "acp/prompt_started",
        requestId: `${sessionId}-prompt`,
        sessionId,
        prompt: [{ type: "text", text: `Prompt ${sessionId}` }],
      }));
      state = appReducer(state, event({
        type: "acp/session_update",
        notification: {
          sessionId,
          update: {
            sessionUpdate: "agent_message_chunk",
            messageId: `${sessionId}-answer`,
            content: { type: "text", text: `Streaming ${sessionId}` },
          },
        },
      }));
    }
    state = appReducer(state, event({
      type: "bridge/runtime_replay_complete",
      sessionIds: ["first", "second"],
    }));
    state = appReducer(state, {
      type: "runtime/replay_complete",
      preferredSessionId: "first",
      fallbackSessionId: "second",
    });

    expect(state.session?.sessionId).toBe("first");
    expect(state.running).toBe(true);
    expect(JSON.stringify(state.timeline)).toContain("Streaming first");
    expect(state.cachedSessions.get("second")?.running).toBe(true);

    state = appReducer(state, { type: "session/activate_cached", sessionId: "second" });
    expect(state.session?.sessionId).toBe("second");
    expect(state.running).toBe(true);
    expect(JSON.stringify(state.timeline)).toContain("Streaming second");

    state = appReducer(state, event({
      type: "acp/prompt_complete",
      requestId: "first-prompt",
      sessionId: "first",
      response: { stopReason: "end_turn" },
    }));
    expect(state.running).toBe(true);
    expect(state.cachedSessions.get("first")?.running).toBe(false);
    expect(state.cachedSessions.get("first")?.timeline.at(-1)).toMatchObject({ type: "stop" });
  });

  it("caches background interactions without recursing while another session transitions", () => {
    const background = {
      cwd: "/workspace/background",
      session: { sessionId: "background" },
      availableCommands: [],
      configOptions: [],
      timeline: [],
      permissions: [],
      elicitations: [],
      running: true,
      terminalSnapshots: [],
    };
    let state = appReducer({
      ...initialState,
      session: { sessionId: "current" },
      cwd: "/workspace/current",
      cachedSessions: new Map([["background", background]]),
    }, {
      type: "session/transition_start",
      kind: "new",
      requestId: "new-current",
      cwd: "/workspace/new",
    });

    state = appReducer(state, event({
      type: "acp/permission_request",
      permissionId: "background-permission",
      request: {
        sessionId: "background",
        toolCall: { toolCallId: "background-tool", title: "Background tool" },
        options: [{ optionId: "allow", name: "Allow", kind: "allow_once" }],
      },
    }));
    state = appReducer(state, event({
      type: "acp/elicitation_request",
      elicitationId: "background-form",
      request: {
        sessionId: "background",
        mode: "form",
        message: "Background input",
        requestedSchema: { type: "object", properties: {} },
      },
    }));

    expect(state.sessionTransition?.requestId).toBe("new-current");
    expect(state.cachedSessions.get("background")?.permissions)
      .toHaveLength(1);
    expect(state.cachedSessions.get("background")?.elicitations)
      .toHaveLength(1);
  });

  it("commits replayed state only when the matching session transition succeeds", () => {
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
    state = appReducer(state, event({
      type: "acp/session_update",
      notification: {
        sessionId: "saved",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "loaded",
          content: { type: "text", text: "Loaded" },
        },
      },
    }));
    state = appReducer(state, event({
      type: "acp/session_attached",
      requestId: "load-request",
      method: "load",
      sessionId: "saved",
      response: {},
    }));
    expect(state.session?.sessionId).toBe("saved");
    expect(state.timeline).toHaveLength(1);
    expect(state.timeline[0]).toMatchObject({
      type: "assistant",
      chunks: [{ messageId: "loaded" }],
    });
    expect(state.sessionTransition).toBeUndefined();
  });

  it("coalesces text chunks only when they belong to the same message", () => {
    const first = appReducer(
      { ...initialState, session: { sessionId: "s1" } },
      event({
        type: "acp/session_update",
        notification: {
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
      event({
        type: "acp/session_update",
        notification: {
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

  it("starts a new assistant entry after an interleaved tool event", () => {
    let state = appReducer(
      { ...initialState, session: { sessionId: "s1" } },
      event({
        type: "acp/session_update",
        notification: {
          sessionId: "s1",
          update: {
            sessionUpdate: "agent_message_chunk",
            messageId: "m1",
            content: { type: "text", text: "before " },
          },
        },
      }),
    );
    state = appReducer(state, event({
      type: "acp/session_update",
      notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "tool-1",
          title: "Interleaved",
        },
      },
    }));
    state = appReducer(state, event({
      type: "acp/session_update",
      notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "m1",
          content: { type: "text", text: "after" },
        },
      },
    }));

    expect(state.timeline).toHaveLength(3);
    expect(state.timeline[0]).toMatchObject({
      type: "assistant",
      chunks: [{ messageId: "m1", blocks: [{ type: "text", text: "before " }] }],
    });
    expect(state.timeline[2]).toMatchObject({
      type: "assistant",
      chunks: [{ messageId: "m1", blocks: [{ type: "text", text: "after" }] }],
    });
  });

  it("merges anonymous adjacent chunks with an identified message like Zed", () => {
    let state = appReducer(
      { ...initialState, session: { sessionId: "s1" } },
      event({
        type: "acp/session_update",
        notification: {
          sessionId: "s1",
          update: {
            sessionUpdate: "agent_message_chunk",
            messageId: "identified",
            content: { type: "text", text: "identified" },
          },
        },
      }),
    );
    state = appReducer(state, event({
      type: "acp/session_update",
      notification: {
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
      event({
        type: "acp/session_update",
        notification: {
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
      event({
        type: "acp/session_update",
        notification: {
          sessionId: "s1",
          update: { sessionUpdate: "tool_call_update", toolCallId: "t1", status: "completed" },
        },
      }),
    );

    const item = updated.timeline[0];
    expect(item.type).toBe("tool");
    if (item.type === "tool") {
      expect(item.call).toMatchObject({ title: "Read file", kind: "read", status: "completed" });
      expect(item.raw).toHaveLength(2);
    }
  });

  it("renders a failed placeholder for a tool update without a creation", () => {
    const state = appReducer(
      { ...initialState, session: { sessionId: "s1" } },
      event({
        type: "acp/session_update",
        notification: {
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
    const listed = appReducer(
      initialState,
      event({
        type: "acp/sessions_listed",
        requestId: "list-1",
        response: {
          sessions: [{ sessionId: "saved", cwd: "/workspace", title: "Saved" }],
          nextCursor: "next",
        },
      }),
    );
    const pending = appReducer(listed, {
      type: "session/transition_start",
      kind: "attach",
      requestId: "load-1",
      sessionId: "saved",
      title: "Saved",
    });
    const attached = appReducer(
      pending,
      event({
        type: "acp/session_attached",
        requestId: "load-1",
        method: "load",
        sessionId: "saved",
        response: {},
      }),
    );

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

    const wrongRequest = appReducer(pending, event({
      type: "acp/session_forked",
      requestId: "fork-stale",
      sourceSessionId: "source",
      response: { sessionId: "stale-fork" },
    }));
    expect(wrongRequest.session?.sessionId).toBe("source");
    expect(wrongRequest.backgroundEvents).toHaveLength(1);

    const unrelated = appReducer(wrongRequest, event({
      type: "acp/session_forked",
      requestId: "fork-other",
      sourceSessionId: "other",
      response: { sessionId: "other-fork" },
    }));
    expect(unrelated.session?.sessionId).toBe("source");
    expect(unrelated.backgroundEvents).toHaveLength(2);

    const forked = appReducer(unrelated, event({
      type: "acp/session_forked",
      requestId: "fork-current",
      sourceSessionId: "source",
      response: {
        sessionId: "forked",
        modes: {
          currentModeId: "plan",
          availableModes: [{ id: "plan", name: "Plan" }],
        },
      },
    }));
    expect(forked.session?.sessionId).toBe("forked");
    expect(forked.modeId).toBe("plan");
    expect(forked.timeline).toEqual(active.timeline);
    expect(forked.running).toBe(false);
    expect(forked.sessionTransition).toBeUndefined();

    const failed = appReducer(pending, event({
      type: "bridge/error",
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

    const closedElsewhere = appReducer(pending, event({
      type: "acp/session_closed",
      requestId: "close-from-another-browser",
      sessionId: "closing",
    }));
    expect(closedElsewhere.session).toBeUndefined();
    expect(closedElsewhere.timeline).toEqual([]);
    expect(closedElsewhere.sessionTransition).toBeUndefined();

    const repeated = appReducer(closedElsewhere, event({
      type: "acp/session_closed",
      requestId: "close-current",
      sessionId: "closing",
    }));
    expect(repeated.session).toBeUndefined();
    expect(repeated.timeline).toEqual([]);
    expect(repeated.sessionTransition).toBeUndefined();

    const failed = appReducer(pending, event({
      type: "bridge/error",
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
      stage: "deleting",
    });
    expect(pending.pendingSessionDeletions).toEqual([
      { requestId: "delete-current", sessionId: "saved", stage: "deleting" },
    ]);

    const deletedElsewhere = appReducer(pending, event({
      type: "acp/session_deleted",
      requestId: "delete-from-another-browser",
      sessionId: "saved",
    }));
    expect(deletedElsewhere.sessions).toEqual([]);
    expect(deletedElsewhere.pendingSessionDeletions).toEqual([]);

    const failed = appReducer(pending, event({
      type: "bridge/error",
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
      stage: "deleting",
    });
    const deleted = appReducer(retrying, event({
      type: "acp/session_deleted",
      requestId: "delete-retry",
      sessionId: "saved",
    }));
    expect(deleted.sessions).toEqual([]);
    expect(deleted.pendingSessionDeletions).toEqual([]);
  });

  it("deselects the current session and advances close-before-delete atomically", () => {
    const active = {
      ...initialState,
      session: { sessionId: "current" },
      title: "Current thread",
      cwd: "/workspace/current",
      sessions: [{
        sessionId: "current",
        cwd: "/workspace/current",
        title: "Current thread",
      }],
      timeline: [{ id: "answer", type: "agent_message" as const, text: "Kept snapshot" }],
    };
    const closing = appReducer(active, {
      type: "session/delete_start",
      requestId: "close-current-for-delete",
      sessionId: "current",
      stage: "closing",
    });
    expect(closing.session).toBeUndefined();
    expect(closing.title).toBeUndefined();
    expect(closing.cachedSessions.get("current")?.timeline).toEqual(active.timeline);
    expect(closing.pendingSessionDeletions).toEqual([{
      requestId: "close-current-for-delete",
      sessionId: "current",
      stage: "closing",
    }]);

    const closed = appReducer(closing, event({
      type: "acp/session_closed",
      requestId: "close-current-for-delete",
      sessionId: "current",
    }));
    expect(closed.cachedSessions.has("current")).toBe(false);
    expect(closed.backgroundEvents).toEqual([]);

    const deleting = appReducer(closed, {
      type: "session/delete_continue",
      closeRequestId: "close-current-for-delete",
      requestId: "delete-current",
      sessionId: "current",
    });
    expect(deleting.pendingSessionDeletions).toEqual([{
      requestId: "delete-current",
      sessionId: "current",
      stage: "deleting",
    }]);

    const deleted = appReducer(deleting, event({
      type: "acp/session_deleted",
      requestId: "delete-current",
      sessionId: "current",
    }));
    expect(deleted.sessions).toEqual([]);
    expect(deleted.pendingSessionDeletions).toEqual([]);
  });

  it("keeps a current-session snapshot recoverable when delete close fails", () => {
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
      stage: "closing",
    });
    const failed = appReducer(closing, event({
      type: "bridge/error",
      requestId: "close-current-for-delete",
      operation: "session/close",
      message: "Synthetic close failure",
    }));
    expect(failed.session).toBeUndefined();
    expect(failed.pendingSessionDeletions).toEqual([]);
    expect(failed.cachedSessions.get("current")?.timeline).toEqual(active.timeline);
    expect(failed.timeline.at(-1)).toMatchObject({
      type: "error",
      message: "Synthetic close failure",
    });

    const restored = appReducer(failed, {
      type: "session/activate_cached",
      sessionId: "current",
    });
    expect(restored.session?.sessionId).toBe("current");
    expect(restored.timeline).toEqual(active.timeline);
  });

  it("tracks URL consent and completion as separate states", () => {
    const requested = appReducer(
      { ...initialState, session: { sessionId: "s1" } },
      event({
        type: "acp/elicitation_request",
        elicitationId: "bridge-flow",
        request: {
          sessionId: "s1",
          mode: "url",
          message: "Connect account",
          elicitationId: "agent-flow",
          url: "https://example.test/connect",
        },
      }),
    );
    const accepted = appReducer(requested, event({
      type: "acp/elicitation_resolved",
      elicitationId: "bridge-flow",
      response: { action: "accept" },
    }));
    expect(accepted.externalFlows).toEqual([
      expect.objectContaining({ elicitationId: "agent-flow", status: "waiting" }),
    ]);

    const switched = appReducer(accepted, { type: "session/reset" });
    expect(switched.externalFlows).toEqual([
      expect.objectContaining({ elicitationId: "agent-flow", status: "waiting" }),
    ]);

    const completed = appReducer(
      switched,
      event({
        type: "acp/elicitation_complete",
        notification: { elicitationId: "agent-flow" },
      }),
    );
    expect(completed.externalFlows[0]?.status).toBe("completed");
  });

  it("does not regress an already completed external flow to waiting", () => {
    const state = appReducer({
      ...initialState,
      session: { sessionId: "session" },
      externalFlows: [{
        elicitationId: "agent-flow",
        message: "External flow",
        status: "completed" as const,
      }],
      elicitations: [{
        elicitationId: "bridge-flow",
        request: {
          sessionId: "session",
          mode: "url" as const,
          message: "Connect",
          elicitationId: "agent-flow",
          url: "https://example.test/connect",
        },
      }],
    }, event({
      type: "acp/elicitation_resolved",
      elicitationId: "bridge-flow",
      response: { action: "accept" },
    }));
    expect(state.externalFlows).toEqual([
      expect.objectContaining({ elicitationId: "agent-flow", status: "completed" }),
    ]);
  });

  it("terminates only the matching session-scoped external flow", () => {
    const sessionRequested = appReducer(
      { ...initialState, session: { sessionId: "session" } },
      event({
        type: "acp/elicitation_request",
        elicitationId: "bridge-session-flow",
        request: {
          sessionId: "session",
          mode: "url",
          message: "Session connection",
          elicitationId: "session-flow",
          url: "https://example.test/session",
        },
      }),
    );
    const accepted = appReducer(sessionRequested, event({
      type: "acp/elicitation_resolved",
      elicitationId: "bridge-session-flow",
      response: { action: "accept" },
    }));
    expect(accepted.externalFlows).toEqual([
      expect.objectContaining({
        elicitationId: "session-flow",
        sessionId: "session",
        status: "waiting",
      }),
    ]);

    const wrongSession = appReducer(accepted, event({
      type: "acp/elicitation_aborted",
      elicitationId: "session-flow",
      sessionId: "other",
      reason: "session_closed",
    }));
    expect(wrongSession.externalFlows[0]?.status).toBe("waiting");
    expect(wrongSession.backgroundEvents).toHaveLength(1);

    const cancelled = appReducer(wrongSession, event({
      type: "acp/elicitation_aborted",
      elicitationId: "session-flow",
      sessionId: "session",
      reason: "session_cancelled",
    }));
    expect(cancelled.externalFlows[0]).toMatchObject({
      status: "cancelled",
      abortReason: "session_cancelled",
    });

    const lateCompletion = appReducer(cancelled, event({
      type: "acp/elicitation_complete",
      notification: { elicitationId: "session-flow" },
    }));
    expect(lateCompletion.externalFlows[0]?.status).toBe("cancelled");
  });

  it("isolates NES sessions and keeps optimistic document text through acknowledgements", () => {
    const started = appReducer(initialState, event({
      type: "acp/nes_started",
      requestId: "nes-start",
      response: { sessionId: "nes-current" },
    }));
    const wrongSession = appReducer(started, event({
      type: "acp/document_opened",
      requestId: "wrong-open",
      document: {
        sessionId: "nes-other",
        path: "/workspace/other.ts",
        uri: "file:///workspace/other.ts",
        languageId: "typescript",
        version: 1,
        text: "other",
      },
    }));
    expect(wrongSession.nesDocuments).toHaveLength(0);
    expect(wrongSession.backgroundEvents).toHaveLength(1);

    const opened = appReducer(wrongSession, event({
      type: "acp/document_opened",
      requestId: "open",
      document: {
        sessionId: "nes-current",
        path: "/workspace/file.ts",
        uri: "file:///workspace/file.ts",
        languageId: "typescript",
        version: 1,
        text: "one",
      },
    }));
    const suggesting = appReducer(opened, {
      type: "nes/suggest_start",
      requestId: "suggest",
      sessionId: "nes-current",
      uri: "file:///workspace/file.ts",
      triggerKind: "automatic",
    });
    const suggested = appReducer(suggesting, event({
      type: "acp/nes_suggestions",
      requestId: "suggest",
      sessionId: "nes-current",
      uri: "file:///workspace/file.ts",
      response: {
        suggestions: [{
          kind: "jump",
          id: "stale-jump",
          uri: "file:///workspace/file.ts",
          position: { line: 0, character: 0 },
        }],
      },
    }));
    const optimistic = appReducer(suggested, {
      type: "nes/local_change",
      uri: "file:///workspace/file.ts",
      text: "two",
    });
    expect(optimistic.nesDocuments[0]?.text).toBe("one");
    expect(optimistic.nesDrafts["file:///workspace/file.ts"]).toBe("two");
    expect(optimistic.nesSuggestions).toHaveLength(0);

    const invalidated = appReducer(optimistic, event({
      type: "acp/nes_suggestion_resolved",
      requestId: "change",
      sessionId: "nes-current",
      suggestionId: "stale-jump",
      outcome: "rejected",
      reason: "replaced",
    }));
    expect(invalidated.nesSuggestions).toEqual([]);

    const newerDraft = appReducer(invalidated, {
      type: "nes/local_change",
      uri: "file:///workspace/file.ts",
      text: "three",
    });
    const staleAcknowledgement = appReducer(newerDraft, event({
      type: "acp/document_changed",
      requestId: "change",
      document: {
        ...newerDraft.nesDocuments[0]!,
        version: 2,
        text: "two",
      },
    }));
    expect(staleAcknowledgement.nesDocuments[0]).toMatchObject({ version: 2, text: "two" });
    expect(staleAcknowledgement.nesDrafts["file:///workspace/file.ts"]).toBe("three");

    const acknowledged = appReducer(staleAcknowledgement, event({
      type: "acp/document_changed",
      requestId: "change-latest",
      document: {
        ...staleAcknowledgement.nesDocuments[0]!,
        version: 3,
        text: "three",
      },
    }));
    expect(acknowledged.nesDrafts["file:///workspace/file.ts"]).toBeUndefined();
  });

  it("commits or rolls back optimistic NES acceptance by exact request", () => {
    const uri = "file:///workspace/file.ts";
    const started = appReducer(initialState, event({
      type: "acp/nes_started",
      requestId: "start",
      response: { sessionId: "nes" },
    }));
    const opened = appReducer(started, event({
      type: "acp/document_opened",
      requestId: "open",
      document: {
        sessionId: "nes",
        path: "/workspace/file.ts",
        uri,
        languageId: "typescript",
        version: 1,
        text: "one",
      },
    }));
    const suggesting = appReducer(opened, {
      type: "nes/suggest_start",
      requestId: "suggest",
      sessionId: "nes",
      uri,
      triggerKind: "manual",
    });
    const suggested = appReducer(suggesting, event({
      type: "acp/nes_suggestions",
      requestId: "suggest",
      sessionId: "nes",
      uri,
      response: {
        suggestions: [{
          kind: "edit",
          id: "edit",
          uri,
          edits: [{
            range: {
              start: { line: 0, character: 0 },
              end: { line: 0, character: 0 },
            },
            newText: "accepted ",
          }],
        }],
      },
    }));

    const pending = appReducer(suggested, {
      type: "nes/accept_start",
      requestId: "accept",
      sessionId: "nes",
      suggestionId: "edit",
      text: "accepted one",
    });
    expect(pending.nesDrafts[uri]).toBe("accepted one");
    expect(pending.pendingNesAccept).toMatchObject({
      requestId: "accept",
      suggestionId: "edit",
      documentAcknowledged: false,
    });

    const staleError = appReducer(pending, event({
      type: "bridge/error",
      requestId: "other",
      operation: "nes/accept",
      message: "stale failure",
    }));
    expect(staleError.pendingNesAccept?.requestId).toBe("accept");
    expect(staleError.nesDrafts[uri]).toBe("accepted one");

    const documentAcknowledged = appReducer(staleError, event({
      type: "acp/document_changed",
      requestId: "accept",
      document: {
        ...opened.nesDocuments[0]!,
        version: 2,
        text: "accepted one",
      },
    }));
    expect(documentAcknowledged.nesDrafts[uri]).toBeUndefined();
    expect(documentAcknowledged.pendingNesAccept?.documentAcknowledged).toBe(true);
    const accepted = appReducer(documentAcknowledged, event({
      type: "acp/nes_suggestion_resolved",
      requestId: "accept",
      sessionId: "nes",
      suggestionId: "edit",
      outcome: "accepted",
    }));
    expect(accepted.pendingNesAccept).toBeUndefined();
    expect(accepted.nesSuggestions).toEqual([]);
    expect(accepted.nesDocuments[0]).toMatchObject({
      version: 2,
      text: "accepted one",
    });

    const priorDraft = appReducer(suggested, {
      type: "nes/local_change",
      uri,
      text: "local one",
    });
    const suggestingLocal = appReducer(priorDraft, {
      type: "nes/suggest_start",
      requestId: "suggest-local",
      sessionId: "nes",
      uri,
      triggerKind: "automatic",
    });
    const suggestedLocal = appReducer(suggestingLocal, event({
      type: "acp/nes_suggestions",
      requestId: "suggest-local",
      sessionId: "nes",
      uri,
      response: {
        suggestions: [{
          kind: "edit",
          id: "edit",
          uri,
          edits: [{
            range: {
              start: { line: 0, character: 0 },
              end: { line: 0, character: 0 },
            },
            newText: "accepted ",
          }],
        }],
      },
    }));
    const failing = appReducer(suggestedLocal, {
      type: "nes/accept_start",
      requestId: "accept-failing",
      sessionId: "nes",
      suggestionId: "edit",
      text: "accepted local one",
    });
    const failed = appReducer(failing, event({
      type: "bridge/error",
      requestId: "accept-failing",
      operation: "nes/accept",
      message: "suggestion expired",
    }));
    expect(failed.pendingNesAccept).toBeUndefined();
    expect(failed.nesDrafts[uri]).toBe("local one");
    expect(failed.nesSuggestions).toHaveLength(1);
    expect(failed.timeline.at(-1)).toMatchObject({
      type: "error",
      message: "suggestion expired",
    });

    const changedWhilePending = appReducer(failing, {
      type: "nes/local_change",
      uri,
      text: "newer typing",
    });
    const failedAfterTyping = appReducer(changedWhilePending, event({
      type: "bridge/error",
      requestId: "accept-failing",
      operation: "nes/accept",
      message: "suggestion expired",
    }));
    expect(failedAfterTyping.nesDrafts[uri]).toBe("newer typing");
    expect(failedAfterTyping.pendingNesAccept).toBeUndefined();

    const missingDocumentAck = appReducer(failing, event({
      type: "acp/nes_suggestion_resolved",
      requestId: "accept-failing",
      sessionId: "nes",
      suggestionId: "edit",
      outcome: "accepted",
    }));
    expect(missingDocumentAck.pendingNesAccept).toBeUndefined();
    expect(missingDocumentAck.nesDrafts[uri]).toBe("local one");
    expect(missingDocumentAck.timeline.at(-1)).toMatchObject({
      type: "error",
      message: expect.stringContaining("without a matching document acknowledgement"),
    });
  });

  it("commits only the matching automatic NES response and keeps its errors local", () => {
    const uri = "file:///workspace/file.ts";
    const started = appReducer(initialState, event({
      type: "acp/nes_started",
      requestId: "start",
      response: { sessionId: "nes" },
    }));
    const opened = appReducer(started, event({
      type: "acp/document_opened",
      requestId: "open",
      document: {
        sessionId: "nes",
        path: "/workspace/file.ts",
        uri,
        languageId: "typescript",
        version: 1,
        text: "one",
      },
    }));
    const pending = appReducer(opened, {
      type: "nes/suggest_start",
      requestId: "latest",
      sessionId: "nes",
      uri,
      triggerKind: "automatic",
    });
    expect(pending.pendingNesSuggestion).toMatchObject({
      requestId: "latest",
      triggerKind: "automatic",
    });

    const stale = appReducer(pending, event({
      type: "acp/nes_suggestions",
      requestId: "stale",
      sessionId: "nes",
      uri,
      response: { suggestions: [] },
    }));
    expect(stale.pendingNesSuggestion?.requestId).toBe("latest");
    expect(stale.backgroundEvents).toHaveLength(1);

    const staleFailure = appReducer(stale, event({
      type: "bridge/error",
      requestId: "stale",
      operation: "nes/suggest",
      message: "old prediction failed",
    }));
    expect(staleFailure.pendingNesSuggestion?.requestId).toBe("latest");
    expect(staleFailure.nesError).toBeUndefined();
    expect(staleFailure.timeline).toEqual([]);
    expect(staleFailure.backgroundEvents).toHaveLength(2);

    const failed = appReducer(staleFailure, event({
      type: "bridge/error",
      requestId: "latest",
      operation: "nes/suggest",
      message: "prediction unavailable",
    }));
    expect(failed.pendingNesSuggestion).toBeUndefined();
    expect(failed.nesError).toBe("prediction unavailable");
    expect(failed.timeline).toEqual([]);
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
      event({
        type: "bridge/error",
        requestId: "list",
        operation: "session/list",
        message: "list failed",
      }),
    );
    expect(listFailed.running).toBe(true);
    expect(listFailed.pendingPrompt).toEqual(running.pendingPrompt);

    const duplicatePromptFailed = appReducer(
      running,
      event({
        type: "bridge/error",
        requestId: "prompt-duplicate",
        operation: "session/prompt",
        message: "A prompt is already running",
      }),
    );
    expect(duplicatePromptFailed.running).toBe(true);
    expect(duplicatePromptFailed.pendingPrompt).toEqual(running.pendingPrompt);

    const promptFailed = appReducer(
      running,
      event({
        type: "bridge/error",
        requestId: "prompt-current",
        operation: "session/prompt",
        message: "prompt failed",
      }),
    );
    expect(promptFailed.running).toBe(false);
    expect(promptFailed.pendingPrompt).toBeUndefined();

    const recoverableProtocolError = appReducer(
      running,
      event({
        type: "bridge/error",
        message: "Invalid Agent update was dropped",
      }),
    );
    expect(recoverableProtocolError.running).toBe(true);

    const stopped = appReducer(
      recoverableProtocolError,
      event({ type: "bridge/phase", phase: "error" }),
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
      event({
        type: "acp/session_update",
        notification: {
          sessionId: "old",
          update: {
            sessionUpdate: "agent_message_chunk",
            content: { type: "text", text: "late" },
          },
        },
      }),
    );
    const lateStop = appReducer(
      lateUpdate,
      event({
        type: "acp/prompt_complete",
        requestId: "old-prompt",
        sessionId: "old",
        response: { stopReason: "end_turn" },
      }),
    );

    const staleCurrentStop = appReducer(lateStop, event({
      type: "acp/prompt_complete",
      requestId: "stale-current-prompt",
      sessionId: "current",
      response: { stopReason: "end_turn" },
    }));
    expect(staleCurrentStop.timeline).toHaveLength(1);
    expect(staleCurrentStop.timeline[0]).toMatchObject({ role: "user" });
    expect(staleCurrentStop.running).toBe(true);
    expect(staleCurrentStop.pendingPrompt?.requestId).toBe("current-prompt");

    const completed = appReducer(staleCurrentStop, event({
      type: "acp/prompt_complete",
      requestId: "current-prompt",
      sessionId: "current",
      response: { stopReason: "end_turn" },
    }));
    expect(completed.timeline.at(-1)).toMatchObject({ type: "stop" });
    expect(completed.running).toBe(false);
    expect(completed.pendingPrompt).toBeUndefined();

    expect(lateStop.running).toBe(true);
    expect(staleCurrentStop.backgroundEvents).toHaveLength(3);
  });

  it("updates ID-addressed plans in place and preserves their timeline position", () => {
    const active = { ...initialState, session: { sessionId: "s1" } };
    const first = appReducer(active, event({
      type: "acp/session_update",
      notification: {
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
    const updated = appReducer(withMessage, event({
      type: "acp/session_update",
      notification: {
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
      expect(plan.raw).toHaveLength(2);
    }

    const removed = appReducer(updated, event({
      type: "acp/session_update",
      notification: {
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
      expect(removed.timeline[0].raw).toHaveLength(3);
    }

    const orphanRemoval = appReducer(active, event({
      type: "acp/session_update",
      notification: {
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
    state = appReducer(state, event({
      type: "acp/session_update",
      notification: {
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

    state = appReducer(state, event({
      type: "acp/session_update",
      notification: {
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
    state = appReducer(state, event({
      type: "acp/prompt_complete",
      requestId: "plan-turn",
      sessionId: "s1",
      response: { stopReason: "end_turn" },
    }));

    expect(state.activePlan).toBeUndefined();
    expect(state.timeline.map(({ type }) => type)).toEqual(["message", "plan", "stop"]);
  });

  it("applies compaction patch and streaming-summary semantics in place", () => {
    const active = { ...initialState, session: { sessionId: "s1" } };
    const started = appReducer(active, event({
      type: "acp/session_update",
      notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "compaction_update",
          compactionId: "c1",
          status: "in_progress",
        },
      },
    }));
    const chunked = appReducer(started, event({
      type: "acp/session_update",
      notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "compaction_summary_chunk",
          compactionId: "c1",
          content: { type: "text", text: "summary" },
        },
      },
    }));
    const completed = appReducer(chunked, event({
      type: "acp/session_update",
      notification: {
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
      expect(compaction.raw).toHaveLength(3);
    }
  });

  it("preserves cumulative cost when a usage update only changes context tokens", () => {
    const active = { ...initialState, session: { sessionId: "s1" } };
    const withCost = appReducer(active, event({
      type: "acp/session_update",
      notification: {
        sessionId: "s1",
        update: {
          sessionUpdate: "usage_update",
          used: 40,
          size: 100,
          cost: { amount: 0.25, currency: "USD" },
        },
      },
    }));
    const tokensOnly = appReducer(withCost, event({
      type: "acp/session_update",
      notification: {
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
