import { describe, expect, it } from "vitest";
import {
  MAX_BRIDGE_ERROR_DATA_BYTES,
  parseClientCommand,
  parseServerEvent,
} from "../shared/bridge";

describe("browser bridge messages", () => {
  it("accepts supported commands", () => {
    expect(parseClientCommand(JSON.stringify({
      type: "bridge/ping",
      nonce: "resume-probe",
    }))).toEqual({ type: "bridge/ping", nonce: "resume-probe" });
    expect(parseClientCommand(JSON.stringify({
      type: "auth/authenticate",
      requestId: "auth-request",
      methodId: "agent-login",
    }))).toEqual({
      type: "auth/authenticate",
      requestId: "auth-request",
      methodId: "agent-login",
    });
    expect(parseClientCommand(JSON.stringify({
      type: "auth/logout",
      requestId: "logout-request",
    }))).toEqual({ type: "auth/logout", requestId: "logout-request" });
    expect(parseClientCommand(JSON.stringify({
      type: "auth/terminal_start",
      requestId: "terminal-auth",
      methodId: "terminal-login",
      cols: 80,
      rows: 24,
    }))).toMatchObject({ type: "auth/terminal_start", cols: 80, rows: 24 });
    expect(parseClientCommand(JSON.stringify({
      type: "auth/terminal_input",
      requestId: "terminal-auth",
      data: "secret\r",
    }))).toMatchObject({ type: "auth/terminal_input", data: "secret\r" });
    expect(parseClientCommand(JSON.stringify({
      type: "auth/terminal_resize",
      requestId: "terminal-auth",
      cols: 100,
      rows: 30,
    }))).toMatchObject({ type: "auth/terminal_resize", cols: 100 });
    expect(parseClientCommand(JSON.stringify({
      type: "context/search",
      requestId: "context-search",
      query: "src app",
    }))).toEqual({
      type: "context/search",
      requestId: "context-search",
      query: "src app",
    });
    expect(parseClientCommand(JSON.stringify({
      type: "context/read",
      requestId: "context-read",
      sessionId: "session",
      path: "/workspace/src/app.tsx",
    }))).toMatchObject({ type: "context/read", sessionId: "session" });
    expect(
      parseClientCommand(
        JSON.stringify({ type: "session/new", requestId: "request-1", cwd: "/workspace" }),
      ),
    ).toEqual({ type: "session/new", requestId: "request-1", cwd: "/workspace" });
    expect(
      parseClientCommand(
        JSON.stringify({ type: "session/load", requestId: "request-2", sessionId: "saved" }),
      ),
    ).toEqual({ type: "session/load", requestId: "request-2", sessionId: "saved" });
    expect(
      parseClientCommand(
        JSON.stringify({ type: "session/list", requestId: "request-3", cursor: "next" }),
      ),
    ).toEqual({ type: "session/list", requestId: "request-3", cursor: "next" });
    expect(
      parseClientCommand(
        JSON.stringify({ type: "session/fork", requestId: "request-4", sessionId: "active" }),
      ),
    ).toEqual({ type: "session/fork", requestId: "request-4", sessionId: "active" });
    expect(
      parseClientCommand(JSON.stringify({
        type: "nes/suggest",
        requestId: "request-5",
        sessionId: "nes",
        uri: "file:///workspace/file.ts",
        position: { line: 1, character: 2 },
        triggerKind: "manual",
      })),
    ).toMatchObject({ type: "nes/suggest", sessionId: "nes" });
  });

  it("rejects unknown and malformed commands at the trust boundary", () => {
    expect(() => parseClientCommand('{"type":"process/spawn"}')).toThrow("Unknown");
    expect(() => parseClientCommand(JSON.stringify({
      type: "x".repeat(129),
    }))).toThrow("command type is too long");
    expect(() => parseClientCommand(JSON.stringify({
      type: "session/new",
      requestId: "x".repeat(1_025),
    }))).toThrow("requestId exceeds 1024 characters");
    expect(() => parseClientCommand(JSON.stringify({
      type: "session/new",
      requestId: "new-relative",
      cwd: "relative/project",
    }))).toThrow("must be an absolute path");
    expect(() => parseClientCommand(JSON.stringify({
      type: "auth/authenticate",
      requestId: "auth",
      methodId: "x".repeat(1_025),
    }))).toThrow("methodId exceeds 1024 characters");
    expect(() => parseClientCommand(JSON.stringify({
      type: "auth/terminal_start",
      requestId: "auth",
      methodId: "terminal",
      cols: 1,
      rows: 24,
    }))).toThrow("cols must be an integer between 2 and 500");
    expect(() => parseClientCommand(JSON.stringify({
      type: "auth/terminal_input",
      requestId: "auth",
      data: "x".repeat(65_537),
    }))).toThrow("data exceeds 65536 characters");
    expect(() => parseClientCommand(JSON.stringify({
      type: "session/cancel",
      sessionId: "x".repeat(1_025),
    }))).toThrow("sessionId exceeds 1024 characters");
    expect(() => parseClientCommand(JSON.stringify({
      type: "session/list",
      requestId: "x",
      cursor: "x".repeat(4_097),
    }))).toThrow("cursor exceeds 4096 characters");
    expect(() => parseClientCommand(JSON.stringify({
      type: "context/search",
      requestId: "x",
      query: "x".repeat(257),
    }))).toThrow("query exceeds 256 characters");
    expect(() => parseClientCommand(JSON.stringify({
      type: "context/read",
      requestId: "x",
      sessionId: "s",
      path: "x".repeat(16_385),
    }))).toThrow("path exceeds 16384 characters");
    expect(() => parseClientCommand('{"type":"session/prompt","requestId":"x"}')).toThrow("sessionId");
    expect(() => parseClientCommand('{"type":"permission/respond","requestId":"response","permissionId":"x","outcome":{"outcome":"selected"}}')).toThrow("optionId");
    expect(() => parseClientCommand(JSON.stringify({
      type: "session/prompt",
      requestId: "x",
      sessionId: "s",
      prompt: [{ type: "image", data: 42, mimeType: "image/png" }],
    }))).toThrow("data must be a string");
    expect(() => parseClientCommand(JSON.stringify({
      type: "session/prompt",
      requestId: "x",
      sessionId: "s",
      prompt: [{ type: "image", data: "AA==", mimeType: "text/html" }],
    }))).toThrow("image/* family");
    expect(() => parseClientCommand(JSON.stringify({
      type: "session/prompt",
      requestId: "x",
      sessionId: "s",
      prompt: [{ type: "resource_link", name: "relative", uri: "./secret" }],
    }))).toThrow("resource URI is invalid");
    expect(() => parseClientCommand(JSON.stringify({
      type: "session/prompt",
      requestId: "x",
      sessionId: "s",
      prompt: [{ type: "resource", resource: { uri: "x", text: "a", blob: "b" } }],
    }))).toThrow("exactly one");
    expect(() => parseClientCommand(JSON.stringify({
      type: "session/prompt",
      requestId: "x",
      sessionId: "s",
      prompt: [{ type: "resource", resource: { uri: "urn:test", text: "ok", blob: 42 } }],
    }))).toThrow("exactly one");
    expect(() => parseClientCommand(JSON.stringify({
      type: "session/prompt",
      requestId: "x",
      sessionId: "s",
      prompt: [{
        type: "resource_link",
        name: "bad metadata",
        uri: "urn:test",
        title: 42,
      }],
    }))).toThrow("title must be a string");
    expect(() => parseClientCommand(JSON.stringify({
      type: "elicitation/respond",
      requestId: "response",
      elicitationId: "x",
      response: { action: "accept", content: { bad: { nested: true } } },
    }))).toThrow("invalid content value");
    expect(() => parseClientCommand(JSON.stringify({
      type: "elicitation/respond",
      requestId: "response",
      elicitationId: "x",
      response: { action: "invented" },
    }))).toThrow("Unsupported elicitation response action");
    expect(() => parseClientCommand(JSON.stringify({
      type: "elicitation/respond",
      requestId: "response",
      elicitationId: "x",
      response: { action: "decline", content: { leaked: "value" } },
    }))).toThrow("Only accepted elicitation responses");
    expect(() => parseClientCommand(JSON.stringify({
      type: "document/focus",
      sessionId: "nes",
      uri: "file:///workspace/file.ts",
      position: { line: -1, character: 0 },
      visibleRange: {
        start: { line: 0, character: 0 },
        end: { line: 1, character: 0 },
      },
    }))).toThrow("non-negative");
    expect(() => parseClientCommand(JSON.stringify({
      type: "nes/reject",
      requestId: "x",
      sessionId: "nes",
      suggestionId: "suggestion",
      reason: "invented",
    }))).toThrow("reason");
  });

  it("validates server event envelopes before they reach the React reducer", () => {
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/pong",
      nonce: "resume-probe",
    }))).toEqual({ type: "bridge/pong", nonce: "resume-probe" });
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
        intentResults: {},
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
        intentResults: [],
        evictedIntentResultIds: [],
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
        intentResults: [],
        evictedIntentResultIds: [],
      },
    }))).toMatchObject({
      type: "bridge/runtime_delta",
      delta: { seq: 9, change: { kind: "turn_update_appended" } },
    });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/intent_ack",
      requestId: "prompt-1",
      operationId: "operation-1",
      disposition: "duplicate",
      status: "agent_acknowledged",
    }))).toEqual({
      type: "bridge/intent_ack",
      requestId: "prompt-1",
      operationId: "operation-1",
      disposition: "duplicate",
      status: "agent_acknowledged",
    });
    expect(parseServerEvent(JSON.stringify({
      type: "bridge/intent_ack",
      requestId: "prompt-2",
      operationId: "operation-2",
      disposition: "accepted",
      status: "accepted",
    }))).toMatchObject({
      disposition: "accepted",
      status: "accepted",
    });
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/intent_ack",
      requestId: "prompt-1",
      operationId: "operation-1",
      disposition: "duplicate",
      status: "completed",
    }))).toThrow("status");
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
        truncated: false,
        exitStatus: { exitCode: 0, signal: null },
        released: true,
      },
    }))).toMatchObject({
      type: "acp/terminal_state",
      terminal: { terminalId: "terminal", released: true },
    });
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
    expect(parseServerEvent(JSON.stringify({
      type: "acp/document_opened",
      requestId: "request",
      document: {
        sessionId: "nes",
        path: "/workspace/empty.ts",
        uri: "file:///workspace/empty.ts",
        languageId: "typescript",
        version: 1,
        text: "",
      },
    }))).toMatchObject({ type: "acp/document_opened" });
    expect(parseServerEvent(JSON.stringify({
      type: "acp/nes_suggestions",
      requestId: "request",
      sessionId: "nes",
      uri: "file:///workspace/empty.ts",
      response: {
        suggestions: [{
          kind: "edit",
          id: "suggestion",
          uri: "file:///workspace/empty.ts",
          edits: [{
            range: {
              start: { line: 0, character: 0 },
              end: { line: 0, character: 0 },
            },
            newText: "hello",
          }],
        }],
      },
    }))).toMatchObject({
      type: "acp/nes_suggestions",
      response: { suggestions: [{ id: "suggestion" }] },
    });

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
        intentResults: {},
      },
    }))).toThrow("throughSeq");
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/runtime_delta",
      delta: {
        epoch: "epoch",
        seq: 1,
        scopeRevision: null,
        change: { kind: "invented" },
        intentResults: [],
        evictedIntentResultIds: [],
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
      data: { padding: "x".repeat(MAX_BRIDGE_ERROR_DATA_BYTES) },
    }))).toThrow("relay limit");
    expect(() => parseServerEvent(JSON.stringify({
      type: "bridge/auth_terminal_exited",
      requestId: "terminal-auth",
      methodId: "terminal-login",
      status: "invented",
      exitCode: 0,
    }))).toThrow("status is invalid");
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
    expect(() => parseServerEvent(JSON.stringify({
      type: "acp/document_changed",
      requestId: "request",
      document: {
        sessionId: "nes",
        path: "/workspace/file.ts",
        uri: "file:///workspace/file.ts",
        languageId: "typescript",
        version: -1,
        text: "bad",
      },
    }))).toThrow("non-negative version");
    expect(() => parseServerEvent(JSON.stringify({
      type: "acp/nes_started",
      requestId: "request",
      response: { sessionId: "x".repeat(1_025) },
    }))).toThrow("sessionId exceeds 1024 characters");
    expect(() => parseServerEvent(JSON.stringify({
      type: "acp/nes_suggestions",
      requestId: "request",
      sessionId: "nes",
      uri: "file:///workspace/file.ts",
      response: {
        suggestions: [{
          kind: "edit",
          id: "",
          uri: "file:///workspace/file.ts",
          edits: [],
        }],
      },
    }))).toThrow("suggestion 0 id is required");
    expect(() => parseServerEvent(JSON.stringify({
      type: "acp/nes_suggestions",
      requestId: "request",
      sessionId: "nes",
      uri: "file:///workspace/file.ts",
      response: {
        suggestions: [{
          kind: "jump",
          id: "jump",
          uri: "file:///workspace/file.ts",
          position: { line: -1, character: 0 },
        }],
      },
    }))).toThrow("non-negative integer coordinates");
  });
});
