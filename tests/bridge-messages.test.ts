import { describe, expect, it } from "vitest";
import { parseServerEvent } from "../shared/bridge";

describe("browser bridge messages", () => {
  it("rejects retired browser transport and unadvertised editor events", () => {
    for (const type of ["bridge/pong", "bridge/intent_ack", "acp/nes_started", "acp/document_opened"]) {
      expect(() => parseServerEvent(JSON.stringify({ type }))).toThrow("Unknown");
    }
  });

  it("validates server event envelopes before they reach the React reducer", () => {
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/hello",
      transport: "ws",
      command: ["ws://agent.example/acp"],
      cwd: "",
      readOnly: false,
      additionalDirectories: [],
      mcpServers: [],
    }))).toMatchObject({ type: "bridge/hello", transport: "ws", cwd: "" });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/runtime_replay_started",
      sessionCount: 1,
    }))).toEqual({ type: "bridge/runtime_replay_started", sessionCount: 1 });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/runtime_session",
      sessionId: "session",
      cwd: "/workspace",
      session: { sessionId: "session" },
      truncated: false,
    }))).toMatchObject({ type: "bridge/runtime_session", sessionId: "session" });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/runtime_replay_complete",
      sessionIds: ["session"],
    }))).toEqual({ type: "bridge/runtime_replay_complete", sessionIds: ["session"] });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/runtime_snapshot",
      snapshot: {
        epoch: "epoch-1",
        throughSeq: 7,
        connectionRevision: 1,
        sessions: {},
        requestElicitations: {},
        requestUrlFlows: {},
      },
    }))).toMatchObject({
      type: "bridge/runtime_snapshot",
      snapshot: { epoch: "epoch-1", throughSeq: 7 },
    });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/runtime_delta",
      delta: {
        epoch: "epoch-1",
        seq: 8,
        scopeRevision: 1,
        change: { kind: "session_removed", sessionId: "session", incarnation: 1 },
      },
    }))).toMatchObject({
      type: "bridge/runtime_delta",
      delta: { epoch: "epoch-1", seq: 8 },
    });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/runtime_delta",
      delta: {
        epoch: "epoch-1",
        seq: 9,
        scopeRevision: 2,
        change: {
          kind: "turn_update_appended",
          session_id: "session",
          incarnation: 1,
          revision: 2,
          operation_id: "operation-1",
          update: { sessionUpdate: "agent_message_chunk" },
        },
      },
    }))).toMatchObject({
      type: "bridge/runtime_delta",
      delta: { seq: 9, change: { kind: "turn_update_appended" } },
    });
    expect(parseServerEvent(JSON.stringify({
      type: "acp/authenticated",
      requestId: "auth",
      methodId: "agent-login",
      response: { _meta: { account: "Agent owned" } },
    }))).toMatchObject({ type: "acp/authenticated", methodId: "agent-login" });
    expect(parseServerEvent(JSON.stringify({
      type: "acp/logged_out",
      requestId: "logout",
      response: {},
    }))).toMatchObject({ type: "acp/logged_out" });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/auth_terminal_started",
      requestId: "terminal-auth",
      methodId: "terminal-login",
    }))).toMatchObject({ type: "bridge/auth_terminal_started" });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/auth_terminal_output",
      requestId: "terminal-auth",
      data: "Enter code: ",
    }))).toMatchObject({ type: "bridge/auth_terminal_output" });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/auth_terminal_exited",
      requestId: "terminal-auth",
      methodId: "terminal-login",
      status: "succeeded",
      exitCode: 0,
    }))).toMatchObject({ type: "bridge/auth_terminal_exited", status: "succeeded" });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/error",
      requestId: "new",
      operation: "session/new",
      code: -32_000,
      message: "Authentication required",
      data: { hint: "Sign in with the Agent" },
      dataBytes: 33,
    }))).toMatchObject({
      type: "bridge/error",
      code: -32_000,
      data: { hint: "Sign in with the Agent" },
    });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/context_search_result",
      requestId: "context-search",
      query: "app",
      matches: [{
        path: "/workspace/src/app.tsx",
        name: "app.tsx",
        relativePath: "src/app.tsx",
        rootName: "workspace",
        size: 42,
      }],
    }))).toMatchObject({
      type: "bridge/context_search_result",
      matches: [{ name: "app.tsx" }],
    });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/context_search_result",
      requestId: "context-search-all",
      query: "",
      matches: [],
    }))).toMatchObject({
      type: "bridge/context_search_result",
      query: "",
      matches: [],
    });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/context_attached",
      requestId: "context-read",
      sessionId: "session",
      attachment: {
        name: "src/app.tsx",
        size: 18,
        block: {
          type: "resource",
          resource: {
            uri: "file:///workspace/src/app.tsx",
            mimeType: "text/typescript",
            text: "export default App",
          },
        },
      },
    }))).toMatchObject({ type: "bridge/context_attached" });
    expect(parseServerEvent(JSON.stringify({
      type: "acp/session_created",
      requestId: "request",
      response: { sessionId: "session" },
      earlyUpdates: [{
        sessionId: "session",
        update: {
          sessionUpdate: "available_commands_update",
          availableCommands: [],
        },
      }],
    }))).toMatchObject({ type: "acp/session_created", earlyUpdates: [{ sessionId: "session" }] });
    expect(parseServerEvent(JSON.stringify({
      type: "acp/elicitation_aborted",
      elicitationId: "external",
      sessionId: "session",
      reason: "session_closed",
    }))).toMatchObject({ type: "acp/elicitation_aborted" });
    expect(parseServerEvent(JSON.stringify({
      type: "acp/prompt_started",
      requestId: "prompt",
      sessionId: "session",
      prompt: [{ type: "text", text: "Continue" }],
    }))).toMatchObject({ type: "acp/prompt_started", sessionId: "session" });
    expect(parseServerEvent(JSON.stringify({
      type: "acp/prompt_complete",
      requestId: "prompt",
      sessionId: "session",
      response: {
        stopReason: "max_tokens",
        usage: { totalTokens: 3, inputTokens: 2, outputTokens: 1 },
      },
    }))).toMatchObject({ type: "acp/prompt_complete" });
    expect(parseServerEvent(JSON.stringify({
      type: "acp/terminal_state",
      terminal: {
        sessionId: "session",
        terminalId: "terminal",
        output: "done",
        outputBytes: "ZG9uZQ==",
        truncated: false,
        exitStatus: { exitCode: 0, signal: null },
        released: true,
      },
    }))).toMatchObject({
      type: "acp/terminal_state",
      terminal: { terminalId: "terminal", released: true },
    });
    for (const outputBytes of ["%%%", "a===", "abcd\n", 5]) {
      expect(() => parseServerEvent(JSON.stringify({
        type: "acp/terminal_state",
        terminal: {
          sessionId: "session", terminalId: "terminal", output: "", outputBytes,
          truncated: false, released: false,
        },
      }))).toThrow("outputBytes");
    }
    for (const exitCode of [-1, 0x1_0000_0000]) {
      expect(() => parseServerEvent(JSON.stringify({
        type: "acp/terminal_state",
        terminal: {
          sessionId: "session",
          terminalId: "terminal",
          output: "failed",
          truncated: false,
          exitStatus: { exitCode, signal: null },
          released: true,
        },
      }))).toThrow("uint32");
    }

    expect(() => parseServerEvent("null")).toThrow("object with a type");
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/runtime_snapshot",
      snapshot: {
        epoch: "epoch",
        throughSeq: -1,
        connectionRevision: 0,
        sessions: {},
        requestElicitations: {},
        requestUrlFlows: {},
      },
    }))).toThrow("throughSeq");
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/runtime_delta",
      delta: {
        epoch: "epoch",
        seq: 1,
        scopeRevision: null,
        change: { kind: "invented" },
      },
    }))).toThrow("change kind");
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/error",
      message: "bad code",
      code: 1.5,
    }))).toThrow("safe integer");
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/error",
      message: "bad truncation marker",
      dataTruncated: "yes",
    }))).toThrow("dataTruncated");
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/error",
      message: "bad data byte count",
      dataBytes: -1,
    }))).toThrow("non-negative");
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/error",
      message: "oversized error data",
      data: { padding: "x".repeat(256 * 1024) },
    }))).not.toThrow();
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/auth_terminal_exited",
      requestId: "terminal-auth",
      methodId: "terminal-login",
      status: "invented",
      exitCode: 0,
    }))).toThrow("status is invalid");
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/context_search_result",
      requestId: "context-search",
      matches: [],
    }))).toThrow("query must be a string");
    expect(() => parseServerEvent('{"type":"acp/invented"}')).toThrow("Unknown server");
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/runtime_session",
      sessionId: "session",
      cwd: "/workspace",
      session: { sessionId: "different" },
      truncated: false,
    }))).toThrow("sessionId mismatch");
    expect(() => parseServerEvent(JSON.stringify({
      type: "acp/session_update",
      notification: { sessionId: "session" },
    }))).toThrow("update must be an object");
    expect(() => parseServerEvent(JSON.stringify({
      type: "acp/session_created",
      requestId: "request",
      response: { sessionId: "session" },
      earlyUpdates: [{
        sessionId: "other-session",
        update: { sessionUpdate: "usage_update", used: 1, size: 2 },
      }],
    }))).toThrow("different session");
    expect(() => parseServerEvent(JSON.stringify({
      type: "acp/terminal_state",
      terminal: {
        sessionId: "session",
        terminalId: "terminal",
        output: "x",
        truncated: "no",
        released: false,
      },
    }))).toThrow("requires truncated");
    expect(() => parseServerEvent(JSON.stringify({
      type: "acp/elicitation_aborted",
      elicitationId: "external",
      sessionId: "session",
      reason: "invented",
    }))).toThrow("reason");
    expect(() => parseServerEvent(JSON.stringify({
      type: "acp/prompt_complete",
      requestId: "prompt",
      sessionId: "session",
      response: {
        stopReason: "end_turn",
        usage: { totalTokens: 1, inputTokens: 1, outputTokens: 1 },
      },
    }))).toThrow("exceeds totalTokens");
  });
});
