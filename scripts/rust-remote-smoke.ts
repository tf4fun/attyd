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
  let sessionCount = 0;
  let promptStarted = false;
  let concurrentPromptCompleted = false;
  let cancellationObserved = false;
  let finishPrompt: (() => void) | undefined;
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
      sessionCount += 1;
      return { sessionId: `${transport}-session-${sessionCount}` };
    })
    .onRequest(acp.methods.agent.session.prompt, async ({ params }) => {
      if (params.sessionId === `${transport}-session-2`) {
        concurrentPromptCompleted = true;
        return { stopReason: "end_turn" };
      }
      promptStarted = true;
      await new Promise<void>((resolve) => { finishPrompt = resolve; });
      return { stopReason: "cancelled" };
    })
    .onNotification(acp.methods.agent.session.cancel, () => {
      cancellationObserved = true;
      finishPrompt?.();
      finishPrompt = undefined;
    });
  const remote = await startRemoteAgent(agent);
  const endpoint = `${transport === "http" ? "http" : "ws"}://127.0.0.1:${remote.port}/acp`;
  const host = await startRustHost(transport, endpoint);
  let socket = new WebSocket(`ws://127.0.0.1:${host.port}/ws`);
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
    assert.equal(created.response.sessionId, `${transport}-session-1`);
    assert.equal(createdCwd, "/home/agent/project");

    socket.send(JSON.stringify({
      type: "session/prompt",
      requestId: `${transport}-prompt`,
      sessionId: `${transport}-session-1`,
      prompt: [{ type: "text", text: "keep running while the browser reconnects" }],
    }));
    await waitUntil(() => promptStarted, `${transport} prompt to start`);

    socket.send(JSON.stringify({
      type: "session/new",
      requestId: `${transport}-new-concurrent`,
      cwd: "/home/agent/other-project",
    }));
    const concurrentCreated = await waitFor(events, (event) =>
      event.type === "acp/session_created" &&
      event.requestId === `${transport}-new-concurrent`
    );
    assert.equal(concurrentCreated.type, "acp/session_created");
    assert.equal(concurrentCreated.response.sessionId, `${transport}-session-2`);
    socket.send(JSON.stringify({
      type: "session/prompt",
      requestId: `${transport}-prompt-concurrent`,
      sessionId: `${transport}-session-2`,
      prompt: [{ type: "text", text: "run alongside the first session" }],
    }));
    await waitUntil(
      () => concurrentPromptCompleted,
      `${transport} concurrent session prompt`,
    );
    await waitFor(events, (event) =>
      event.type === "acp/prompt_complete" &&
      event.requestId === `${transport}-prompt-concurrent`
    );

    socket.terminate();
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.equal(cancellationObserved, false);

    socket = new WebSocket(`ws://127.0.0.1:${host.port}/ws`);
    const replayEvents: ServerEvent[] = [];
    socket.on("message", (data) => {
      replayEvents.push(JSON.parse(data.toString()) as ServerEvent);
    });
    await new Promise<void>((resolve, reject) => {
      socket.once("open", () => resolve());
      socket.once("error", reject);
    });
    const replayComplete = await waitFor(
      replayEvents,
      (event) => event.type === "bridge/runtime_replay_complete",
    );
    assert.equal(replayComplete.type, "bridge/runtime_replay_complete");
    assert.deepEqual(replayComplete.sessionIds.sort(), [
      `${transport}-session-1`,
      `${transport}-session-2`,
    ]);
    assert.ok(replayEvents.some((event) =>
      event.type === "acp/prompt_started" &&
      event.requestId === `${transport}-prompt`
    ));

    socket.send(JSON.stringify({
      type: "session/cancel",
      sessionId: `${transport}-session-1`,
    }));
    await waitUntil(
      () => cancellationObserved,
      `${transport} explicit cancellation after reconnect`,
    );
  } finally {
    if (socket.readyState !== WebSocket.CLOSED) socket.terminate();
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

async function waitUntil(predicate: () => boolean, label: string): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`Timed out waiting for ${label}`);
}
