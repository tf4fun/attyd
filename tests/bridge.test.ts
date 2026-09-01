import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { WebSocket } from "ws";
import { describe, expect, it } from "vitest";
import { AcpBridge } from "../server/acp-bridge";
import type { ClientCommand, ServerEvent } from "../shared/bridge";
import { appReducer, initialState } from "../src/lib/state";

class TestSocket {
  readonly OPEN = 1;
  readonly readyState = 1;
  readonly events: ServerEvent[] = [];

  send(data: string): void {
    this.events.push(JSON.parse(data) as ServerEvent);
  }
}

describe("ACP process bridge", () => {
  it("answers browser liveness probes without depending on Agent readiness", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      bridge.receive(JSON.stringify({ type: "bridge/ping", nonce: "mobile-resume" }));
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(socket.events).toContainEqual({
        type: "bridge/pong",
        nonce: "mobile-resume",
      });
    } finally {
      bridge.close();
    }
  });

  it("rejects an oversized browser event before launching the Agent", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: ["x".repeat(5 * 1024 * 1024)],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await expect(bridge.start()).rejects.toThrow(
        "Browser bridge event bridge/hello exceeds 5242880 bytes",
      );
      expect(socket.events).toEqual([
        {
          type: "bridge/error",
          message: "Browser bridge event bridge/hello exceeds 5242880 bytes",
        },
        { type: "bridge/phase", phase: "error" },
      ]);
    } finally {
      bridge.close();
    }
  });

  it("rejects an oversized initialize response before advertising readiness", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--oversized-initialize",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await expect(bridge.start()).rejects.toThrow(
        "Agent initialize response exceeds 4000000 browser relay bytes",
      );
      expect(socket.events.some((event) => event.type === "acp/initialized")).toBe(false);
      expect(socket.events.some((event) =>
        event.type === "bridge/phase" && event.phase === "ready"
      )).toBe(false);
      expect(socket.events).toContainEqual({
        type: "bridge/error",
        message: "Agent initialize response exceeds 4000000 browser relay bytes",
      });
      expect(socket.events.at(-1)).toEqual({ type: "bridge/phase", phase: "error" });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("bounds a hostile startup error before reporting the terminal phase", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--oversized-initialize-error",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await expect(bridge.start()).rejects.toMatchObject({ code: -32_000 });
      const error = socket.events.find((event) => event.type === "bridge/error");
      expect(error?.type === "bridge/error" ? error.message : "").toMatch(
        /^ACP error -32000: x+…$/,
      );
      expect(error?.type === "bridge/error" ? error.message.length : 0)
        .toBeLessThanOrEqual(16_385);
      expect(socket.events.at(-1)).toEqual({ type: "bridge/phase", phase: "error" });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects mismatched ACP MCP declarations before starting an Agent", () => {
    const socket = new TestSocket();
    const base = {
      command: [process.execPath] as [string],
      cwd: process.cwd(),
      readOnly: false,
    };
    expect(() => new AcpBridge(socket as unknown as WebSocket, {
      ...base,
      mcpServers: [{ type: "acp", name: "tools", serverId: "tools" }],
    })).toThrow("has no client-side provider");
    expect(() => new AcpBridge(socket as unknown as WebSocket, {
      ...base,
      acpMcpProviders: [{
        name: "tools",
        serverId: "tools",
        command: process.execPath,
        args: [],
        env: [],
      }],
    })).toThrow("was not declared");
  });

  it("passes static session setup without exposing MCP secrets to the browser", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
      additionalDirectories: [join(process.cwd(), "tests")],
      mcpServers: [
        {
          name: "local",
          command: process.execPath,
          args: [],
          env: [{ name: "TOKEN", value: "stdio-secret" }],
        },
        {
          type: "http",
          name: "remote",
          url: "https://example.test/mcp",
          headers: [{ name: "Authorization", value: "http-secret" }],
        },
      ],
    });

    try {
      await bridge.start();
      const hello = socket.events.find((item) => item.type === "bridge/hello");
      expect(hello).toMatchObject({
        type: "bridge/hello",
        additionalDirectories: [join(process.cwd(), "tests")],
        mcpServers: [
          { name: "local", type: "stdio" },
          { name: "remote", type: "http" },
        ],
      });
      expect(JSON.stringify(hello)).not.toContain("secret");

      bridge.receive(command({ type: "session/new", requestId: "configured-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      expect(created.response._meta).toMatchObject({
        receivedSessionSetup: {
          additionalDirectories: [join(process.cwd(), "tests")],
          mcpServers: [
            expect.objectContaining({ name: "local" }),
            expect.objectContaining({ name: "remote", type: "http" }),
          ],
        },
      });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("searches workspace @ context and relays the selected file as an ACP resource", async () => {
    const workspace = process.cwd();
    const contextPath = join(workspace, "tests/fixtures/workspace-context-note.md");
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
      ],
      cwd: workspace,
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "context-new" }));
      const created = await waitForMatching(socket, (item) =>
        item.type === "acp/session_created" && item.requestId === "context-new",
      );
      if (created.type !== "acp/session_created") throw new Error("unreachable");

      bridge.receive(command({
        type: "context/search",
        requestId: "context-search",
        query: "workspace context note",
      }));
      const searched = await waitForMatching(socket, (item) =>
        item.type === "bridge/context_search_result" && item.requestId === "context-search",
      );
      expect(searched).toMatchObject({
        type: "bridge/context_search_result",
        matches: [{ name: "workspace-context-note.md", path: contextPath }],
      });

      bridge.receive(command({
        type: "context/read",
        requestId: "context-read",
        sessionId: created.response.sessionId,
        path: contextPath,
      }));
      const attached = await waitForMatching(socket, (item) =>
        item.type === "bridge/context_attached" && item.requestId === "context-read",
      );
      if (attached.type !== "bridge/context_attached") throw new Error("unreachable");
      expect(attached.attachment).toMatchObject({
        name: "tests/fixtures/workspace-context-note.md",
        block: {
          type: "resource",
          resource: {
            text: expect.stringContaining("# Workspace context"),
            mimeType: "text/markdown",
          },
        },
      });

      bridge.receive(command({
        type: "session/prompt",
        requestId: "context-prompt",
        sessionId: created.response.sessionId,
        prompt: [
          { type: "text", text: "attachment-input-flow" },
          attached.attachment.block,
        ],
      }));
      const response = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "attachment-input-result",
      );
      if (
        response.type !== "acp/session_update" ||
        response.notification.update.sessionUpdate !== "agent_message_chunk" ||
        response.notification.update.content.type !== "text"
      ) throw new Error("Missing context prompt result");
      expect(response.notification.update.content.text)
        .toBe("Received prompt blocks: text,resource.");
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("provides ACP-transport MCP bidirectionally without exposing provider launch secrets", async () => {
    const socket = new TestSocket();
    const fixtureCommand = [
      "--import",
      "tsx",
      join(process.cwd(), "tests/fixtures/fake-mcp-server.ts"),
    ];
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
      mcpServers: [{ type: "acp", name: "client-tools", serverId: "client-tools" }],
      acpMcpProviders: [{
        name: "client-tools",
        serverId: "client-tools",
        command: process.execPath,
        args: fixtureCommand,
        env: [{ name: "MCP_PROVIDER_SECRET", value: "must-stay-server-side" }],
      }],
    });

    try {
      await bridge.start();
      const hello = socket.events.find((item) => item.type === "bridge/hello");
      expect(hello).toMatchObject({
        type: "bridge/hello",
        mcpServers: [{ name: "client-tools", type: "acp" }],
      });
      expect(JSON.stringify(hello)).not.toContain("MCP_PROVIDER_SECRET");
      expect(JSON.stringify(hello)).not.toContain("must-stay-server-side");
      expect(JSON.stringify(hello)).not.toContain("fake-mcp-server");

      bridge.receive(command({ type: "session/new", requestId: "mcp-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      expect(created.response._meta).toMatchObject({
        receivedSessionSetup: {
          mcpServers: [{ type: "acp", name: "client-tools", serverId: "client-tools" }],
        },
      });
      expect(JSON.stringify(created.response)).not.toContain("MCP_PROVIDER_SECRET");

      bridge.receive(command({
        type: "session/prompt",
        requestId: "mcp-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "mcp-flow" }],
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/mcp_message" &&
        item.direction === "server-to-agent" &&
        item.method === "roots/list",
      );
      await waitFor(socket, "acp/prompt_complete");

      const connections = socket.events.filter((item) => item.type === "acp/mcp_connection");
      expect(connections.map(({ action }) => action)).toEqual(["connected", "disconnected"]);
      const messages = socket.events.filter((item) => item.type === "acp/mcp_message");
      expect(messages).toEqual(expect.arrayContaining([
        expect.objectContaining({ direction: "agent-to-server", method: "initialize" }),
        expect.objectContaining({ direction: "agent-to-server", method: "echo" }),
        expect.objectContaining({ direction: "server-to-agent", method: "notifications/progress" }),
        expect.objectContaining({ direction: "server-to-agent", method: "roots/list" }),
      ]));
      const resultUpdate = socket.events.find((item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "mcp-result",
      );
      if (
        resultUpdate?.type !== "acp/session_update" ||
        resultUpdate.notification.update.sessionUpdate !== "agent_message_chunk" ||
        resultUpdate.notification.update.content.type !== "text"
      ) throw new Error("Missing MCP result update");
      expect(JSON.parse(resultUpdate.notification.update.content.text)).toMatchObject({
        echoed: { from: "agent" },
        roundTrip: {
          clientResult: {
            roots: [{ uri: "file:///fake-agent-workspace" }],
            receivedMethod: "roots/list",
          },
        },
      });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("fails fast when configured session roots are not supported by the Agent", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts"), "--minimal"],
      cwd: process.cwd(),
      readOnly: false,
      additionalDirectories: [join(process.cwd(), "tests")],
    });
    try {
      await expect(bridge.start()).rejects.toThrow("did not advertise");
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("fails fast when the Agent does not negotiate ACP-transport MCP", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--minimal",
      ],
      cwd: process.cwd(),
      readOnly: false,
      mcpServers: [{ type: "acp", name: "client-tools", serverId: "client-tools" }],
      acpMcpProviders: [{
        name: "client-tools",
        serverId: "client-tools",
        command: process.execPath,
        args: ["--import", "tsx", join(process.cwd(), "tests/fixtures/fake-mcp-server.ts")],
        env: [],
      }],
    });
    try {
      await expect(bridge.start()).rejects.toThrow("mcpCapabilities.acp");
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects browser commands that target unknown sessions or unoffered controls", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });
    try {
      await bridge.start();
      bridge.receive(command({
        type: "session/prompt",
        requestId: "unknown-prompt",
        sessionId: "invented",
        prompt: [{ type: "text", text: "test" }],
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.requestId === "unknown-prompt" && item.message.includes("Unknown"),
      );

      bridge.receive(command({ type: "session/new", requestId: "new-controls" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/set_mode",
        requestId: "bad-mode",
        sessionId: created.response.sessionId,
        modeId: "invented",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.requestId === "bad-mode" && item.message.includes("not offered"),
      );
      bridge.receive(command({
        type: "session/set_config_option",
        requestId: "bad-config",
        sessionId: created.response.sessionId,
        configId: "verbose",
        value: "yes",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.requestId === "bad-config" && item.message.includes("boolean"),
      );
      bridge.receive(command({
        type: "session/set_config_option",
        requestId: "valid-config",
        sessionId: created.response.sessionId,
        configId: "verbose",
        value: true,
      }));
      const changed = await waitForMatching(socket, (item) =>
        item.type === "acp/config_changed" && item.requestId === "valid-config",
      );
      if (changed.type !== "acp/config_changed") throw new Error("unreachable");
      expect(changed.response.configOptions).toEqual([
        expect.objectContaining({ id: "verbose", currentValue: true }),
      ]);
      bridge.receive(command({ type: "session/new", requestId: "duplicate-new" }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "duplicate-new" &&
        item.message.includes("duplicate active session ID"),
      );
      bridge.receive(command({
        type: "session/set_config_option",
        requestId: "config-after-duplicate-new",
        sessionId: created.response.sessionId,
        configId: "verbose",
        value: false,
      }));
      const afterDuplicate = await waitForMatching(socket, (item) =>
        item.type === "acp/config_changed" &&
        item.requestId === "config-after-duplicate-new"
      );
      expect(afterDuplicate.type === "acp/config_changed"
        ? afterDuplicate.response._meta
        : undefined).toMatchObject({ observedSessionCloses: [] });
      bridge.receive(command({
        type: "session/list",
        requestId: "invented-cursor",
        cursor: "not-offered-by-agent",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "invented-cursor" &&
        item.message.includes("cursor was not offered"),
      );
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects malformed browser messages without poisoning the ACP connection", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive("not-json");
      const error = await waitFor(socket, "bridge/error");
      if (error.type !== "bridge/error") throw new Error("unreachable");
      expect(error.message).toMatch(/JSON|Unexpected token/);
      expect(socket.events.filter((item) => item.type === "bridge/phase").at(-1)).toEqual({
        type: "bridge/phase",
        phase: "ready",
      });

      bridge.receive(JSON.stringify({
        type: "session/new",
        requestId: "x".repeat(1_025),
      }));
      const bounded = await waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.message.includes("requestId exceeds 1024"),
      );
      expect(JSON.stringify(bounded).length).toBeLessThan(256);

      bridge.receive(command({ type: "session/new", requestId: "after-invalid" }));
      await waitFor(socket, "acp/session_created");
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("relays URL elicitation completion separately from user consent", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "new-url" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "prompt-url",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "url-flow" }],
      }));
      const elicitation = await waitFor(socket, "acp/elicitation_request");
      if (elicitation.type !== "acp/elicitation_request") throw new Error("unreachable");
      bridge.receive(JSON.stringify({
        type: "elicitation/respond",
        requestId: "invalid-url-response",
        elicitationId: elicitation.elicitationId,
        response: { action: "invented" },
      }));
      const invalidResponse = await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.message.includes("Unsupported elicitation response action"),
      );
      expect(invalidResponse).toMatchObject({
        type: "bridge/error",
      });
      expect(socket.events.some((item) =>
        item.type === "acp/elicitation_resolved" &&
        item.elicitationId === elicitation.elicitationId,
      )).toBe(false);
      bridge.receive(command({
        type: "elicitation/respond",
        requestId: "accept-url-response",
        elicitationId: elicitation.elicitationId,
        response: { action: "accept" },
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/elicitation_resolved" &&
        item.requestId === "accept-url-response",
      );
      const complete = await waitFor(socket, "acp/elicitation_complete");
      if (complete.type !== "acp/elicitation_complete") throw new Error("unreachable");
      expect(complete.notification.elicitationId).toBe("test-external-flow");
      await waitFor(socket, "acp/prompt_complete");
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("aborts an accepted session URL flow when its session closes", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "pending-url-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      const sessionId = created.response.sessionId;
      bridge.receive(command({
        type: "session/prompt",
        requestId: "pending-url-prompt",
        sessionId,
        prompt: [{ type: "text", text: "pending-url-flow" }],
      }));
      const elicitation = await waitFor(socket, "acp/elicitation_request");
      if (elicitation.type !== "acp/elicitation_request") throw new Error("unreachable");
      bridge.receive(command({
        type: "elicitation/respond",
        requestId: "accept-pending-url-response",
        elicitationId: elicitation.elicitationId,
        response: { action: "accept" },
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/elicitation_resolved" &&
        item.elicitationId === elicitation.elicitationId,
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "pending-url-prompt",
      );

      bridge.receive(command({ type: "session/close", requestId: "pending-url-close", sessionId }));
      const aborted = await waitForMatching(socket, (item) =>
        item.type === "acp/elicitation_aborted" &&
        item.elicitationId === "pending-external-flow",
      );
      expect(aborted).toMatchObject({
        type: "acp/elicitation_aborted",
        sessionId,
        reason: "session_closed",
      });
      await waitForMatching(socket, (item) =>
        item.type === "acp/session_closed" && item.requestId === "pending-url-close",
      );
      expect(socket.events.some((item) =>
        item.type === "acp/elicitation_complete" &&
        item.notification.elicitationId === "pending-external-flow"
      )).toBe(false);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("supports request-scoped elicitation without inventing a session binding", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "request-scope-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      const cases = [
        {
          prompt: "request-scoped-form-flow",
          requestId: "request-scope-prompt",
          resultMessageId: "request-scoped-result",
          expectedScope: undefined,
        },
        {
          prompt: "null-request-scope-flow",
          requestId: "request-scope-null-prompt",
          resultMessageId: "request-scoped-null-result",
          expectedScope: null,
        },
        {
          prompt: "empty-request-scope-flow",
          requestId: "request-scope-empty-prompt",
          resultMessageId: "request-scoped-empty-result",
          expectedScope: "",
        },
      ] as const;
      for (const testCase of cases) {
        bridge.receive(command({
          type: "session/prompt",
          requestId: testCase.requestId,
          sessionId: created.response.sessionId,
          prompt: [{ type: "text", text: testCase.prompt }],
        }));
        const elicitation = await waitForMatching(socket, (item) =>
          item.type === "acp/elicitation_request" &&
          "requestId" in item.request &&
          (testCase.expectedScope === undefined ||
            item.request.requestId === testCase.expectedScope),
        );
        if (elicitation.type !== "acp/elicitation_request") throw new Error("unreachable");
        expect("requestId" in elicitation.request).toBe(true);
        expect("sessionId" in elicitation.request).toBe(false);
        if (testCase.expectedScope !== undefined) {
          expect(elicitation.request.requestId).toBe(testCase.expectedScope);
        }
        bridge.receive(command({
          type: "elicitation/respond",
          requestId: `decline-request-scope-${testCase.requestId}`,
          elicitationId: elicitation.elicitationId,
          response: { action: "decline" },
        }));
        const result = await waitForMatching(socket, (item) =>
          item.type === "acp/session_update" &&
          item.notification.update.sessionUpdate === "agent_message_chunk" &&
          item.notification.update.messageId === testCase.resultMessageId,
        );
        if (
          result.type !== "acp/session_update" ||
          result.notification.update.sessionUpdate !== "agent_message_chunk" ||
          result.notification.update.content.type !== "text"
        ) throw new Error("Missing request-scoped result");
        expect(result.notification.update.content.text).toBe("Request-scoped decline.");
        await waitForMatching(socket, (item) =>
          item.type === "acp/prompt_complete" && item.requestId === testCase.requestId,
        );
      }
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("resolves pending interaction cards when the browser cancels a session turn", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "cancel-interactions-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "cancel-interactions-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "cancel-pending-interactions-flow" }],
      }));
      const permission = await waitFor(socket, "acp/permission_request");
      const elicitation = await waitFor(socket, "acp/elicitation_request");
      if (
        permission.type !== "acp/permission_request" ||
        elicitation.type !== "acp/elicitation_request"
      ) throw new Error("Missing pending interactions");

      bridge.receive(command({
        type: "session/cancel",
        sessionId: created.response.sessionId,
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/permission_resolved" &&
        item.permissionId === permission.permissionId,
      );
      const elicitationResolved = await waitForMatching(socket, (item) =>
        item.type === "acp/elicitation_resolved" &&
        item.elicitationId === elicitation.elicitationId,
      );
      if (elicitationResolved.type !== "acp/elicitation_resolved") {
        throw new Error("Missing elicitation cancellation");
      }
      expect(elicitationResolved.response).toEqual({ action: "cancel" });

      const result = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "cancelled-interactions-result",
      );
      if (
        result.type !== "acp/session_update" ||
        result.notification.update.sessionUpdate !== "agent_message_chunk" ||
        result.notification.update.content.type !== "text"
      ) throw new Error("Missing cancellation result");
      expect(result.notification.update.content.text)
        .toBe("Cancelled interactions: cancelled/cancel.");
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" &&
        item.requestId === "cancel-interactions-prompt",
      );
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("removes pending interaction cards when the Agent cancels their RPC requests", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "agent-cancel-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "agent-cancel-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "agent-cancel-interactions-flow" }],
      }));

      const permission = await waitFor(socket, "acp/permission_request");
      if (permission.type !== "acp/permission_request") throw new Error("unreachable");
      await waitForMatching(socket, (item) =>
        item.type === "acp/permission_resolved" &&
        item.permissionId === permission.permissionId,
      );
      const elicitation = await waitFor(socket, "acp/elicitation_request");
      if (elicitation.type !== "acp/elicitation_request") throw new Error("unreachable");
      const resolved = await waitForMatching(socket, (item) =>
        item.type === "acp/elicitation_resolved" &&
        item.elicitationId === elicitation.elicitationId,
      );
      if (resolved.type !== "acp/elicitation_resolved") throw new Error("unreachable");
      expect(resolved.response).toEqual({ action: "cancel" });

      await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "agent-cancelled-interactions-result",
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "agent-cancel-prompt",
      );
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects optional methods the agent did not advertise", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts"), "--minimal"],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/list", requestId: "unsupported-list" }));
      const error = await waitFor(socket, "bridge/error");
      if (error.type !== "bridge/error") throw new Error("unreachable");
      expect(error).toMatchObject({
        requestId: "unsupported-list",
        message: "Agent did not advertise session/list",
      });
      bridge.receive(command({
        type: "context/search",
        requestId: "unsupported-context",
        query: "app",
      }));
      await expect(waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.requestId === "unsupported-context",
      )).resolves.toMatchObject({
        operation: "context/search",
        message: "Agent did not advertise context/search",
      });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("gates and relays the agent-owned session lifecycle", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/list", requestId: "list-1" }));
      const listed = await waitFor(socket, "acp/sessions_listed");
      if (listed.type !== "acp/sessions_listed") throw new Error("unreachable");
      expect(listed.response.sessions[0]).toMatchObject({
        sessionId: "saved-session",
        title: "Saved ACP session",
      });

      bridge.receive(command({ type: "session/load", requestId: "load-1", sessionId: "saved-session" }));
      const attached = await waitFor(socket, "acp/session_attached");
      if (attached.type !== "acp/session_attached") throw new Error("unreachable");
      expect(attached).toMatchObject({ method: "load", sessionId: "saved-session" });
      expect(socket.events).toContainEqual(expect.objectContaining({
        type: "acp/session_update",
        notification: expect.objectContaining({ sessionId: "saved-session" }),
      }));

      bridge.receive(command({ type: "session/fork", requestId: "fork-1", sessionId: "saved-session" }));
      const forked = await waitFor(socket, "acp/session_forked");
      if (forked.type !== "acp/session_forked") throw new Error("unreachable");
      expect(forked).toMatchObject({
        sourceSessionId: "saved-session",
        response: {
          sessionId: "forked-session",
          _meta: { forkedFrom: "saved-session" },
        },
      });

      bridge.receive(command({ type: "session/close", requestId: "close-1", sessionId: "saved-session" }));
      await waitFor(socket, "acp/session_closed");
      bridge.receive(command({ type: "session/delete", requestId: "delete-1", sessionId: "saved-session" }));
      await waitFor(socket, "acp/session_deleted");
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("negotiates Agent-owned authentication and capability-gated logout", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--auth-required",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      const initialized = socket.events.find((item) => item.type === "acp/initialized");
      expect(initialized?.type === "acp/initialized"
        ? initialized.response.authMethods
        : undefined).toEqual([
        expect.objectContaining({ id: "agent-login", name: "Continue with Fake Agent" }),
      ]);

      bridge.receive(command({ type: "session/list", requestId: "list-before-auth" }));
      await expect(waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.requestId === "list-before-auth"
      )).resolves.toMatchObject({
        operation: "session/list",
        code: -32_000,
        message: expect.stringContaining("Authentication required"),
      });

      bridge.receive(command({
        type: "auth/authenticate",
        requestId: "unoffered-auth",
        methodId: "invented",
      }));
      await expect(waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.requestId === "unoffered-auth"
      )).resolves.toMatchObject({
        operation: "auth/authenticate",
        message: "Authentication method was not offered by the Agent",
      });

      bridge.receive(command({
        type: "auth/authenticate",
        requestId: "agent-auth",
        methodId: "agent-login",
      }));
      await expect(waitForMatching(socket, (item) =>
        item.type === "acp/authenticated" && item.requestId === "agent-auth"
      )).resolves.toMatchObject({
        methodId: "agent-login",
        response: { _meta: { authenticatedBy: "agent-login" } },
      });

      bridge.receive(command({ type: "session/list", requestId: "list-after-auth" }));
      await expect(waitForMatching(socket, (item) =>
        item.type === "acp/sessions_listed" && item.requestId === "list-after-auth"
      )).resolves.toMatchObject({ response: { sessions: expect.any(Array) } });

      bridge.receive(command({ type: "auth/logout", requestId: "agent-logout" }));
      await expect(waitForMatching(socket, (item) =>
        item.type === "acp/logged_out" && item.requestId === "agent-logout"
      )).resolves.toMatchObject({ response: { _meta: { signedOut: true } } });

      bridge.receive(command({ type: "session/list", requestId: "list-after-logout" }));
      await expect(waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.requestId === "list-after-logout"
      )).resolves.toMatchObject({ code: -32_000 });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects an invalid Agent terminal-auth launch environment", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--invalid-terminal-auth",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });
    try {
      await expect(bridge.start()).rejects.toThrow(
        "invalid environment variable name",
      );
      expect(socket.events.some((item) => item.type === "acp/initialized")).toBe(false);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("runs terminal authentication in a separate PTY and succeeds after reconnect", async () => {
    const root = await mkdtemp(join(tmpdir(), "attyd-terminal-auth-"));
    const authFile = join(root, "authenticated");
    const options = {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--terminal-auth-required",
      ] as [string, ...string[]],
      cwd: process.cwd(),
      readOnly: false,
      env: { ...process.env, ATTYD_FAKE_AUTH_FILE: authFile },
    };
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, options);
    let reconnected: AcpBridge | undefined;
    try {
      await bridge.start();
      const initialized = socket.events.find((item) => item.type === "acp/initialized");
      expect(initialized?.type === "acp/initialized"
        ? initialized.response.authMethods
        : undefined).toEqual([
        expect.objectContaining({
          id: "terminal-login",
          type: "terminal",
          args: ["--terminal-login"],
        }),
      ]);

      bridge.receive(command({
        type: "auth/authenticate",
        requestId: "wrong-auth-route",
        methodId: "terminal-login",
      }));
      await expect(waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.requestId === "wrong-auth-route"
      )).resolves.toMatchObject({
        message: expect.stringContaining("auth/terminal_start"),
      });

      bridge.receive(command({
        type: "auth/terminal_start",
        requestId: "terminal-auth",
        methodId: "terminal-login",
        cols: 90,
        rows: 25,
      }));
      await expect(waitForMatching(socket, (item) =>
        item.type === "bridge/auth_terminal_started" && item.requestId === "terminal-auth"
      )).resolves.toMatchObject({ methodId: "terminal-login" });
      await expect(waitForMatching(socket, (item) =>
        item.type === "bridge/auth_terminal_output" && item.data.includes("Enter access code")
      )).resolves.toMatchObject({ requestId: "terminal-auth" });

      bridge.receive(command({
        type: "auth/terminal_resize",
        requestId: "terminal-auth",
        cols: 100,
        rows: 30,
      }));
      bridge.receive(command({
        type: "auth/terminal_input",
        requestId: "terminal-auth",
        data: "open-sesame\r",
      }));
      await expect(waitForMatching(socket, (item) =>
        item.type === "bridge/auth_terminal_exited" && item.requestId === "terminal-auth"
      )).resolves.toMatchObject({
        methodId: "terminal-login",
        status: "succeeded",
        exitCode: 0,
      });

      bridge.receive(command({ type: "session/list", requestId: "old-agent-list" }));
      await expect(waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.requestId === "old-agent-list"
      )).resolves.toMatchObject({ code: -32_000 });

      bridge.close();
      const reconnectedSocket = new TestSocket();
      reconnected = new AcpBridge(reconnectedSocket as unknown as WebSocket, options);
      await reconnected.start();
      reconnected.receive(command({ type: "session/list", requestId: "reconnected-list" }));
      await expect(waitForMatching(reconnectedSocket, (item) =>
        item.type === "acp/sessions_listed" && item.requestId === "reconnected-list"
      )).resolves.toMatchObject({ response: { sessions: expect.any(Array) } });
    } finally {
      reconnected?.close();
      bridge.close();
      await rm(root, { recursive: true, force: true });
    }
  }, 15_000);

  it("accepts a transient replay mode and lets the attachment response establish final controls", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--invalid-load-mode-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "replay-current-new" }));
      const created = await waitForMatching(socket, (item) =>
        item.type === "acp/session_created" && item.requestId === "replay-current-new",
      );
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({ type: "session/list", requestId: "replay-list" }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/sessions_listed" && item.requestId === "replay-list",
      );

      let state = {
        ...initialState,
        session: created.response,
        modeId: created.response.modes?.currentModeId,
        configOptions: created.response.configOptions ?? [],
        timeline: [{
          id: "current-history",
          type: "assistant" as const,
          chunks: [{
            id: "current-history-chunk",
            role: "agent" as const,
            blocks: [{ type: "text" as const, text: "Keep current history." }],
            raw: [],
          }],
        }],
      };
      state = appReducer(state, {
        type: "session/transition_start",
        kind: "attach",
        requestId: "replay-invalid-load",
        sessionId: "saved-session",
        title: "Saved session",
      });
      const invalidStart = socket.events.length;
      bridge.receive(command({
        type: "session/load",
        requestId: "replay-invalid-load",
        sessionId: "saved-session",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/session_attached" && item.requestId === "replay-invalid-load",
      );
      for (const bridgeEvent of socket.events.slice(invalidStart)) {
        state = appReducer(state, { type: "server/event", event: bridgeEvent });
      }
      expect(state.session?.sessionId).toBe("saved-session");
      expect(state.modeId).toBe("plan");
      expect(state.configOptions).toEqual([
        expect.objectContaining({ id: "verbose", currentValue: true }),
      ]);
      expect(JSON.stringify(state.timeline)).toContain("Loaded history.");
      expect(state.sessionTransition).toBeUndefined();
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("keeps an out-of-order session creation from superseding the requested UI transition", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--race-new",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      let state = appReducer(initialState, {
        type: "session/transition_start",
        kind: "new",
        requestId: "new-race-primary",
      });
      bridge.receive(command({ type: "session/new", requestId: "new-race-primary" }));
      bridge.receive(command({ type: "session/new", requestId: "new-race-late-command" }));

      const unexpectedFirst = await waitForMatching(socket, (item) =>
        item.type === "acp/session_created" &&
        item.requestId === "new-race-late-command",
      );
      const requestedSecond = await waitForMatching(socket, (item) =>
        item.type === "acp/session_created" &&
        item.requestId === "new-race-primary",
      );
      expect(unexpectedFirst).toMatchObject({
        type: "acp/session_created",
        response: { sessionId: "test-session-2" },
      });
      expect(requestedSecond).toMatchObject({
        type: "acp/session_created",
        response: { sessionId: "test-session-1" },
      });

      for (const bridgeEvent of socket.events.filter(
        (item): item is Extract<ServerEvent, { type: "acp/session_created" }> =>
          item.type === "acp/session_created",
      )) {
        state = appReducer(state, { type: "server/event", event: bridgeEvent });
      }
      expect(state.session?.sessionId).toBe("test-session-1");
      expect(state.sessionTransition).toBeUndefined();
      expect(state.backgroundEvents).toEqual([
        expect.objectContaining({
          type: "acp/session_created",
          requestId: "new-race-late-command",
        }),
      ]);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("atomically replays updates emitted before session/new returns", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--early-new-updates",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      let state = appReducer(initialState, {
        type: "session/transition_start",
        kind: "new",
        requestId: "early-new",
      });
      bridge.receive(command({ type: "session/new", requestId: "early-new" }));
      const created = await waitForMatching(socket, (event) =>
        event.type === "acp/session_created" && event.requestId === "early-new",
      );
      if (created.type !== "acp/session_created") throw new Error("unreachable");

      expect(created.earlyUpdates?.map(({ update }) => update.sessionUpdate)).toEqual([
        "current_mode_update",
        "config_option_update",
        "available_commands_update",
        "usage_update",
        "session_info_update",
        "agent_message_chunk",
      ]);
      expect(socket.events.some((event) =>
        event.type === "acp/session_update" &&
        event.notification.sessionId === created.response.sessionId
      )).toBe(false);

      state = appReducer(state, { type: "server/event", event: created });
      expect(state.session?.sessionId).toBe("test-session");
      expect(state.availableCommands).toEqual([
        expect.objectContaining({ name: "bootstrap" }),
      ]);
      expect(state.usage).toEqual({ used: 5, size: 100, cost: undefined });
      expect(state.title).toBe("Early ACP session");
      expect(state.timeline).toEqual([
        expect.objectContaining({
          type: "assistant",
          chunks: [expect.objectContaining({ messageId: "early-session-message" })],
        }),
      ]);
      expect(state.modeId).toBe("build");
      expect(state.configOptions).toEqual([
        expect.objectContaining({ id: "verbose", currentValue: false }),
      ]);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("does not track an oversized session/new response and retries cleanly", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--oversized-new-response-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "oversized-new" }));
      await waitForMatching(socket, (event) =>
        event.type === "bridge/error" &&
        event.requestId === "oversized-new" &&
        event.message.includes("session/new response exceeds 4000000"),
      );
      expect(socket.events.some((event) =>
        event.type === "acp/session_created" && event.requestId === "oversized-new"
      )).toBe(false);

      bridge.receive(command({ type: "session/new", requestId: "new-after-oversized" }));
      const created = await waitForMatching(socket, (event) =>
        event.type === "acp/session_created" && event.requestId === "new-after-oversized",
      );
      expect(created).toMatchObject({
        type: "acp/session_created",
        response: {
          sessionId: "test-session",
          _meta: { observedSessionCloses: ["test-session"] },
        },
      });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("does not invent session/close while cleaning up for an Agent that omitted it", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--minimal",
        "--oversized-new-response-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "minimal-oversized-new" }));
      await waitForMatching(socket, (event) =>
        event.type === "bridge/error" && event.requestId === "minimal-oversized-new"
      );
      bridge.receive(command({ type: "session/new", requestId: "minimal-new-retry" }));
      const created = await waitForMatching(socket, (event) =>
        event.type === "acp/session_created" && event.requestId === "minimal-new-retry"
      );
      expect(created.type === "acp/session_created" ? created.response._meta : undefined)
        .toMatchObject({ observedSessionCloses: [] });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("relays an early dynamic mode without requiring it in the creation response", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--invalid-early-new-mode-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "invalid-early-new" }));
      const created = await waitForMatching(socket, (event) =>
        event.type === "acp/session_created" && event.requestId === "invalid-early-new",
      );
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      expect(created.response._meta).toMatchObject({ observedSessionCloses: [] });
      expect(created.earlyUpdates?.[0]).toMatchObject({
        sessionId: "test-session",
        update: { sessionUpdate: "current_mode_update", currentModeId: "ghost" },
      });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("closes an untracked fork whose Agent response cannot be relayed", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--oversized-fork-response-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "fork-cleanup-source" }));
      const source = await waitFor(socket, "acp/session_created");
      if (source.type !== "acp/session_created") throw new Error("unreachable");

      bridge.receive(command({
        type: "session/fork",
        requestId: "oversized-fork",
        sessionId: source.response.sessionId,
      }));
      await waitForMatching(socket, (event) =>
        event.type === "bridge/error" &&
        event.requestId === "oversized-fork" &&
        event.message.includes("session/fork response exceeds 4000000"),
      );
      expect(socket.events.some((event) =>
        event.type === "acp/session_forked" && event.requestId === "oversized-fork"
      )).toBe(false);

      bridge.receive(command({
        type: "session/fork",
        requestId: "fork-after-cleanup",
        sessionId: source.response.sessionId,
      }));
      const forked = await waitForMatching(socket, (event) =>
        event.type === "acp/session_forked" && event.requestId === "fork-after-cleanup"
      );
      expect(forked.type === "acp/session_forked" ? forked.response : undefined)
        .toMatchObject({
          sessionId: "forked-session",
          _meta: { observedSessionCloses: ["forked-session"] },
        });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("never closes the active source when a rejected fork reuses its ID", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--fork-source-id-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "fork-source-id-new" }));
      const source = await waitFor(socket, "acp/session_created");
      if (source.type !== "acp/session_created") throw new Error("unreachable");

      bridge.receive(command({
        type: "session/fork",
        requestId: "fork-source-id",
        sessionId: source.response.sessionId,
      }));
      await waitForMatching(socket, (event) =>
        event.type === "bridge/error" &&
        event.requestId === "fork-source-id" &&
        event.message.includes("source session ID"),
      );
      bridge.receive(command({
        type: "session/set_config_option",
        requestId: "source-control-after-bad-fork",
        sessionId: source.response.sessionId,
        configId: "verbose",
        value: true,
      }));
      const controlled = await waitForMatching(socket, (event) =>
        event.type === "acp/config_changed" &&
        event.requestId === "source-control-after-bad-fork"
      );
      expect(controlled.type === "acp/config_changed" ? controlled.response._meta : undefined)
        .toMatchObject({ observedSessionCloses: [] });

      bridge.receive(command({
        type: "session/fork",
        requestId: "fork-after-source-id",
        sessionId: source.response.sessionId,
      }));
      await waitForMatching(socket, (event) =>
        event.type === "acp/session_forked" && event.requestId === "fork-after-source-id"
      );
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("binds pre-response session/fork updates to the returned fork ID", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--early-fork-updates",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      let state = appReducer(initialState, {
        type: "session/transition_start",
        kind: "new",
        requestId: "fork-source",
      });
      bridge.receive(command({ type: "session/new", requestId: "fork-source" }));
      const created = await waitForMatching(socket, (event) =>
        event.type === "acp/session_created" && event.requestId === "fork-source",
      );
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      state = appReducer(state, { type: "server/event", event: created });

      state = appReducer(state, {
        type: "session/transition_start",
        kind: "fork",
        requestId: "early-fork",
        sessionId: created.response.sessionId,
      });
      bridge.receive(command({
        type: "session/fork",
        requestId: "early-fork",
        sessionId: created.response.sessionId,
      }));
      const forked = await waitForMatching(socket, (event) =>
        event.type === "acp/session_forked" && event.requestId === "early-fork",
      );
      if (forked.type !== "acp/session_forked") throw new Error("unreachable");
      expect(forked.earlyUpdates).toHaveLength(2);

      state = appReducer(state, { type: "server/event", event: forked });
      expect(state.session?.sessionId).toBe("forked-session");
      expect(state.availableCommands).toEqual([
        expect.objectContaining({ name: "fork-status" }),
      ]);
      expect(state.timeline).toEqual([
        expect.objectContaining({
          type: "assistant",
          chunks: [expect.objectContaining({ messageId: "early-fork-message" })],
        }),
      ]);
      expect(state.modeId).toBe("build");
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("locks a fork source against concurrent mutations and duplicate forks", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--slow-fork",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "fork-race-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      const sourceSessionId = created.response.sessionId;

      bridge.receive(command({
        type: "session/fork",
        requestId: "fork-race-primary",
        sessionId: sourceSessionId,
      }));
      bridge.receive(command({
        type: "session/prompt",
        requestId: "fork-race-prompt",
        sessionId: sourceSessionId,
        prompt: [{ type: "text", text: "must not run during fork" }],
      }));
      bridge.receive(command({
        type: "session/close",
        requestId: "fork-race-close",
        sessionId: sourceSessionId,
      }));
      bridge.receive(command({
        type: "session/set_mode",
        requestId: "fork-race-mode",
        sessionId: sourceSessionId,
        modeId: "plan",
      }));
      bridge.receive(command({
        type: "session/fork",
        requestId: "fork-race-duplicate",
        sessionId: sourceSessionId,
      }));

      for (const [requestId, fragment] of [
        ["fork-race-prompt", "Cannot start a prompt"],
        ["fork-race-close", "Cannot close the session"],
        ["fork-race-mode", "Cannot change the mode"],
        ["fork-race-duplicate", "already running"],
      ] as const) {
        await waitForMatching(socket, (item) =>
          item.type === "bridge/error" &&
          item.requestId === requestId &&
          item.message.includes(fragment),
        );
      }

      const forked = await waitForMatching(socket, (item) =>
        item.type === "acp/session_forked" && item.requestId === "fork-race-primary",
      );
      expect(forked).toMatchObject({
        type: "acp/session_forked",
        sourceSessionId,
        response: { sessionId: "forked-session" },
      });
      expect(socket.events.filter((item) => item.type === "acp/session_forked")).toHaveLength(1);

      bridge.receive(command({
        type: "session/prompt",
        requestId: "fork-race-after",
        sessionId: sourceSessionId,
        prompt: [{ type: "text", text: "background-flow" }],
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "fork-race-after",
      );
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("locks a closing session, unlocks after failure, and commits one retry", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--slow-close",
        "--fail-close-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "close-race-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      const sessionId = created.response.sessionId;

      bridge.receive(command({
        type: "session/close",
        requestId: "close-race-primary",
        sessionId,
      }));
      bridge.receive(command({
        type: "session/prompt",
        requestId: "close-race-prompt",
        sessionId,
        prompt: [{ type: "text", text: "must not run during close" }],
      }));
      bridge.receive(command({
        type: "session/fork",
        requestId: "close-race-fork",
        sessionId,
      }));
      bridge.receive(command({
        type: "session/set_mode",
        requestId: "close-race-mode",
        sessionId,
        modeId: "plan",
      }));
      bridge.receive(command({
        type: "session/close",
        requestId: "close-race-duplicate",
        sessionId,
      }));

      for (const [requestId, fragment] of [
        ["close-race-prompt", "Cannot start a prompt"],
        ["close-race-fork", "Cannot fork the session"],
        ["close-race-mode", "Cannot change the mode"],
        ["close-race-duplicate", "already running"],
      ] as const) {
        await waitForMatching(socket, (item) =>
          item.type === "bridge/error" &&
          item.requestId === requestId &&
          item.message.includes(fragment),
        );
      }
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "close-race-primary" &&
        item.message.includes("Synthetic close failure"),
      );
      expect(socket.events.some((item) => item.type === "acp/session_closed")).toBe(false);

      bridge.receive(command({
        type: "session/prompt",
        requestId: "close-race-after-failure",
        sessionId,
        prompt: [{ type: "text", text: "background-flow" }],
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" &&
        item.requestId === "close-race-after-failure",
      );

      bridge.receive(command({
        type: "session/close",
        requestId: "close-race-retry",
        sessionId,
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/session_closed" && item.requestId === "close-race-retry",
      );
      expect(socket.events.filter((item) => item.type === "acp/session_closed")).toHaveLength(1);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("serializes session controls against prompts and lifecycle mutations", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--slow-control",
        "--fail-control-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "control-race-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      const sessionId = created.response.sessionId;

      bridge.receive(command({
        type: "session/set_mode",
        requestId: "control-race-primary",
        sessionId,
        modeId: "plan",
      }));
      bridge.receive(command({
        type: "session/set_config_option",
        requestId: "control-race-duplicate",
        sessionId,
        configId: "verbose",
        value: true,
      }));
      bridge.receive(command({
        type: "session/prompt",
        requestId: "control-race-prompt",
        sessionId,
        prompt: [{ type: "text", text: "must not run during a control update" }],
      }));
      bridge.receive(command({
        type: "session/fork",
        requestId: "control-race-fork",
        sessionId,
      }));
      bridge.receive(command({
        type: "session/close",
        requestId: "control-race-close",
        sessionId,
      }));

      for (const [requestId, fragment] of [
        ["control-race-duplicate", "already running"],
        ["control-race-prompt", "Cannot start a prompt"],
        ["control-race-fork", "Cannot fork the session"],
        ["control-race-close", "Cannot close the session"],
      ] as const) {
        await waitForMatching(socket, (item) =>
          item.type === "bridge/error" &&
          item.requestId === requestId &&
          item.message.includes(fragment),
        );
      }
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "control-race-primary" &&
        item.message.includes("Synthetic control failure"),
      );
      expect(socket.events.some((item) => item.type === "acp/mode_changed")).toBe(false);

      bridge.receive(command({
        type: "session/set_config_option",
        requestId: "control-race-retry",
        sessionId,
        configId: "verbose",
        value: true,
      }));
      const changed = await waitForMatching(socket, (item) =>
        item.type === "acp/config_changed" && item.requestId === "control-race-retry",
      );
      expect(changed).toMatchObject({
        type: "acp/config_changed",
        response: {
          configOptions: [expect.objectContaining({ id: "verbose", currentValue: true })],
        },
      });

      bridge.receive(command({
        type: "session/prompt",
        requestId: "control-race-after",
        sessionId,
        prompt: [{ type: "text", text: "background-flow" }],
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "control-race-after",
      );
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects an oversized config response before committing tracked controls", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--oversized-config-response-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "config-budget-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");

      bridge.receive(command({
        type: "session/set_config_option",
        requestId: "oversized-config",
        sessionId: created.response.sessionId,
        configId: "verbose",
        value: true,
      }));
      await waitForMatching(socket, (event) =>
        event.type === "bridge/error" &&
        event.requestId === "oversized-config" &&
        event.message.includes("set_config_option response exceeds 4000000"),
      );
      expect(socket.events.some((event) =>
        event.type === "acp/config_changed" && event.requestId === "oversized-config"
      )).toBe(false);

      bridge.receive(command({
        type: "session/set_config_option",
        requestId: "config-after-oversized",
        sessionId: created.response.sessionId,
        configId: "verbose",
        value: false,
      }));
      const changed = await waitForMatching(socket, (event) =>
        event.type === "acp/config_changed" &&
        event.requestId === "config-after-oversized",
      );
      expect(changed).toMatchObject({
        type: "acp/config_changed",
        response: {
          configOptions: [expect.objectContaining({ id: "verbose", currentValue: false })],
        },
      });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("does not unlock the active prompt when a rapid duplicate is rejected", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "prompt-race-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      const sessionId = created.response.sessionId;
      const primaryBlocks = [{ type: "text" as const, text: "background-flow" }];
      let state = appReducer({
        ...initialState,
        session: created.response,
      }, {
        type: "user/prompt",
        requestId: "prompt-race-primary",
        sessionId,
        blocks: primaryBlocks,
      });
      const afterDuplicateUiAction = appReducer(state, {
        type: "user/prompt",
        requestId: "prompt-race-duplicate",
        sessionId,
        blocks: [{ type: "text", text: "duplicate" }],
      });
      expect(afterDuplicateUiAction).toBe(state);

      bridge.receive(command({
        type: "session/prompt",
        requestId: "prompt-race-primary",
        sessionId,
        prompt: primaryBlocks,
      }));
      bridge.receive(command({
        type: "session/prompt",
        requestId: "prompt-race-duplicate",
        sessionId,
        prompt: [{ type: "text", text: "duplicate" }],
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "prompt-race-duplicate" &&
        item.message.includes("already running"),
      );

      const duplicateError = socket.events.find((item) =>
        item.type === "bridge/error" && item.requestId === "prompt-race-duplicate"
      );
      if (!duplicateError) throw new Error("Missing duplicate prompt error");
      state = appReducer(state, { type: "server/event", event: duplicateError });
      expect(state.running).toBe(true);
      expect(state.pendingPrompt?.requestId).toBe("prompt-race-primary");

      const completed = await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "prompt-race-primary",
      );
      state = appReducer(state, { type: "server/event", event: completed });
      expect(state.running).toBe(false);
      expect(state.pendingPrompt).toBeUndefined();
      expect(state.timeline.filter((item) => item.type === "message" && item.role === "user"))
        .toHaveLength(1);
      expect(state.timeline.at(-1)).toMatchObject({ type: "stop" });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("serializes session deletion against attachment in both directions", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--slow-load",
        "--slow-delete",
        "--fail-delete-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/list", requestId: "delete-race-list" }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/sessions_listed" && item.requestId === "delete-race-list",
      );

      bridge.receive(command({
        type: "session/delete",
        requestId: "delete-race-primary",
        sessionId: "saved-session",
      }));
      bridge.receive(command({
        type: "session/load",
        requestId: "delete-race-load-during-delete",
        sessionId: "saved-session",
      }));
      bridge.receive(command({
        type: "session/delete",
        requestId: "delete-race-duplicate",
        sessionId: "saved-session",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "delete-race-load-during-delete" &&
        item.message.includes("being deleted"),
      );
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "delete-race-duplicate" &&
        item.message.includes("already running"),
      );
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "delete-race-primary" &&
        item.message.includes("Synthetic delete failure"),
      );

      bridge.receive(command({
        type: "session/load",
        requestId: "delete-race-load-primary",
        sessionId: "saved-session",
      }));
      bridge.receive(command({
        type: "session/delete",
        requestId: "delete-race-during-load",
        sessionId: "saved-session",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "delete-race-during-load" &&
        item.message.includes("Wait for the session attachment"),
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/session_attached" && item.requestId === "delete-race-load-primary",
      );

      bridge.receive(command({
        type: "session/close",
        requestId: "delete-race-close",
        sessionId: "saved-session",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/session_closed" && item.requestId === "delete-race-close",
      );
      bridge.receive(command({
        type: "session/delete",
        requestId: "delete-race-retry",
        sessionId: "saved-session",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/session_deleted" && item.requestId === "delete-race-retry",
      );
      expect(socket.events.filter((item) => item.type === "acp/session_deleted")).toHaveLength(1);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects cyclic Agent pagination cursors", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--cyclic-list",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/list", requestId: "cycle-list-1" }));
      const first = await waitForMatching(socket, (item) =>
        item.type === "acp/sessions_listed" && item.requestId === "cycle-list-1",
      );
      if (first.type !== "acp/sessions_listed") throw new Error("unreachable");
      expect(first.response.nextCursor).toBe("cursor-a");
      bridge.receive(command({
        type: "session/list",
        requestId: "cycle-list-2",
        cursor: "cursor-a",
      }));
      const second = await waitForMatching(socket, (item) =>
        item.type === "acp/sessions_listed" && item.requestId === "cycle-list-2",
      );
      if (second.type !== "acp/sessions_listed") throw new Error("unreachable");
      expect(second.response.nextCursor).toBe("cursor-b");
      bridge.receive(command({
        type: "session/list",
        requestId: "cycle-list-3",
        cursor: "cursor-b",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "cycle-list-3" &&
        item.message.includes("reused session/list cursor"),
      );
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("relays duplicate sessions across pagination pages for client-side upsert", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--duplicate-list-page",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/list", requestId: "duplicate-list-1" }));
      const first = await waitForMatching(socket, (item) =>
        item.type === "acp/sessions_listed" && item.requestId === "duplicate-list-1",
      );
      if (first.type !== "acp/sessions_listed") throw new Error("unreachable");
      expect(first.response.nextCursor).toBe("duplicate-page-2");
      bridge.receive(command({
        type: "session/list",
        requestId: "duplicate-list-2",
        cursor: "duplicate-page-2",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/sessions_listed" && item.requestId === "duplicate-list-2",
      );
      expect(socket.events.filter(
        (item) => item.type === "acp/sessions_listed",
      )).toHaveLength(2);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("relays Agent-owned session cwd values instead of enforcing the request as a filter", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--invalid-list-refresh-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/list", requestId: "list-valid-before-refresh" }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/sessions_listed" &&
        item.requestId === "list-valid-before-refresh",
      );
      bridge.receive(command({ type: "session/list", requestId: "list-invalid-refresh" }));
      const refreshed = await waitForMatching(socket, (item) =>
        item.type === "acp/sessions_listed" && item.requestId === "list-invalid-refresh"
      );
      expect(refreshed.type === "acp/sessions_listed" ? refreshed.response.sessions : [])
        .toContainEqual(expect.objectContaining({
          sessionId: "wrong-workspace",
          cwd: expect.stringContaining("other-workspace"),
        }));
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("preserves session updates and relays permission decisions", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "new-1" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");

      bridge.receive(command({
        type: "session/prompt",
        requestId: "prompt-1",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "test" }],
      }));
      const permission = await waitFor(socket, "acp/permission_request");
      if (permission.type !== "acp/permission_request") throw new Error("unreachable");
      bridge.receive(command({
        type: "permission/respond",
        requestId: "invalid-permission-response",
        permissionId: permission.permissionId,
        outcome: { outcome: "selected", optionId: "invented" },
      }));
      const invalidResponse = await waitForMatching(
        socket,
        (item) => item.type === "bridge/error" && item.message.includes("not offered"),
      );
      expect(invalidResponse).toMatchObject({
        type: "bridge/error",
        requestId: "invalid-permission-response",
        operation: "permission/respond",
      });
      bridge.receive(command({
        type: "permission/respond",
        requestId: "allow-permission-response",
        permissionId: permission.permissionId,
        outcome: { outcome: "selected", optionId: "yes" },
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/permission_resolved" &&
        item.requestId === "allow-permission-response",
      );
      await waitFor(socket, "acp/prompt_complete");

      const updates = socket.events.filter((item) => item.type === "acp/session_update");
      expect(updates.map((item) => item.type === "acp/session_update" && item.notification.update.sessionUpdate)).toEqual([
        "available_commands_update",
        "plan",
        "tool_call",
        "tool_call_update",
        "agent_message_chunk",
        "agent_message_chunk",
      ]);
      expect(socket.events).toContainEqual(expect.objectContaining({ type: "acp/initialized" }));
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("preserves bounded ACP RequestError details through the browser reducer", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "error-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "request-error",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "request-error-flow" }],
      }));
      const bridgeError = await waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.requestId === "request-error",
      );
      if (bridgeError.type !== "bridge/error") throw new Error("unreachable");
      expect(bridgeError).toMatchObject({
        operation: "session/prompt",
        code: -32_000,
        data: {
          hint: "Configure the Agent provider",
          padding: expect.any(String),
        },
        message: expect.stringContaining("ACP error -32000: Authentication required"),
      });
      expect(bridgeError.dataBytes).toBeGreaterThan(20_000);
      expect(bridgeError.message).toContain("Configure the Agent provider");
      expect(bridgeError.message).toContain("…");
      expect(bridgeError.message.length).toBeLessThan(4_200);

      const running = appReducer({
        ...initialState,
        session: { sessionId: created.response.sessionId },
      }, {
        type: "user/prompt",
        requestId: "request-error",
        sessionId: created.response.sessionId,
        blocks: [{ type: "text", text: "request-error-flow" }],
      });
      const visible = appReducer(running, { type: "server/event", event: bridgeError });
      expect(visible.running).toBe(false);
      expect(visible.pendingPrompt).toBeUndefined();
      expect(visible.timeline.at(-1)).toMatchObject({
        type: "error",
        code: -32_000,
        data: { hint: "Configure the Agent provider" },
        retryBlocks: [{ type: "text", text: "request-error-flow" }],
        message: expect.stringContaining("Configure the Agent provider"),
      });

      bridge.receive(command({
        type: "session/prompt",
        requestId: "oversized-error-data",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "oversized-error-data-flow" }],
      }));
      const oversized = await waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.requestId === "oversized-error-data",
      );
      expect(oversized).toMatchObject({
        type: "bridge/error",
        code: -32_603,
        dataTruncated: true,
        dataBytes: expect.any(Number),
      });
      if (oversized.type !== "bridge/error") throw new Error("unreachable");
      expect(oversized.data).toBeUndefined();
      expect(oversized.dataBytes).toBeGreaterThan(256 * 1024);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("validates prompt usage and recovers after an invalid Agent response", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "usage-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      const sessionId = created.response.sessionId;

      bridge.receive(command({
        type: "session/prompt",
        requestId: "usage-valid",
        sessionId,
        prompt: [{ type: "text", text: "usage-flow" }],
      }));
      const valid = await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "usage-valid",
      );
      expect(valid).toMatchObject({
        type: "acp/prompt_complete",
        response: {
          stopReason: "max_tokens",
          usage: {
            totalTokens: 21,
            inputTokens: 13,
            outputTokens: 8,
            thoughtTokens: 3,
          },
        },
      });

      bridge.receive(command({
        type: "session/prompt",
        requestId: "usage-invalid",
        sessionId,
        prompt: [{ type: "text", text: "invalid-usage-flow" }],
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "usage-invalid" &&
        item.operation === "session/prompt" &&
        item.message.includes("exceeds totalTokens"),
      );
      expect(socket.events.some((item) =>
        item.type === "acp/prompt_complete" && item.requestId === "usage-invalid"
      )).toBe(false);

      bridge.receive(command({
        type: "session/prompt",
        requestId: "usage-after-invalid",
        sessionId,
        prompt: [{ type: "text", text: "background-flow" }],
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "usage-after-invalid",
      );
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects oversized notification metadata before mutating update lifecycle state", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "relay-budget-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "relay-budget-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "oversized-notification-flow" }],
      }));

      await waitForMatching(socket, (event) =>
        event.type === "bridge/error" &&
        event.message.includes("session/update notification exceeds 4000000"),
      );
      const accepted = await waitForMatching(socket, (event) =>
        event.type === "acp/session_update" &&
        event.notification.update.sessionUpdate === "tool_call" &&
        event.notification.update.toolCallId === "relay-budget-tool",
      );
      expect(accepted).toMatchObject({
        type: "acp/session_update",
        notification: {
          update: {
            title: "Accepted after oversized notification",
            status: "completed",
          },
        },
      });
      await waitForMatching(socket, (event) =>
        event.type === "acp/prompt_complete" &&
        event.requestId === "relay-budget-prompt",
      );
      expect(socket.events.filter((event) =>
        event.type === "acp/session_update" &&
        event.notification.update.sessionUpdate === "tool_call" &&
        event.notification.update.toolCallId === "relay-budget-tool"
      )).toHaveLength(1);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("drops an invalid Agent update without poisoning the ACP connection", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "invalid-update-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "invalid-update-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "invalid-compaction-flow" }],
      }));

      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.message.includes("summary chunks require an in-progress compaction"),
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "after-invalid-update",
      );
      await waitFor(socket, "acp/prompt_complete");
      expect(socket.events.some((item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "compaction_summary_chunk" &&
        item.notification.update.compactionId === "not-started",
      )).toBe(false);

      bridge.receive(command({
        type: "session/prompt",
        requestId: "invalid-permission-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "invalid-permission-flow" }],
      }));
      const independentPermission = await waitForMatching(socket, (item) =>
        item.type === "acp/permission_request" &&
        item.request.toolCall.toolCallId === "never-created",
      );
      if (independentPermission.type !== "acp/permission_request") {
        throw new Error("Missing self-contained permission request");
      }
      bridge.receive(command({
        type: "permission/respond",
        requestId: "independent-permission-response",
        permissionId: independentPermission.permissionId,
        outcome: { outcome: "selected", optionId: "allow" },
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/permission_resolved" &&
        item.requestId === "independent-permission-response",
      );
      const rejected = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "invalid-permission-result",
      );
      if (
        rejected.type !== "acp/session_update" ||
        rejected.notification.update.sessionUpdate !== "agent_message_chunk" ||
        rejected.notification.update.content.type !== "text"
      ) throw new Error("Missing invalid permission result");
      expect(rejected.notification.update.content.text).toBe("Invalid permission rejected: false.");
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" &&
        item.requestId === "invalid-permission-prompt",
      );
      bridge.receive(command({
        type: "session/prompt",
        requestId: "invalid-elicitation-tool-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "invalid-elicitation-tool-flow" }],
      }));
      const independentElicitation = await waitForMatching(socket, (item) =>
        item.type === "acp/elicitation_request" &&
        "toolCallId" in item.request && item.request.toolCallId === "never-created",
      );
      if (independentElicitation.type !== "acp/elicitation_request") {
        throw new Error("Missing independently scoped elicitation");
      }
      bridge.receive(command({
        type: "elicitation/respond",
        requestId: "independent-elicitation-response",
        elicitationId: independentElicitation.elicitationId,
        response: { action: "accept", content: {} },
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/elicitation_resolved" &&
        item.requestId === "independent-elicitation-response",
      );
      const invalidElicitation = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "invalid-elicitation-tool-result",
      );
      if (
        invalidElicitation.type !== "acp/session_update" ||
        invalidElicitation.notification.update.sessionUpdate !== "agent_message_chunk" ||
        invalidElicitation.notification.update.content.type !== "text"
      ) throw new Error("Missing invalid elicitation result");
      expect(invalidElicitation.notification.update.content.text)
        .toBe("Invalid elicitation rejected: false.");
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" &&
        item.requestId === "invalid-elicitation-tool-prompt",
      );
      bridge.receive(command({
        type: "session/prompt",
        requestId: "invalid-message-role-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "invalid-message-role-flow" }],
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_thought_chunk" &&
        item.notification.update.messageId === "cross-role-message",
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "invalid-message-role-result",
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" &&
        item.requestId === "invalid-message-role-prompt",
      );
      expect(socket.events.some((item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_thought_chunk" &&
        item.notification.update.messageId === "cross-role-message",
      )).toBe(true);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("drops semantically invalid Agent media without poisoning message identity", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "invalid-content-new" }));
      const created = await waitForMatching(socket, (item) =>
        item.type === "acp/session_created" && item.requestId === "invalid-content-new",
      );
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "invalid-content-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "invalid-content-flow" }],
      }));

      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.message.includes("image MIME type must use the image/* family"),
      );
      const recovered = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "content-recovery",
      );
      if (
        recovered.type !== "acp/session_update" ||
        recovered.notification.update.sessionUpdate !== "agent_message_chunk"
      ) throw new Error("Missing recovered content update");
      expect(recovered.notification.update.content).toEqual({
        type: "text",
        text: "Connection survived invalid media.",
      });
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "invalid-content-prompt",
      );
      expect(socket.events.some((item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.content.type === "image"
      )).toBe(false);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("drops an invalid dynamic config update and accepts the next complete replacement", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "config-update-new" }));
      const created = await waitForMatching(socket, (item) =>
        item.type === "acp/session_created" && item.requestId === "config-update-new",
      );
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "config-update-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "invalid-config-update-flow" }],
      }));

      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.message.includes("duplicate config option ID"),
      );
      const validUpdate = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "config_option_update",
      );
      if (
        validUpdate.type !== "acp/session_update" ||
        validUpdate.notification.update.sessionUpdate !== "config_option_update"
      ) throw new Error("Missing valid config update");
      expect(validUpdate.notification.update.configOptions).toEqual([{
        type: "boolean",
        id: "verbose",
        name: "Verbose",
        currentValue: true,
      }]);
      await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "config-update-recovery",
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "config-update-prompt",
      );
      expect(socket.events.filter((item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "config_option_update"
      )).toHaveLength(1);

      bridge.receive(command({
        type: "session/set_config_option",
        requestId: "config-update-after",
        sessionId: created.response.sessionId,
        configId: "verbose",
        value: false,
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/config_changed" && item.requestId === "config-update-after",
      );
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects an inactive terminal reference before tracking its tool call", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "terminal-reference-new" }));
      const created = await waitForMatching(socket, (item) =>
        item.type === "acp/session_created" && item.requestId === "terminal-reference-new",
      );
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "terminal-reference-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "invalid-terminal-flow" }],
      }));

      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.message.includes("Unknown terminal: never-created"),
      );
      const recovered = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "tool_call" &&
        item.notification.update.toolCallId === "terminal-recovery",
      );
      if (
        recovered.type !== "acp/session_update" ||
        recovered.notification.update.sessionUpdate !== "tool_call"
      ) throw new Error("Missing recovered tool call");
      expect(recovered.notification.update.title).toBe("Recovered tool output");
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "terminal-reference-prompt",
      );
      expect(socket.events.filter((item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "tool_call" &&
        item.notification.update.toolCallId === "terminal-recovery"
      )).toHaveLength(1);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("accepts a live session-owned terminal reference and its terminal lifecycle", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "terminal-flow-new" }));
      const created = await waitForMatching(socket, (item) =>
        item.type === "acp/session_created" && item.requestId === "terminal-flow-new",
      );
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "terminal-flow-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "terminal-flow" }],
      }));

      const tool = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "tool_call" &&
        item.notification.update.toolCallId === "terminal-tool",
      );
      if (
        tool.type !== "acp/session_update" ||
        tool.notification.update.sessionUpdate !== "tool_call"
      ) throw new Error("Missing terminal tool call");
      expect(tool.notification.update.content?.[0]).toMatchObject({
        type: "terminal",
        terminalId: expect.any(String),
      });
      expect(tool.notification.update.locations).toEqual([{
        path: process.cwd(),
        line: 0,
      }]);
      const completed = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "tool_call_update" &&
        item.notification.update.toolCallId === "terminal-tool" &&
        item.notification.update.status === "completed",
      );
      if (
        completed.type !== "acp/session_update" ||
        completed.notification.update.sessionUpdate !== "tool_call_update"
      ) throw new Error("Missing completed terminal tool call");
      expect(completed.notification.update.rawOutput).toMatchObject({
        output: "TERMINAL_FLOW_OUTPUT",
        truncated: false,
        exitStatus: { exitCode: 0 },
      });
      const terminalState = await waitForMatching(socket, (item) =>
        item.type === "acp/terminal_state" &&
        item.terminal.sessionId === created.response.sessionId &&
        item.terminal.output === "TERMINAL_FLOW_OUTPUT" &&
        item.terminal.released,
      );
      if (terminalState.type !== "acp/terminal_state") {
        throw new Error("Missing terminal presentation state");
      }
      expect(terminalState.terminal).toMatchObject({
        truncated: false,
        released: true,
        exitStatus: { exitCode: 0 },
      });
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "terminal-flow-prompt",
      );
      expect(socket.events.some((item) =>
        item.type === "bridge/error" && item.message.includes("terminal")
      )).toBe(false);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("propagates Agent cancellation into terminal waiters without killing the process", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "terminal-cancel-new" }));
      const created = await waitForMatching(socket, (item) =>
        item.type === "acp/session_created" && item.requestId === "terminal-cancel-new",
      );
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      const eventStart = socket.events.length;
      bridge.receive(command({
        type: "session/prompt",
        requestId: "terminal-cancel-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "terminal-cancel-flow" }],
      }));

      const completed = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "tool_call_update" &&
        item.notification.update.toolCallId === "terminal-cancel-tool" &&
        item.notification.update.status === "completed",
      );
      if (
        completed.type !== "acp/session_update" ||
        completed.notification.update.sessionUpdate !== "tool_call_update"
      ) throw new Error("Missing terminal cancellation result");
      expect(completed.notification.update.rawOutput).toMatchObject({
        invalidCreateRejected: true,
        waitCancelled: true,
        processSurvivedCancellation: true,
        exitStatus: { signal: expect.any(String) },
      });
      await waitForMatching(socket, (item) =>
        item.type === "acp/terminal_state" &&
        item.terminal.sessionId === created.response.sessionId &&
        item.terminal.released,
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "terminal-cancel-prompt",
      );

      let browser = appReducer({
        ...initialState,
        phase: "ready",
        session: created.response,
      }, {
        type: "user/prompt",
        requestId: "terminal-cancel-prompt",
        sessionId: created.response.sessionId,
        blocks: [{ type: "text", text: "terminal-cancel-flow" }],
      });
      for (const event of socket.events.slice(eventStart)) {
        browser = appReducer(browser, { type: "server/event", event });
      }
      expect(browser.running).toBe(false);
      expect(browser.pendingPrompt).toBeUndefined();
      expect(browser.timeline.find((item) =>
        item.type === "tool" && item.call.toolCallId === "terminal-cancel-tool"
      )).toMatchObject({
        type: "tool",
        call: {
          status: "completed",
          rawOutput: {
            waitCancelled: true,
            processSurvivedCancellation: true,
          },
        },
      });
      expect(browser.terminalSnapshots.at(-1)?.released).toBe(true);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("coalesces terminal bursts while preserving the bounded final snapshot", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "terminal-burst-new" }));
      const created = await waitForMatching(socket, (item) =>
        item.type === "acp/session_created" && item.requestId === "terminal-burst-new",
      );
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "terminal-burst-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "terminal-burst-flow" }],
      }));

      const tool = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "tool_call" &&
        item.notification.update.toolCallId === "terminal-burst-tool",
      );
      if (
        tool.type !== "acp/session_update" ||
        tool.notification.update.sessionUpdate !== "tool_call" ||
        tool.notification.update.content?.[0]?.type !== "terminal"
      ) throw new Error("Missing terminal burst tool");
      const terminalId = tool.notification.update.content[0].terminalId;
      await waitForMatching(socket, (item) =>
        item.type === "acp/terminal_state" &&
        item.terminal.terminalId === terminalId &&
        item.terminal.released,
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "terminal-burst-prompt",
      );

      const snapshots = socket.events.filter((item) =>
        item.type === "acp/terminal_state" && item.terminal.terminalId === terminalId
      );
      expect(snapshots.length).toBeLessThan(30);
      expect(snapshots.at(-1)).toMatchObject({
        type: "acp/terminal_state",
        terminal: {
          output: "x".repeat(16),
          truncated: true,
          released: true,
          exitStatus: { exitCode: 0 },
        },
      });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects duplicate and unaccepted URL elicitation lifecycle events", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "session/new", requestId: "elicitation-new" }));
      const created = await waitFor(socket, "acp/session_created");
      if (created.type !== "acp/session_created") throw new Error("unreachable");
      bridge.receive(command({
        type: "session/prompt",
        requestId: "duplicate-url-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "duplicate-url-flow" }],
      }));
      const elicitation = await waitFor(socket, "acp/elicitation_request");
      if (elicitation.type !== "acp/elicitation_request") throw new Error("unreachable");
      bridge.receive(command({
        type: "elicitation/respond",
        requestId: "decline-duplicate-url-response",
        elicitationId: elicitation.elicitationId,
        response: { action: "decline" },
      }));
      const duplicateResult = await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "duplicate-url-result",
      );
      if (
        duplicateResult.type !== "acp/session_update" ||
        duplicateResult.notification.update.sessionUpdate !== "agent_message_chunk" ||
        duplicateResult.notification.update.content.type !== "text"
      ) throw new Error("Missing duplicate URL result");
      expect(duplicateResult.notification.update.content.text).toBe("Duplicate URL rejected: true.");
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "duplicate-url-prompt",
      );
      expect(socket.events.filter((item) => item.type === "acp/elicitation_request")).toHaveLength(1);

      const completeCount = socket.events.filter(
        (item) => item.type === "acp/elicitation_complete",
      ).length;
      bridge.receive(command({
        type: "session/prompt",
        requestId: "invalid-complete-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "invalid-complete-flow" }],
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" && item.message.includes("was not accepted"),
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/session_update" &&
        item.notification.update.sessionUpdate === "agent_message_chunk" &&
        item.notification.update.messageId === "invalid-complete-result",
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/prompt_complete" && item.requestId === "invalid-complete-prompt",
      );
      expect(socket.events.filter(
        (item) => item.type === "acp/elicitation_complete",
      )).toHaveLength(completeCount);
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects an unusable Agent NES session ID and allows a clean retry", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--invalid-nes-start-id-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "nes/start", requestId: "nes-invalid-start" }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "nes-invalid-start" &&
        item.message.includes("Agent session ID must contain between 1 and 1024 characters"),
      );
      expect(socket.events.some((item) =>
        item.type === "acp/nes_started" && item.requestId === "nes-invalid-start"
      )).toBe(false);

      bridge.receive(command({ type: "nes/start", requestId: "nes-start-retry" }));
      const started = await waitForMatching(socket, (item) =>
        item.type === "acp/nes_started" && item.requestId === "nes-start-retry"
      );
      expect(started.type === "acp/nes_started" ? started.response : undefined)
        .toMatchObject({
          sessionId: "nes-session",
          _meta: { observedCloseAttempts: 0 },
        });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("closes an Agent NES session whose start response cannot be relayed", async () => {
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--oversized-nes-start-response-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "nes/start", requestId: "nes-oversized-start" }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "nes-oversized-start" &&
        item.message.includes("Agent nes/start response exceeds 4000000 browser relay bytes"),
      );
      expect(socket.events.some((item) =>
        item.type === "acp/nes_started" && item.requestId === "nes-oversized-start"
      )).toBe(false);

      bridge.receive(command({ type: "nes/start", requestId: "nes-start-after-cleanup" }));
      const started = await waitForMatching(socket, (item) =>
        item.type === "acp/nes_started" && item.requestId === "nes-start-after-cleanup"
      );
      expect(started.type === "acp/nes_started" ? started.response : undefined)
        .toMatchObject({
          sessionId: "nes-session",
          _meta: { observedCloseAttempts: 1 },
        });
    } finally {
      bridge.close();
    }
  }, 10_000);

  it("rejects an unusable Agent NES suggestion ID without poisoning later suggestions", async () => {
    const directory = await mkdtemp(join(tmpdir(), "attyd-nes-invalid-suggestion-"));
    const documentPath = join(directory, "sample.ts");
    await writeFile(documentPath, "export const value = 1;\n", "utf8");
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--invalid-nes-suggestion-id-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
      additionalDirectories: [directory],
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "nes/start", requestId: "nes-start" }));
      const started = await waitFor(socket, "acp/nes_started");
      if (started.type !== "acp/nes_started") throw new Error("unreachable");
      bridge.receive(command({
        type: "document/open",
        requestId: "doc-open",
        sessionId: started.response.sessionId,
        path: documentPath,
        languageId: "typescript",
      }));
      const opened = await waitFor(socket, "acp/document_opened");
      if (opened.type !== "acp/document_opened") throw new Error("unreachable");

      bridge.receive(command({
        type: "nes/suggest",
        requestId: "nes-invalid-suggest",
        sessionId: started.response.sessionId,
        uri: opened.document.uri,
        position: { line: 0, character: 0 },
        triggerKind: "manual",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "nes-invalid-suggest" &&
        item.message.includes("NES suggestion ID must contain between 1 and 1024 characters"),
      );
      expect(socket.events.some((item) =>
        item.type === "acp/nes_suggestions" && item.requestId === "nes-invalid-suggest"
      )).toBe(false);

      bridge.receive(command({
        type: "nes/suggest",
        requestId: "nes-suggest-retry",
        sessionId: started.response.sessionId,
        uri: opened.document.uri,
        position: { line: 0, character: 0 },
        triggerKind: "manual",
      }));
      const retried = await waitForMatching(socket, (item) =>
        item.type === "acp/nes_suggestions" && item.requestId === "nes-suggest-retry"
      );
      expect(retried.type === "acp/nes_suggestions"
        ? retried.response.suggestions.map(({ id }) => id)
        : undefined).toEqual(["nes-edit-1", "nes-jump-1"]);
    } finally {
      bridge.close();
      await rm(directory, { recursive: true, force: true });
    }
  }, 10_000);

  it("rejects an NES edit cursor outside the resulting document and accepts a clean retry", async () => {
    const directory = await mkdtemp(join(tmpdir(), "attyd-nes-invalid-cursor-"));
    const documentPath = join(directory, "sample.ts");
    await writeFile(documentPath, "export const value = 1;\n", "utf8");
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
        "--invalid-nes-cursor-once",
      ],
      cwd: process.cwd(),
      readOnly: false,
      additionalDirectories: [directory],
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "nes/start", requestId: "nes-start" }));
      const started = await waitFor(socket, "acp/nes_started");
      if (started.type !== "acp/nes_started") throw new Error("unreachable");
      bridge.receive(command({
        type: "document/open",
        requestId: "doc-open",
        sessionId: started.response.sessionId,
        path: documentPath,
        languageId: "typescript",
      }));
      const opened = await waitFor(socket, "acp/document_opened");
      if (opened.type !== "acp/document_opened") throw new Error("unreachable");

      bridge.receive(command({
        type: "nes/suggest",
        requestId: "nes-invalid-cursor",
        sessionId: started.response.sessionId,
        uri: opened.document.uri,
        position: { line: 0, character: 0 },
        triggerKind: "manual",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "nes-invalid-cursor" &&
        item.message.includes("Position line is outside the document"),
      );
      expect(socket.events.some((item) =>
        item.type === "acp/nes_suggestions" && item.requestId === "nes-invalid-cursor"
      )).toBe(false);

      bridge.receive(command({
        type: "nes/suggest",
        requestId: "nes-cursor-retry",
        sessionId: started.response.sessionId,
        uri: opened.document.uri,
        position: { line: 0, character: 0 },
        triggerKind: "manual",
      }));
      const retried = await waitForMatching(socket, (item) =>
        item.type === "acp/nes_suggestions" && item.requestId === "nes-cursor-retry"
      );
      expect(retried.type === "acp/nes_suggestions"
        ? retried.response.suggestions[0]
        : undefined).toMatchObject({
          kind: "edit",
          cursorPosition: { line: 0, character: 9 },
        });
    } finally {
      bridge.close();
      await rm(directory, { recursive: true, force: true });
    }
  }, 10_000);

  it("does not commit NES focus or edit state before required Agent notifications", async () => {
    const directory = await mkdtemp(join(tmpdir(), "attyd-nes-notify-failure-"));
    const documentPath = join(directory, "sample.ts");
    await writeFile(documentPath, "export const value = 1;\n", "utf8");
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(process.cwd(), "tests/fixtures/fake-agent.ts"),
      ],
      cwd: process.cwd(),
      readOnly: false,
      additionalDirectories: [directory],
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "nes/start", requestId: "nes-start" }));
      const started = await waitFor(socket, "acp/nes_started");
      if (started.type !== "acp/nes_started") throw new Error("unreachable");
      bridge.receive(command({
        type: "document/open",
        requestId: "doc-open",
        sessionId: started.response.sessionId,
        path: documentPath,
        languageId: "typescript",
      }));
      const opened = await waitFor(socket, "acp/document_opened");
      if (opened.type !== "acp/document_opened") throw new Error("unreachable");

      const restoreFocus = failNextAgentNotification(
        bridge,
        "document/didFocus",
        "injected focus failure",
      );
      bridge.receive(command({
        type: "document/focus",
        sessionId: started.response.sessionId,
        uri: opened.document.uri,
        position: { line: 0, character: 7 },
        visibleRange: {
          start: { line: 0, character: 0 },
          end: { line: 1, character: 0 },
        },
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.operation === "document/focus" &&
        item.message === "injected focus failure"
      );
      restoreFocus();

      bridge.receive(command({
        type: "nes/suggest",
        requestId: "nes-suggest-after-focus-failure",
        sessionId: started.response.sessionId,
        uri: opened.document.uri,
        position: { line: 0, character: 0 },
        triggerKind: "manual",
      }));
      const suggestion = await waitForMatching(socket, (item) =>
        item.type === "acp/nes_suggestions" &&
        item.requestId === "nes-suggest-after-focus-failure"
      );
      if (suggestion.type !== "acp/nes_suggestions") throw new Error("unreachable");
      expect(suggestion.response._meta?.receivedContext).toMatchObject({
        openFiles: [{
          uri: opened.document.uri,
          languageId: "typescript",
        }],
      });
      expect(suggestion.response._meta?.receivedContext).not.toHaveProperty("userActions");
      expect(suggestion.response._meta?.receivedContext).not.toHaveProperty(
        "openFiles.0.visibleRange",
      );

      const restoreAccept = failNextAgentNotification(
        bridge,
        "nes/accept",
        "injected accept failure",
      );
      const acceptedText = "/* NES */export const value = 1;\n";
      bridge.receive(command({
        type: "nes/accept",
        requestId: "nes-accept-failed",
        sessionId: started.response.sessionId,
        suggestionId: "nes-edit-1",
        text: acceptedText,
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "nes-accept-failed" &&
        item.message === "injected accept failure"
      );
      expect(socket.events.some((item) =>
        item.type === "acp/document_changed" && item.requestId === "nes-accept-failed"
      )).toBe(false);
      expect(socket.events.some((item) =>
        item.type === "acp/nes_suggestion_resolved" &&
        item.requestId === "nes-accept-failed" &&
        item.suggestionId === "nes-edit-1"
      )).toBe(false);
      restoreAccept();

      const recordedNotifications = recordAgentNotifications(bridge);
      bridge.receive(command({
        type: "nes/accept",
        requestId: "nes-accept-retry",
        sessionId: started.response.sessionId,
        suggestionId: "nes-edit-1",
        text: acceptedText,
      }));
      const changed = await waitForMatching(socket, (item) =>
        item.type === "acp/document_changed" && item.requestId === "nes-accept-retry"
      );
      expect(changed.type === "acp/document_changed" ? changed.document : undefined)
        .toMatchObject({ version: 2, text: acceptedText });
      await waitForMatching(socket, (item) =>
        item.type === "acp/nes_suggestion_resolved" &&
        item.requestId === "nes-accept-retry" &&
        item.suggestionId === "nes-edit-1" &&
        item.outcome === "accepted"
      );
      expect(recordedNotifications.methods).toEqual([
        "nes/accept",
        "document/didChange",
      ]);
      recordedNotifications.restore();

      bridge.receive(command({
        type: "nes/suggest",
        requestId: "nes-suggest-after-accept-retry",
        sessionId: started.response.sessionId,
        uri: opened.document.uri,
        position: { line: 0, character: 0 },
        triggerKind: "manual",
      }));
      const afterRetry = await waitForMatching(socket, (item) =>
        item.type === "acp/nes_suggestions" &&
        item.requestId === "nes-suggest-after-accept-retry"
      );
      if (afterRetry.type !== "acp/nes_suggestions") throw new Error("unreachable");
      const observedNesEvents = afterRetry.response._meta?.observedNesEvents;
      expect(Array.isArray(observedNesEvents) ? observedNesEvents : undefined)
        .toEqual(expect.arrayContaining(["accept:nes-edit-1", "didChange:2"]));
    } finally {
      bridge.close();
      await rm(directory, { recursive: true, force: true });
    }
  }, 10_000);

  it("runs a negotiated NES document and suggestion lifecycle", async () => {
    const directory = await mkdtemp(join(tmpdir(), "attyd-nes-"));
    const documentPath = join(directory, "sample.ts");
    await writeFile(documentPath, "export const value = 1;\n", "utf8");
    const socket = new TestSocket();
    const bridge = new AcpBridge(socket as unknown as WebSocket, {
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      cwd: process.cwd(),
      readOnly: false,
      additionalDirectories: [directory],
    });

    try {
      await bridge.start();
      bridge.receive(command({ type: "nes/start", requestId: "nes-start" }));
      bridge.receive(command({ type: "nes/start", requestId: "nes-start-duplicate" }));
      const started = await waitFor(socket, "acp/nes_started");
      if (started.type !== "acp/nes_started") throw new Error("unreachable");
      expect(started.response.sessionId).toBe("nes-session");
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "nes-start-duplicate" &&
        item.message.includes("already"),
      );

      bridge.receive(command({
        type: "document/open",
        requestId: "doc-open",
        sessionId: started.response.sessionId,
        path: documentPath,
        languageId: "typescript",
      }));
      const openedEvent = await waitForMatching(socket, (item) =>
        item.type === "acp/document_opened" && item.requestId === "doc-open",
      );
      if (openedEvent.type !== "acp/document_opened") throw new Error("unreachable");
      expect(openedEvent.document).toMatchObject({ version: 1, text: "export const value = 1;\n" });
      expect(openedEvent.notification).toMatchObject({ version: 1, languageId: "typescript" });

      bridge.receive(command({
        type: "document/change",
        requestId: "doc-change",
        sessionId: started.response.sessionId,
        uri: openedEvent.document.uri,
        text: "export const value = 2;\n",
      }));
      bridge.receive(command({
        type: "document/change",
        requestId: "doc-change-fast",
        sessionId: started.response.sessionId,
        uri: openedEvent.document.uri,
        text: "export const value = 3;\n",
      }));
      const changedEvent = await waitForMatching(socket, (item) =>
        item.type === "acp/document_changed" && item.requestId === "doc-change",
      );
      if (changedEvent.type !== "acp/document_changed") throw new Error("unreachable");
      expect(changedEvent.document.version).toBe(2);
      expect(changedEvent.notification?.contentChanges[0]).toMatchObject({
        range: {
          start: { line: 0, character: 21 },
          end: { line: 0, character: 22 },
        },
        text: "2",
      });
      const fastChangedEvent = await waitForMatching(socket, (item) =>
        item.type === "acp/document_changed" && item.requestId === "doc-change-fast",
      );
      if (fastChangedEvent.type !== "acp/document_changed") throw new Error("unreachable");
      expect(fastChangedEvent.document).toMatchObject({
        version: 3,
        text: "export const value = 3;\n",
      });
      expect(fastChangedEvent.notification?.contentChanges[0]).toMatchObject({ text: "3" });

      bridge.receive(command({
        type: "nes/suggest",
        requestId: "nes-suggest",
        sessionId: started.response.sessionId,
        uri: openedEvent.document.uri,
        position: { line: 0, character: 0 },
        triggerKind: "automatic",
      }));
      const suggestionsEvent = await waitForMatching(socket, (item) =>
        item.type === "acp/nes_suggestions" && item.requestId === "nes-suggest",
      );
      if (suggestionsEvent.type !== "acp/nes_suggestions") throw new Error("unreachable");
      expect(suggestionsEvent.response.suggestions.map(({ kind }) => kind)).toEqual(["edit", "jump"]);
      expect(suggestionsEvent.response._meta).toMatchObject({
        receivedTriggerKind: "automatic",
        receivedContext: {
          recentFiles: [expect.objectContaining({ text: "export const value = 3;\n" })],
          editHistory: [
            expect.objectContaining({ diff: expect.stringContaining("-export const value = 1;") }),
            expect.objectContaining({ diff: expect.stringContaining("+export const value = 3;") }),
          ],
          userActions: [
            expect.objectContaining({ action: "replace", position: { line: 0, character: 22 } }),
            expect.objectContaining({ action: "replace", position: { line: 0, character: 22 } }),
          ],
          openFiles: [expect.objectContaining({ languageId: "typescript" })],
        },
      });

      bridge.receive(command({
        type: "document/change",
        requestId: "doc-stale-change",
        sessionId: started.response.sessionId,
        uri: openedEvent.document.uri,
        text: "export const value = 4;\n",
      }));
      for (const suggestionId of ["nes-edit-1", "nes-jump-1"]) {
        await waitForMatching(socket, (item) =>
          item.type === "acp/nes_suggestion_resolved" &&
          item.requestId === "doc-stale-change" &&
          item.suggestionId === suggestionId &&
          item.outcome === "rejected" &&
          item.reason === "replaced",
        );
      }
      await waitForMatching(socket, (item) =>
        item.type === "acp/document_changed" && item.requestId === "doc-stale-change",
      );
      bridge.receive(command({
        type: "nes/accept",
        requestId: "nes-accept-stale",
        sessionId: started.response.sessionId,
        suggestionId: "nes-edit-1",
        text: "/* NES */export const value = 3;\n",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "nes-accept-stale" &&
        item.message.includes("no longer pending"),
      );

      bridge.receive(command({
        type: "nes/suggest",
        requestId: "nes-suggest-refreshed",
        sessionId: started.response.sessionId,
        uri: openedEvent.document.uri,
        position: { line: 0, character: 0 },
        triggerKind: "manual",
      }));
      const refreshedSuggestions = await waitForMatching(socket, (item) =>
        item.type === "acp/nes_suggestions" && item.requestId === "nes-suggest-refreshed",
      );
      if (refreshedSuggestions.type !== "acp/nes_suggestions") throw new Error("unreachable");
      expect(refreshedSuggestions.response._meta).toMatchObject({
        observedRejects: [
          { sessionId: started.response.sessionId, id: "nes-edit-1", reason: "replaced" },
          { sessionId: started.response.sessionId, id: "nes-jump-1", reason: "replaced" },
        ],
      });

      bridge.receive(command({
        type: "nes/accept",
        requestId: "nes-accept-wrong",
        sessionId: started.response.sessionId,
        suggestionId: "nes-edit-1",
        text: "wrong",
      }));
      await waitForMatching(socket, (item) =>
        item.type === "bridge/error" &&
        item.requestId === "nes-accept-wrong" &&
        item.message.includes("does not match"),
      );

      const acceptedText = "/* NES */export const value = 4;\n";
      const recordedNotifications = recordAgentNotifications(bridge);
      bridge.receive(command({
        type: "nes/accept",
        requestId: "nes-accept",
        sessionId: started.response.sessionId,
        suggestionId: "nes-edit-1",
        text: acceptedText,
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/document_changed" && item.requestId === "nes-accept",
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/nes_suggestion_resolved" &&
        item.requestId === "nes-accept" &&
        item.suggestionId === "nes-jump-1" &&
        item.outcome === "rejected" &&
        item.reason === "replaced",
      );
      await waitForMatching(socket, (item) =>
        item.type === "acp/nes_suggestion_resolved" &&
        item.suggestionId === "nes-edit-1" &&
        item.outcome === "accepted",
      );
      expect(recordedNotifications.methods).toEqual([
        "nes/reject",
        "nes/accept",
        "document/didChange",
      ]);
      recordedNotifications.restore();

      bridge.receive(command({
        type: "nes/suggest",
        requestId: "nes-suggest-after-accept",
        sessionId: started.response.sessionId,
        uri: openedEvent.document.uri,
        position: { line: 0, character: 0 },
        triggerKind: "manual",
      }));
      const afterAccept = await waitForMatching(socket, (item) =>
        item.type === "acp/nes_suggestions" &&
        item.requestId === "nes-suggest-after-accept"
      );
      if (afterAccept.type !== "acp/nes_suggestions") throw new Error("unreachable");
      const observedNesEvents = afterAccept.response._meta?.observedNesEvents;
      expect(Array.isArray(observedNesEvents) ? observedNesEvents : undefined)
        .toEqual(expect.arrayContaining(["accept:nes-edit-1", "didChange:5"]));

      bridge.receive(command({
        type: "document/save",
        requestId: "doc-save",
        sessionId: started.response.sessionId,
        uri: openedEvent.document.uri,
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/document_saved" && item.requestId === "doc-save",
      );
      expect(await readFile(documentPath, "utf8")).toBe(acceptedText);

      bridge.receive(command({
        type: "document/close",
        requestId: "doc-close",
        sessionId: started.response.sessionId,
        uri: openedEvent.document.uri,
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/document_closed" && item.requestId === "doc-close",
      );
      bridge.receive(command({
        type: "nes/close",
        requestId: "nes-close",
        sessionId: started.response.sessionId,
      }));
      await waitForMatching(socket, (item) =>
        item.type === "acp/nes_closed" && item.requestId === "nes-close",
      );
    } finally {
      bridge.close();
      await rm(directory, { recursive: true, force: true });
    }
  }, 10_000);
});

function command(value: ClientCommand): string {
  return JSON.stringify(value);
}

async function waitFor<T extends ServerEvent["type"]>(socket: TestSocket, type: T): Promise<Extract<ServerEvent, { type: T }>> {
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    const event = socket.events.find((item) => item.type === type);
    if (event) return event as Extract<ServerEvent, { type: T }>;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`Timed out waiting for ${type}: ${JSON.stringify(socket.events)}`);
}

async function waitForMatching(
  socket: TestSocket,
  predicate: (event: ServerEvent) => boolean,
): Promise<ServerEvent> {
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    const event = socket.events.find(predicate);
    if (event) return event;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`Timed out waiting for event: ${JSON.stringify(socket.events)}`);
}

function failNextAgentNotification(
  bridge: AcpBridge,
  method: string,
  message: string,
): () => void {
  type Notify = (method: string, params?: unknown) => Promise<void>;
  const connection = (bridge as unknown as {
    connection?: { agent: { notify: Notify } };
  }).connection;
  if (!connection) throw new Error("ACP test bridge has no connection");
  const agent = connection.agent;
  const original = agent.notify.bind(agent);
  let pending = true;
  agent.notify = async (candidate, params) => {
    if (pending && candidate === method) {
      pending = false;
      throw new Error(message);
    }
    await original(candidate, params);
  };
  return () => {
    agent.notify = original;
  };
}

function recordAgentNotifications(bridge: AcpBridge): {
  methods: string[];
  restore: () => void;
} {
  type Notify = (method: string, params?: unknown) => Promise<void>;
  const connection = (bridge as unknown as {
    connection?: { agent: { notify: Notify } };
  }).connection;
  if (!connection) throw new Error("ACP test bridge has no connection");
  const agent = connection.agent;
  const original = agent.notify.bind(agent);
  const methods: string[] = [];
  agent.notify = async (method, params) => {
    methods.push(method);
    await original(method, params);
  };
  return {
    methods,
    restore: () => {
      agent.notify = original;
    },
  };
}
