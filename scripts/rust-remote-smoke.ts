import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import type { AddressInfo, Socket } from "node:net";
import { join } from "node:path";
import * as acp from "@agentclientprotocol/sdk";
import {
  createNodeHttpHandler,
  createNodeWebSocketUpgradeHandler,
} from "@agentclientprotocol/sdk/experimental/node";
import { AcpServer } from "@agentclientprotocol/sdk/experimental/server";
import { WebSocketServer } from "ws";
import type { PromptResult } from "../shared/protocol.js";
import { globalLive, sessionLive, sessionScope, SmokeClient } from "./protocol-smoke-client.js";

type RecordEvent = (event: string, details?: unknown) => void;

for (const transport of ["http", "ws"] as const) {
  const started = performance.now();
  const trace: Array<{ elapsedMs: number; event: string; details?: unknown }> = [];
  const record: RecordEvent = (event, details) => {
    trace.push({ elapsedMs: Math.round(performance.now() - started), event, details });
  };
  let initializedCapabilities: acp.ClientCapabilities | undefined;
  let createdCwd: string | undefined;
  let sessionCount = 0;
  let promptStarted = false;
  let concurrentPromptReceived = false;
  let cancellationObserved = false;
  let finishPrompt: (() => void) | undefined;
  const agent = acp
    .agent({ name: `rust-remote-${transport}` })
    .onRequest(acp.methods.agent.initialize, ({ params }) => {
      record("agent.initialize");
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
      record("agent.session.new", { sessionId: `${transport}-session-${sessionCount}` });
      return { sessionId: `${transport}-session-${sessionCount}` };
    })
    .onRequest(acp.methods.agent.session.prompt, async ({ params }) => {
      record("agent.session.prompt", { sessionId: params.sessionId });
      if (params.sessionId === `${transport}-session-2`) {
        concurrentPromptReceived = true;
        return { stopReason: "end_turn" };
      }
      promptStarted = true;
      await new Promise<void>((resolve) => { finishPrompt = resolve; });
      return { stopReason: "cancelled" };
    })
    .onNotification(acp.methods.agent.session.cancel, () => {
      record("agent.session.cancel");
      cancellationObserved = true;
    });
  const remote = await startRemoteAgent(agent, record);
  const endpoint = `${transport === "http" ? "http" : "ws"}://127.0.0.1:${remote.port}/acp`;
  const host = await startRustHost(transport, endpoint);
  const origin = `http://127.0.0.1:${host.port}`;
  const client = new SmokeClient(origin);

  try {
    client.follow([{ scope: "global" }]);
    await client.until("global", (replica) => (globalLive(replica)?.agent.phase === "ready" ? true : undefined),
      `${transport} runtime initialization`);
    assert.equal(initializedCapabilities?.terminal, false);
    assert.deepEqual(initializedCapabilities?.fs, {
      readTextFile: false,
      writeTextFile: false,
    });
    assert.equal(initializedCapabilities?.auth?.terminal, false);

    const missingCwd = await client.ops.write("global", { kind: "create", args: {} as { cwd: string } });
    assert.ok(missingCwd.kind === "refused", JSON.stringify(missingCwd));
    assert.equal(missingCwd.error.code, "invalid_request");

    const created = await createSession(client, "/home/agent/project");
    assert.equal(created, `${transport}-session-1`);
    assert.equal(createdCwd, "/home/agent/project");
    const first = sessionScope(created);
    client.follow([{ scope: "global" }, { scope: first }]);
    const opened = await client.ended(first);
    assert.equal(sessionLive(client.replicas.get(first))?.cwd, "/home/agent/project");
    assert.equal(opened.kind, "open");

    const firstTurn = await prompt(client, first, "keep running while the browser reconnects");
    await waitUntil(() => promptStarted, `${transport} prompt to start`);

    const concurrent = await createSession(client, "/home/agent/other-project");
    assert.equal(concurrent, `${transport}-session-2`);
    // Creating another thread navigates the browser to that session: its
    // stream replaces the first session's while the first turn runs.
    const second = sessionScope(concurrent);
    client.follow([{ scope: "global" }, { scope: second }]);
    await client.ended(second);
    record("concurrent.turn.submit", { sessionId: concurrent });
    const concurrentTurn = await prompt(client, second, "run alongside the first session");
    record("concurrent.turn.accepted", { op: concurrentTurn });
    await waitUntil(() => concurrentPromptReceived, `${transport} concurrent session prompt`);
    assert.equal((await client.ended(second, concurrentTurn)).status, "completed");
    assert.equal(client.replicas.get(second)?.log.length, 1);

    // No stream follows the first session now; leaving it cancels nothing.
    client.follow([{ scope: "global" }]);
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.equal(cancellationObserved, false);

    // The replica held from the earlier stream stays until the new stream aligns it.
    const before = client.replicas.get(first);
    client.follow([{ scope: "global" }, { scope: first }]);
    const restored = await client.until(first,
      (replica) => (replica !== before ? sessionLive(replica)?.op : undefined), `${transport} running turn alignment`);
    assert.equal(restored.id, firstTurn);
    assert.equal(restored.status, "running");
    assert.deepEqual(restored.input, { prompt: [{
      type: "text",
      text: "keep running while the browser reconnects",
    }] });

    const cancel = await client.ops.write(first, { kind: "cancel", args: {} });
    assert.ok(cancel.kind === "accepted" && cancel.op === firstTurn, JSON.stringify(cancel));
    await waitUntil(() => cancellationObserved, `${transport} explicit cancellation after reconnect`);
    // A requested cancel ends nothing: the turn runs until the Agent answers.
    const cancelling = await client.until(first, (replica) => {
      const op = sessionLive(replica)?.op;
      return op?.cancelRequested ? op : undefined;
    }, `${transport} cancel request`);
    assert.equal(cancelling.status, "running");
    assert.equal(cancelling.id, firstTurn);
    finishPrompt?.();
    finishPrompt = undefined;
    const answered = await client.ended(first, firstTurn);
    assert.equal(answered.id, firstTurn);
    assert.equal(answered.status, "completed");
    assert.equal((answered.result as PromptResult).stopReason, "cancelled");
    assert.equal(client.replicas.get(first)?.log.length, 1);

    // Remote workspaces belong to the Agent host, including Windows Agents
    // reached from a POSIX client. Preserve both drive-letter and UNC paths.
    for (const agentCwd of ["C:\\agent\\project", "\\\\agent-host\\workspace\\project"]) {
      const windowsSession = await createSession(client, agentCwd);
      assert.equal(createdCwd, agentCwd, "the Agent must receive the original remote path");
      const summary = await client.until("global",
        (replica) => globalLive(replica)?.sessions[windowsSession], `${transport} ${agentCwd} session`);
      assert.equal(summary.cwd, agentCwd);
      assert.equal(summary.op.status, "completed");
    }
    assert.deepEqual(client.streamErrors, []);
  } catch (error) {
    record("test.failed", String(error));
    // Preserve both sides before teardown cancels the outstanding prompt. A 202
    // only confirms admission; it does not prove the ACP request reached the Agent.
    const snapshots = await Promise.allSettled([
      "global",
      sessionScope(`${transport}-session-1`),
      sessionScope(`${transport}-session-2`),
    ].map(async (scope) => {
      const path = `/api/v2/scopes/${encodeURIComponent(scope)}`;
      const response = await fetch(`${origin}${path}`, { signal: AbortSignal.timeout(1_000) });
      return { path, status: response.status, body: await response.json() };
    }));
    console.error(JSON.stringify({
      transport,
      error: String(error),
      promptStarted,
      concurrentPromptReceived,
      cancellationObserved,
      streamErrors: client.streamErrors,
      snapshots: snapshots.map((result) => result.status === "fulfilled"
        ? result.value
        : { error: String(result.reason) }),
      trace,
      host: host.diagnostics(),
    }, null, 2));
    throw error;
  } finally {
    client.close();
    finishPrompt?.();
    await host.close();
    await remote.close();
  }
}

console.log("Rust remote ACP smoke passed (HTTP/SSE and WebSocket Agent transports)");

/** Creates a session in `cwd`; `create` answers at its end with the session ID. */
async function createSession(client: SmokeClient, cwd: string): Promise<string> {
  await client.ended("global");
  const created = await client.ops.write("global", { kind: "create", args: { cwd } });
  assert.ok(created.kind === "ended" && "result" in created.outcome, JSON.stringify(created));
  await client.ended("global", created.op);
  return (created.outcome.result as { sessionId: string }).sessionId;
}

/** Starts a prompt of `text`; returns its operation ID. */
async function prompt(client: SmokeClient, scope: string, text: string): Promise<string> {
  const started = await client.ops.write(scope, { kind: "prompt", args: { prompt: [{ type: "text", text }] } });
  assert.ok(started.kind === "accepted", JSON.stringify(started));
  return started.op;
}

async function startRemoteAgent(agent: ReturnType<typeof acp.agent>, record: RecordEvent): Promise<{
  port: number;
  close(): Promise<void>;
}> {
  const acpServer = new AcpServer({ agent });
  const webSocketServer = new WebSocketServer({ noServer: true });
  const handler = createNodeHttpHandler(acpServer);
  let requestId = 0;
  let connectionId = 0;
  const connections = new WeakMap<Socket, number>();
  const server = createServer((request, response) => {
    const details = {
      requestId: ++requestId,
      connectionId: connections.get(request.socket),
      remotePort: request.socket.remotePort,
      method: request.method,
      sessionId: request.headers["acp-session-id"],
    };
    record("http.request", details);
    response.once("finish", () => {
      record("http.response.finished", { ...details, status: response.statusCode });
    });
    response.once("close", () => {
      record("http.response.closed", { ...details, status: response.statusCode });
    });
    handler(request, response);
  });
  server.on("connection", (socket) => {
    const details = { connectionId: ++connectionId, remotePort: socket.remotePort };
    connections.set(socket, details.connectionId);
    record("http.connection.open", details);
    socket.once("close", () => record("http.connection.closed", details));
  });
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
): Promise<{ port: number; diagnostics(): unknown; close(): Promise<void> }> {
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
  ], {
    stdio: ["ignore", "pipe", "pipe"],
  });
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
    diagnostics: () => ({ stdout, stderr, exitCode: child.exitCode, signalCode: child.signalCode }),
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

async function waitUntil(predicate: () => boolean, label: string): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`Timed out waiting for ${label}`);
}
