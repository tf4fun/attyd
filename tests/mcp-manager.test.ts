import { join } from "node:path";
import * as acp from "@agentclientprotocol/sdk";
import { afterEach, describe, expect, it } from "vitest";
import {
  McpManager,
  parseConnectMcpRequest,
  parseDisconnectMcpRequest,
  parseMessageMcp,
  type McpActivity,
} from "../server/mcp-manager";

const managers: McpManager[] = [];

afterEach(() => {
  for (const manager of managers.splice(0)) manager.close();
});

describe("ACP-transport MCP manager", () => {
  it("routes requests, errors, notifications, and server-initiated messages", async () => {
    const activities: McpActivity[] = [];
    const notifications: acp.MessageMcpNotification[] = [];
    const stderr: string[] = [];
    const manager = createManager({
      activities,
      notifications,
      stderr,
      onServerRequest: async (request) => ({ roots: [{ uri: "file:///workspace" }], request }),
    });
    const { connectionId } = await manager.connect({ serverId: "fixture" });

    await expect(manager.message({
      connectionId,
      method: "echo",
      params: { hello: "world" },
    })).resolves.toEqual({ hello: "world" });
    await expect(manager.message({ connectionId, method: "fail" })).rejects.toMatchObject({
      code: -32042,
      message: "deliberate MCP failure",
      data: { fixture: true },
    });
    await expect(manager.message({ connectionId, method: "resultAndError" })).rejects.toMatchObject({
      code: -32603,
      message: "MCP response contains both result and error",
    });

    const slow = manager.message({
      connectionId,
      method: "delayed",
      params: { delay: 30, value: "slow" },
    });
    const fast = manager.message({
      connectionId,
      method: "delayed",
      params: { delay: 0, value: "fast" },
    });
    await expect(Promise.all([slow, fast])).resolves.toEqual(["slow", "fast"]);

    const roundTrip = await manager.message({ connectionId, method: "serverRoundTrip" });
    expect(roundTrip).toMatchObject({
      clientResult: {
        roots: [{ uri: "file:///workspace" }],
        request: { connectionId, method: "roots/list" },
      },
    });
    expect(notifications).toContainEqual({
      connectionId,
      method: "notifications/progress",
      params: { progressToken: "fixture", progress: 0.5 },
    });
    expect(activities).toEqual(expect.arrayContaining([
      expect.objectContaining({ direction: "agent-to-server", method: "echo", kind: "request" }),
      expect.objectContaining({
        direction: "server-to-agent",
        method: "echo",
        kind: "response",
        result: { hello: "world" },
      }),
      expect.objectContaining({
        direction: "server-to-agent",
        method: "fail",
        kind: "response",
        error: expect.objectContaining({ code: -32042, message: "deliberate MCP failure" }),
      }),
      expect.objectContaining({ direction: "server-to-agent", method: "roots/list", kind: "request" }),
      expect.objectContaining({ direction: "agent-to-server", method: "roots/list", kind: "response" }),
      expect.objectContaining({ direction: "server-to-agent", method: "notifications/progress", kind: "notification" }),
    ]));

    await expect(manager.message({ connectionId, method: "duplicateResponse" })).resolves.toBe("first");
    await waitFor(() => stderr.some((chunk) => chunk.includes("Unknown response id")));
    await expect(manager.notify({
      connectionId,
      method: "notifications/initialized",
    })).resolves.toBeUndefined();
  }, 10_000);

  it("isolates connections and rejects unknown or disconnected IDs", async () => {
    const manager = createManager();
    await expect(manager.connect({ serverId: "invented" })).rejects.toMatchObject({ code: -32002 });

    const first = await manager.connect({ serverId: "fixture" });
    const second = await manager.connect({ serverId: "fixture" });
    expect(first.connectionId).not.toBe(second.connectionId);

    expect(manager.disconnect(first)).toEqual({});
    await expect(manager.message({
      connectionId: first.connectionId,
      method: "echo",
    })).rejects.toMatchObject({ code: -32002 });
    await expect(manager.message({
      connectionId: second.connectionId,
      method: "echo",
      params: { connection: "second" },
    })).resolves.toEqual({ connection: "second" });
    expect(() => manager.disconnect(first)).toThrow("Unknown MCP connection");
  }, 10_000);

  it("cancels and disconnects pending requests without leaking completions", async () => {
    const manager = createManager();
    const connectController = new AbortController();
    const cancelledConnect = manager.connect(
      { serverId: "fixture" },
      connectController.signal,
    );
    connectController.abort();
    await expect(cancelledConnect).rejects.toMatchObject({ code: -32800 });

    const first = await manager.connect({ serverId: "fixture" });
    const controller = new AbortController();
    const cancelled = manager.message(
      { connectionId: first.connectionId, method: "never" },
      controller.signal,
    );
    controller.abort();
    await expect(cancelled).rejects.toMatchObject({ code: -32800 });

    const pending = manager.message({ connectionId: first.connectionId, method: "never" });
    manager.disconnect(first);
    await expect(pending).rejects.toThrow("disconnected");
  }, 10_000);

  it("enforces provider and per-connection pending-request bounds", async () => {
    expect(() => new McpManager({
      cwd: process.cwd(),
      providers: [provider(), provider()],
      onServerRequest: async () => ({}),
      onServerNotification: () => {},
    })).toThrow("Duplicate ACP MCP serverId");

    const manager = createManager();
    const connected = await manager.connect({ serverId: "fixture" });
    const pending = Array.from({ length: 128 }, () =>
      manager.message({ connectionId: connected.connectionId, method: "never" })
        .catch((error: unknown) => error),
    );
    await expect(manager.message({
      connectionId: connected.connectionId,
      method: "never",
    })).rejects.toThrow("128 MCP requests");
    manager.disconnect(connected);
    expect(await Promise.all(pending)).toHaveLength(128);
  }, 10_000);

  it("rejects pending work with the provider name and exit status", async () => {
    const manager = createManager();
    const connected = await manager.connect({ serverId: "fixture" });
    await expect(manager.message({
      connectionId: connected.connectionId,
      method: "exit",
    })).rejects.toThrow("MCP server fixture exited (23)");
    await expect(manager.message({
      connectionId: connected.connectionId,
      method: "echo",
    })).rejects.toMatchObject({ code: -32002 });

    const recovered = await manager.connect({ serverId: "fixture" });
    await expect(manager.message({
      connectionId: recovered.connectionId,
      method: "echo",
      params: { recovered: true },
    })).resolves.toEqual({ recovered: true });
  }, 10_000);

  it("strictly parses generated ACP transport params", () => {
    expect(parseConnectMcpRequest({ serverId: "fixture", _meta: { trace: true } })).toEqual({
      serverId: "fixture",
      _meta: { trace: true },
    });
    expect(parseDisconnectMcpRequest({ connectionId: "connection" })).toEqual({
      connectionId: "connection",
    });
    expect(parseMessageMcp({
      connectionId: "connection",
      method: "tools/call",
      params: { name: "x" },
    })).toEqual({
      connectionId: "connection",
      method: "tools/call",
      params: { name: "x" },
    });
    expect(() => parseMessageMcp({
      connectionId: "connection",
      method: "tools/call",
      params: [],
    })).toThrow("object or null");
    expect(() => parseMessageMcp({ connectionId: "", method: "x" })).toThrow("between 1");
    expect(() => parseConnectMcpRequest({ serverId: "x", _meta: [] })).toThrow("_meta");
  });
});

function createManager(overrides: {
  activities?: McpActivity[];
  notifications?: acp.MessageMcpNotification[];
  stderr?: string[];
  onServerRequest?: (request: acp.MessageMcpRequest) => Promise<unknown>;
} = {}): McpManager {
  const manager = new McpManager({
    cwd: process.cwd(),
    providers: [provider()],
    onServerRequest: overrides.onServerRequest ?? (async () => ({})),
    onServerNotification: (notification) => {
      overrides.notifications?.push(notification);
    },
    onActivity: (activity) => overrides.activities?.push(activity),
    onStderr: (chunk) => overrides.stderr?.push(chunk),
  });
  managers.push(manager);
  return manager;
}

function provider() {
  return {
    name: "fixture",
    serverId: "fixture",
    command: process.execPath,
    args: ["--import", "tsx", join(process.cwd(), "tests/fixtures/fake-mcp-server.ts")],
    env: [],
  };
}

async function waitFor(predicate: () => boolean): Promise<void> {
  const deadline = Date.now() + 2_000;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("Timed out waiting for condition");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}
