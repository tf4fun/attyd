import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import * as acp from "@agentclientprotocol/sdk";
import {
  createNodeHttpHandler,
  createNodeWebSocketUpgradeHandler,
} from "@agentclientprotocol/sdk/experimental/node";
import { AcpServer } from "@agentclientprotocol/sdk/experimental/server";
import { WebSocketServer, type WebSocket } from "ws";
import { describe, expect, it } from "vitest";
import { AcpBridge } from "../server/acp-bridge";
import type { ClientCommand, ServerEvent } from "../shared/bridge";

class TestSocket {
  readonly OPEN = 1;
  readonly readyState = 1;
  readonly events: ServerEvent[] = [];

  send(data: string): void {
    this.events.push(JSON.parse(data) as ServerEvent);
  }
}

describe("ACP remote transports", () => {
  it.each(["http", "ws"] as const)(
    "connects to an ACP Agent over %s",
    async (transport) => {
      let terminalAuthCapability: boolean | undefined;
      let filesystemCapability: unknown;
      let terminalCapability: unknown;
      let createdCwd: string | undefined;
      const agent = acp
        .agent({ name: `remote-${transport}-agent` })
        .onRequest(acp.methods.agent.initialize, ({ params }) => {
          terminalAuthCapability = params.clientCapabilities.auth?.terminal;
          filesystemCapability = params.clientCapabilities.fs;
          terminalCapability = params.clientCapabilities.terminal;
          return {
            protocolVersion: acp.PROTOCOL_VERSION,
            agentCapabilities: {},
            agentInfo: { name: `remote-${transport}-agent`, version: "1.0.0" },
          };
        })
        .onRequest(acp.methods.agent.session.new, ({ params }) => {
          createdCwd = params.cwd;
          return { sessionId: `${transport}-session` };
        });
      const remote = await startRemoteAgent(agent);
      const endpoint = `${transport === "http" ? "http" : "ws"}://127.0.0.1:${remote.port}/acp`;
      const socket = new TestSocket();
      const bridge = new AcpBridge(socket as unknown as WebSocket, {
        transport,
        command: [endpoint],
        cwd: process.cwd(),
        readOnly: false,
      });

      try {
        await bridge.start();
        expect(terminalAuthCapability).not.toBe(true);
        expect(filesystemCapability).toEqual({
          readTextFile: false,
          writeTextFile: false,
        });
        expect(terminalCapability).toBe(false);
        expect(socket.events).toContainEqual(expect.objectContaining({
          type: "bridge/hello",
          transport,
          command: [endpoint],
        }));
        expect(socket.events).toContainEqual({
          type: "bridge/phase",
          phase: "ready",
        });

        bridge.receive(command({ type: "session/new", requestId: `${transport}-missing-cwd` }));
        const missingCwd = await waitFor(socket, "bridge/error");
        expect(missingCwd).toMatchObject({
          requestId: `${transport}-missing-cwd`,
          operation: "session/new",
        });
        expect(missingCwd.message).toContain("requires an absolute Agent workspace");

        bridge.receive(command({
          type: "session/new",
          requestId: `${transport}-new`,
          cwd: "/home/agent/project",
        }));
        const created = await waitFor(socket, "acp/session_created");
        expect(createdCwd).toBe("/home/agent/project");
        expect(created).toMatchObject({
          requestId: `${transport}-new`,
          cwd: "/home/agent/project",
          response: { sessionId: `${transport}-session` },
        });
      } finally {
        bridge.close();
        await remote.close();
      }
    },
    10_000,
  );
});

async function startRemoteAgent(agent: ReturnType<typeof acp.agent>): Promise<{
  port: number;
  close: () => Promise<void>;
}> {
  const acpServer = new AcpServer({ agent });
  const httpHandler = createNodeHttpHandler(acpServer);
  const webSocketServer = new WebSocketServer({ noServer: true });
  const upgrade = createNodeWebSocketUpgradeHandler(acpServer, webSocketServer);
  const server = createServer(httpHandler);
  server.on("upgrade", upgrade);
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const { port } = server.address() as AddressInfo;
  return {
    port,
    close: async () => {
      await acpServer.close();
      webSocketServer.close();
      await new Promise<void>((resolve, reject) => {
        server.close((error) => error ? reject(error) : resolve());
      });
    },
  };
}

function command(value: ClientCommand): string {
  return JSON.stringify(value);
}

async function waitFor<T extends ServerEvent["type"]>(
  socket: TestSocket,
  type: T,
): Promise<Extract<ServerEvent, { type: T }>> {
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    const event = socket.events.find((candidate) => candidate.type === type);
    if (event) return event as Extract<ServerEvent, { type: T }>;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`Timed out waiting for ${type}`);
}
