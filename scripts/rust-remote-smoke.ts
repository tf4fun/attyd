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

interface RuntimeView {
  connected: boolean;
  phase?: { phase?: string } | null;
}

interface SessionView {
  bridgeEpoch: string;
  sessionIncarnation: number;
  historyRevision: string | null;
  phase: string;
  timeline: unknown[];
  activeTurn: { operationId: string; prompt: unknown[]; cancelRequested?: boolean; terminal?: unknown } | null;
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

type RecordEvent = (event: string, details?: unknown) => void;

interface EventObserver {
  close(): Promise<void>;
}

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
  let observer: EventObserver | undefined;
  let globalObserver: EventObserver | undefined;
  let testFailed = false;

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
    globalObserver = await observeEvents(`${origin}/api/v1/events`, record);

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

    observer = await observeSession(origin, created, record);

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
    // Creating another thread navigates the browser to that session. Keep
    // consuming its events just as EventSource does, while the first turn runs.
    await observer.close();
    observer = await observeSession(origin, concurrentCreated, record);
    record("concurrent.turn.submit", { sessionId: concurrentCreated.sessionId });
    const concurrentTurn = await postTurn(
      origin,
      concurrentCreated.sessionId,
      concurrentCreated.view.historyRevision,
      `${transport}-prompt-concurrent`,
      "run alongside the first session",
    );
    record("concurrent.turn.accepted", concurrentTurn);
    await waitUntil(
      () => concurrentPromptReceived,
      `${transport} concurrent session prompt`,
    );
    await waitForValue(
      () => getSession(origin, concurrentCreated.sessionId),
      (view) => view.phase === "ready" && view.activeTurn == null,
      `${transport} concurrent turn reconciliation`,
    );

    await observer.close();
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
    observer = await observeSession(origin, { ...created, view: restored }, record);

    await postJson(
      `${origin}/api/v1/sessions/${encodeURIComponent(created.sessionId)}/turns/${encodeURIComponent(firstTurn.operationId)}/cancel`,
      undefined,
    );
    await waitUntil(
      () => cancellationObserved,
      `${transport} explicit cancellation after reconnect`,
    );
    const cancelling = await getSession(origin, created.sessionId);
    assert.equal(cancelling.phase, "running");
    assert.equal(cancelling.activeTurn?.cancelRequested, true);
    assert.equal(cancelling.activeTurn?.terminal, null);
    assert.equal(cancelling.activeTurn?.operationId, firstTurn.operationId);
    finishPrompt?.();
    finishPrompt = undefined;
    await waitForValue(
      () => getSession(origin, created.sessionId),
      (view) => view.phase === "ready" && view.activeTurn == null,
      `${transport} cancelled turn reconciliation`,
    );

    // Remote workspaces belong to the Agent host, including Windows Agents
    // reached from a POSIX client. Preserve both drive-letter and UNC paths.
    for (const agentCwd of ["C:\\agent\\project", "\\\\agent-host\\workspace\\project"]) {
      const windowsSession = await postJson<CreatedSession>(
        `${origin}/api/v1/sessions`,
        { cwd: agentCwd },
        201,
      );
      assert.equal(windowsSession.cwd, agentCwd);
      assert.equal(createdCwd, agentCwd, "the Agent must receive the original remote path");
      assert.equal((await getSession(origin, windowsSession.sessionId)).phase, "ready");
    }
  } catch (error) {
    testFailed = true;
    record("test.failed", String(error));
    // Preserve both sides before teardown cancels the outstanding prompt. A 202
    // only confirms admission; it does not prove the ACP request reached the Agent.
    const snapshots = await Promise.allSettled([
      "/api/v1/runtime",
      `/api/v1/sessions/${transport}-session-1`,
      `/api/v1/sessions/${transport}-session-2`,
    ].map(async (path) => {
      const response = await fetch(`${origin}${path}`, { signal: AbortSignal.timeout(1_000) });
      return { path, status: response.status, body: await response.json() };
    }));
    console.error(JSON.stringify({
      transport,
      error: String(error),
      promptStarted,
      concurrentPromptReceived,
      cancellationObserved,
      snapshots: snapshots.map((result) => result.status === "fulfilled"
        ? result.value
        : { error: String(result.reason) }),
      trace,
      host: host.diagnostics(),
    }, null, 2));
    throw error;
  } finally {
    const closed = await Promise.allSettled([observer?.close(), globalObserver?.close()]);
    finishPrompt?.();
    await host.close();
    await remote.close();
    for (const result of closed) {
      if (result.status !== "rejected") continue;
      if (!testFailed) throw result.reason;
      console.error("Browser event cleanup failed:", result.reason);
    }
  }
}

console.log("Rust remote ACP smoke passed (HTTP/SSE and WebSocket Agent transports)");

function observeSession(origin: string, session: CreatedSession, record: RecordEvent): Promise<EventObserver> {
  const query = new URLSearchParams({
    expectedEpoch: session.view.bridgeEpoch,
    expectedIncarnation: String(session.view.sessionIncarnation),
  });
  return observeEvents(
    `${origin}/api/v1/sessions/${encodeURIComponent(session.sessionId)}/events?${query}`,
    record,
  );
}

async function observeEvents(url: string, record: RecordEvent): Promise<EventObserver> {
  const controller = new AbortController();
  const startup = setTimeout(() => controller.abort(), 10_000);
  let reader: ReadableStreamDefaultReader<Uint8Array>;
  try {
    const response = await fetch(url, { signal: controller.signal });
    assert.equal(response.status, 200, `${url} returned ${response.status}`);
    assert.ok(response.body);
    reader = response.body.getReader();
    assert.equal((await reader.read()).done, false, `${url} ended before its initial event`);
  } catch (error) {
    controller.abort();
    throw error;
  } finally {
    clearTimeout(startup);
  }
  let failure: unknown;
  const drained = (async () => {
    try {
      while (!(await reader.read()).done) { /* Continuously consume browser events. */ }
      if (!controller.signal.aborted) throw new Error(`${url} ended unexpectedly`);
    } catch (error) {
      if (!controller.signal.aborted) {
        failure = error;
        record("browser.events.failed", { url, error: String(error) });
      }
    } finally {
      reader.releaseLock();
    }
  })();
  return {
    async close() {
      controller.abort();
      await drained;
      if (failure != null) throw failure;
    },
  };
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
