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
import { WebSocketServer } from "ws";

interface RuntimeView {
  connected: boolean;
  phase?: { phase?: string } | null;
}

interface SessionView {
  historyRevision: string | null;
  phase: string;
  timeline: unknown[];
  activeTurn: { operationId: string; prompt: unknown[] } | null;
}

interface CreatedSession {
  sessionId: string;
  cwd?: string;
  view: SessionView;
}

interface StartedTurn {
  operationId: string;
  disposition: "accepted" | "duplicate";
}

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
  const origin = `http://127.0.0.1:${host.port}`;
  let observer: AbortController | undefined;

  try {
    await waitForValue(
      () => getJson<RuntimeView>(`${origin}/api/v1/runtime`),
      (runtime) => runtime.connected && runtime.phase?.phase === "ready",
      `${transport} runtime initialization`,
    );
    assert.equal(initializedCapabilities?.terminal, false);
    assert.deepEqual(initializedCapabilities?.fs, {
      readTextFile: false,
      writeTextFile: false,
    });
    assert.equal(initializedCapabilities?.auth?.terminal, false);

    const missingCwd = await fetch(`${origin}/api/v1/sessions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({}),
    });
    assert.equal(missingCwd.status, 409);
    assert.match(
      JSON.stringify(await missingCwd.json()),
      /requires an absolute Agent workspace/u,
    );

    const created = await postJson<CreatedSession>(
      `${origin}/api/v1/sessions`,
      { cwd: "/home/agent/project" },
      201,
    );
    assert.equal(created.cwd, "/home/agent/project");
    assert.equal(created.sessionId, `${transport}-session-1`);
    assert.equal(createdCwd, "/home/agent/project");
    assert.ok(created.view.historyRevision);

    observer = new AbortController();
    const observed = await fetch(
      `${origin}/api/v1/sessions/${encodeURIComponent(created.sessionId)}/events`,
      { signal: observer.signal },
    );
    assert.equal(observed.status, 200);
    await observed.body?.getReader().read();

    const firstTurn = await postTurn(
      origin,
      created.sessionId,
      created.view.historyRevision,
      `${transport}-prompt`,
      "keep running while the browser reconnects",
    );
    assert.equal(firstTurn.disposition, "accepted");
    await waitUntil(() => promptStarted, `${transport} prompt to start`);

    const concurrentCreated = await postJson<CreatedSession>(
      `${origin}/api/v1/sessions`,
      { cwd: "/home/agent/other-project" },
      201,
    );
    assert.equal(concurrentCreated.sessionId, `${transport}-session-2`);
    assert.ok(concurrentCreated.view.historyRevision);
    await postTurn(
      origin,
      concurrentCreated.sessionId,
      concurrentCreated.view.historyRevision,
      `${transport}-prompt-concurrent`,
      "run alongside the first session",
    );
    await waitUntil(
      () => concurrentPromptCompleted,
      `${transport} concurrent session prompt`,
    );
    await waitForValue(
      () => getSession(origin, concurrentCreated.sessionId),
      (view) => view.phase === "ready" && view.activeTurn == null,
      `${transport} concurrent turn reconciliation`,
    );

    observer.abort();
    observer = undefined;
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.equal(cancellationObserved, false);

    const restored = await waitForValue(
      () => getSession(origin, created.sessionId),
      (view) => view.phase === "running" && view.activeTurn != null,
      `${transport} active turn restoration`,
    );
    assert.equal(restored.activeTurn?.operationId, firstTurn.operationId);
    assert.deepEqual(restored.activeTurn?.prompt, [{
      type: "text",
      text: "keep running while the browser reconnects",
    }]);

    await postJson(
      `${origin}/api/v1/sessions/${encodeURIComponent(created.sessionId)}/turns/${encodeURIComponent(firstTurn.operationId)}/cancel`,
      undefined,
    );
    await waitUntil(
      () => cancellationObserved,
      `${transport} explicit cancellation after reconnect`,
    );
    await waitForValue(
      () => getSession(origin, created.sessionId),
      (view) => view.phase === "ready" && view.activeTurn == null,
      `${transport} cancelled turn reconciliation`,
    );
  } finally {
    observer?.abort();
    await host.close();
    await remote.close();
  }
}

console.log("Rust remote ACP smoke passed (HTTP/SSE and WebSocket Agent transports)");

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

async function getJson<T>(url: string): Promise<T> {
  const response = await fetch(url);
  assert.equal(response.status, 200, `${url} returned ${response.status}`);
  return await response.json() as T;
}

async function postJson<T = unknown>(
  url: string,
  body?: unknown,
  expectedStatus = 200,
  headers: Record<string, string> = {},
): Promise<T> {
  const response = await fetch(url, {
    method: "POST",
    headers: body === undefined
      ? headers
      : { "content-type": "application/json", ...headers },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  assert.equal(
    response.status,
    expectedStatus,
    `${url} returned ${response.status}: ${text}`,
  );
  return (text === "" ? undefined : JSON.parse(text)) as T;
}

async function getSession(origin: string, sessionId: string): Promise<SessionView> {
  return await getJson<SessionView>(
    `${origin}/api/v1/sessions/${encodeURIComponent(sessionId)}`,
  );
}

async function postTurn(
  origin: string,
  sessionId: string,
  historyRevision: string | null,
  intentId: string,
  text: string,
): Promise<StartedTurn> {
  assert.ok(historyRevision, "a new session must expose a history revision");
  return await postJson<StartedTurn>(
    `${origin}/api/v1/sessions/${encodeURIComponent(sessionId)}/turns`,
    { prompt: [{ type: "text", text }] },
    202,
    {
      "If-Match": `"${historyRevision}"`,
      "Idempotency-Key": intentId,
    },
  );
}

async function waitForValue<T>(
  read: () => Promise<T>,
  predicate: (value: T) => boolean,
  label: string,
): Promise<T> {
  const deadline = Date.now() + 10_000;
  let latest: T | undefined;
  while (Date.now() < deadline) {
    latest = await read();
    if (predicate(latest)) return latest;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`Timed out waiting for ${label}: ${JSON.stringify(latest)}`);
}

async function waitUntil(predicate: () => boolean, label: string): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`Timed out waiting for ${label}`);
}
