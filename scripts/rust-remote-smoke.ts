import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { join } from "node:path";
import * as acp from "@agentclientprotocol/sdk";
import {
  createNodeHttpHandler,
  createNodeWebSocketUpgradeHandler,
} from "@agentclientprotocol/sdk/experimental/node";
import { AcpServer } from "@agentclientprotocol/sdk/experimental/server";
import WebSocket, { WebSocketServer } from "ws";
import type { ServerEvent } from "../shared/bridge.js";

for (const transport of ["http", "ws"] as const) {
  let initializedCapabilities: acp.ClientCapabilities | undefined;
  let createdCwd: string | undefined;
  const agent = acp
    .agent({ name: `rust-remote-${transport}` })
    .onRequest(acp.methods.agent.initialize, ({ params }) => {
      initializedCapabilities = params.clientCapabilities;
      return {
        protocolVersion: acp.PROTOCOL_VERSION,
        agentCapabilities: {},
        agentInfo: { name: `rust-remote-${transport}`, version: "1.0.0" },
      };
    })
    .onRequest(acp.methods.agent.session.new, ({ params }) => {
      createdCwd = params.cwd;
      return { sessionId: `${transport}-session` };
    });
  const remote = await startRemoteAgent(agent);
  const endpoint = `${transport === "http" ? "http" : "ws"}://127.0.0.1:${remote.port}/acp`;
  const host = await startRustHost(transport, endpoint);
  const socket = new WebSocket(`ws://127.0.0.1:${host.port}/ws`);
  const events: ServerEvent[] = [];
  socket.on("message", (data) => events.push(JSON.parse(data.toString()) as ServerEvent));

  try {
    await new Promise<void>((resolve, reject) => {
      socket.once("open", () => resolve());
      socket.once("error", reject);
    });
    await waitFor(events, (event) => event.type === "acp/initialized");
    await waitFor(events, (event) => event.type === "bridge/phase" && event.phase === "ready");
    assert.equal(initializedCapabilities?.terminal, false);
    assert.deepEqual(initializedCapabilities?.fs, {
      readTextFile: false,
      writeTextFile: false,
    });
    assert.equal(initializedCapabilities?.auth?.terminal, false);

    socket.send(JSON.stringify({
      type: "session/new",
      requestId: `${transport}-missing-cwd`,
    }));
    const error = await waitFor(events, (event) =>
      event.type === "bridge/error" && event.requestId === `${transport}-missing-cwd`
    );
    assert.equal(error.type, "bridge/error");
    assert.equal(error.code, -32_602);
    assert.match(JSON.stringify(error.data), /requires an absolute Agent workspace/u);

    socket.send(JSON.stringify({
      type: "session/new",
      requestId: `${transport}-new`,
      cwd: "/home/agent/project",
    }));
    const created = await waitFor(events, (event) =>
      event.type === "acp/session_created" && event.requestId === `${transport}-new`
    );
    assert.equal(created.type, "acp/session_created");
    assert.equal(created.cwd, "/home/agent/project");
    assert.equal(created.response.sessionId, `${transport}-session`);
    assert.equal(createdCwd, "/home/agent/project");
  } finally {
    socket.terminate();
    await host.close();
    await remote.close();
  }
}

console.log("Rust remote ACP smoke passed (HTTP/SSE and WebSocket)");

async function startRemoteAgent(agent: ReturnType<typeof acp.agent>): Promise<{
  port: number;
  close(): Promise<void>;
}> {
  const acpServer = new AcpServer({ agent });
  const webSocketServer = new WebSocketServer({ noServer: true });
  const server = createServer(createNodeHttpHandler(acpServer));
  server.on("upgrade", createNodeWebSocketUpgradeHandler(acpServer, webSocketServer));
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  return {
    port: (server.address() as AddressInfo).port,
    async close() {
      await acpServer.close();
      webSocketServer.close();
      await new Promise<void>((resolve, reject) => {
        server.close((error) => error ? reject(error) : resolve());
      });
    },
  };
}

async function startRustHost(
  transport: "http" | "ws",
  endpoint: string,
): Promise<{ port: number; close(): Promise<void> }> {
  const rustBinary = process.env.ATTYD_RUST_BINARY ?? join(process.cwd(), "target/debug/attyd");
  const child = spawn(rustBinary, [
    "--host",
    "127.0.0.1",
    "--port",
    "0",
    "--transport",
    transport,
    "--",
    endpoint,
  ], { stdio: ["ignore", "pipe", "pipe"] });
  let stdout = "";
  let stderr = "";
  const port = await new Promise<number>((resolve, reject) => {
    const timeout = setTimeout(() => {
      reject(new Error(`Rust remote host did not start; stdout=${stdout} stderr=${stderr}`));
    }, 10_000);
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString();
      const match = stdout.match(/attyd listening on http:\/\/127\.0\.0\.1:(\d+)/u);
      if (!match) return;
      clearTimeout(timeout);
      resolve(Number(match[1]));
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString();
    });
    child.once("exit", (code, signal) => {
      clearTimeout(timeout);
      reject(new Error(
        `Rust remote host exited (code=${String(code)}, signal=${String(signal)}); ` +
        `stdout=${stdout} stderr=${stderr}`,
      ));
    });
  });
  return {
    port,
    async close() {
      child.kill("SIGTERM");
      await Promise.race([
        new Promise<void>((resolve) => child.once("exit", () => resolve())),
        new Promise<void>((resolve) => setTimeout(resolve, 2_000)),
      ]);
      if (child.exitCode == null && child.signalCode == null) child.kill("SIGKILL");
    },
  };
}

async function waitFor(
  events: ServerEvent[],
  predicate: (event: ServerEvent) => boolean,
): Promise<ServerEvent> {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    const event = events.find(predicate);
    if (event) return event;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`Timed out waiting for remote event: ${JSON.stringify(events)}`);
}
