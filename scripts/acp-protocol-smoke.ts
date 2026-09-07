import assert from "node:assert/strict";
import { join } from "node:path";
import type {
  BridgeSessionView,
  CreatedSessionResult,
  RuntimeView,
} from "../web/src/lib/business-api.js";
import { startRustTestServer } from "./rust-test-server.js";

// SDK Agent -> ACP client -> MCP server, with the same REST/SSE observation used
// by the browser. The fixtures report upstream results; assertions live here.
const cwd = process.cwd();
const server = await startRustTestServer({
  cwd,
  command: [process.execPath, "--import", "tsx", join(cwd, "tests/fixtures/fake-agent.ts")],
  mcpConfig: {
    mcpServers: [{
      type: "acp",
      name: "protocol-fixture",
      serverId: "protocol-fixture",
      command: process.execPath,
      args: ["--import", "tsx", join(cwd, "tests/fixtures/fake-mcp-server.ts")],
      env: [],
    }],
  },
});
const observer = new AbortController();

try {
  const origin = `http://127.0.0.1:${server.port}`;
  await eventually(async () => {
    const runtime = await getJson<RuntimeView>(`${origin}/api/v1/runtime`);
    return runtime.connected && runtime.phase?.phase === "ready" ? runtime : undefined;
  }, "Agent initialization");
  const createdResponse = await fetch(`${origin}/api/v1/sessions`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({}),
  });
  assert.equal(createdResponse.status, 201);
  const created = await createdResponse.json() as CreatedSessionResult;
  const sessionUrl = `${origin}/api/v1/sessions/${encodeURIComponent(created.sessionId)}`;
  const events = await fetch(`${sessionUrl}/events`, { signal: observer.signal });
  assert.equal(events.status, 200);
  assert.ok(events.body);
  const reader = events.body.getReader();
  await reader.read();
  // Keep the observer registered until all turn results have been inspected.
  const drain = (async () => {
    while (!(await reader.read()).done) { /* consume session deltas */ }
  })().catch((error: unknown) => {
    if (!observer.signal.aborted) throw error;
  });

  try {
    const relay = await runFlow(sessionUrl, "mcp-flow", "mcp-result");
    assert.deepEqual(relay.initialized, {
      protocolVersion: "2025-06-18",
      capabilities: { tools: {} },
      serverInfo: { name: "fake-mcp", version: "1.0.0" },
    });
    assert.deepEqual(relay.echoed, { from: "agent" });
    assert.deepEqual(relay.failed, {
      code: -32042,
      message: "deliberate MCP failure",
      data: { fixture: true },
    });
    assert.equal(asRecord(relay.resultAndError).code, -32603);
    assert.deepEqual(relay.roundTrip, {
      clientResult: {
        roots: [{ uri: "file:///fake-agent-workspace" }],
        receivedMethod: "roots/list",
        receivedParams: { requestedBy: "fake-mcp" },
      },
    });
    assert.deepEqual(relay.agentNotifications, [{
      method: "notifications/progress",
      params: { progressToken: "fixture", progress: 0.5 },
    }]);
    const serverNotifications = asRecord(relay.serverNotifications).notifications;
    assert.ok(Array.isArray(serverNotifications));
    assert.equal(serverNotifications.length, 1);
    const initializedNotification = asRecord(serverNotifications[0]);
    assert.equal(initializedNotification.method, "notifications/initialized");
    assert.ok(initializedNotification.params == null);

    const cancellation = await runFlow(sessionUrl, "mcp-cancel-flow", "mcp-cancel-result");
    assert.deepEqual(cancellation, {
      messageCancelled: true,
      cancellationNotificationObserved: true,
      disconnectRejectedPending: true,
      recoveredEcho: { recovered: true },
    });
    console.log("ACP SDK protocol smoke passed (MCP bidirectional relay, errors, cancellation and reconnect)");
  } finally {
    observer.abort();
    await drain;
  }
} finally {
  observer.abort();
  await server.close();
}

async function runFlow(
  sessionUrl: string,
  flow: string,
  resultMessageId: string,
): Promise<Record<string, unknown>> {
  const before = await getJson<BridgeSessionView>(sessionUrl);
  assert.equal(before.phase, "ready");
  assert.ok(before.historyRevision);
  const submitted = await fetch(`${sessionUrl}/turns`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "idempotency-key": flow,
      "if-match": `"${before.historyRevision}"`,
    },
    body: JSON.stringify({ prompt: [{ type: "text", text: flow }] }),
  });
  assert.equal(submitted.status, 202);
  const completed = await eventually(async () => {
    const view = await getJson<BridgeSessionView>(sessionUrl);
    assert.notEqual(view.phase, "blocked", view.syncError ?? "session became blocked");
    return view.phase === "ready" && view.historyRevision !== before.historyRevision
      ? view : undefined;
  }, flow);
  assert.equal(completed.activeTurn, null);
  assert.equal(completed.turnOutcomes?.at(-1)?.response.stopReason, "end_turn");
  const message = completed.timeline.find((update) =>
    update.sessionUpdate === "agent_message_chunk" && update.messageId === resultMessageId
  );
  assert.ok(message?.sessionUpdate === "agent_message_chunk" && message.content.type === "text",
    `${flow} must report its upstream protocol results`);
  return asRecord(JSON.parse(message.content.text));
}

function asRecord(value: unknown): Record<string, unknown> {
  assert.ok(value != null && typeof value === "object" && !Array.isArray(value));
  return value as Record<string, unknown>;
}

async function getJson<T>(url: string): Promise<T> {
  const response = await fetch(url);
  assert.equal(response.status, 200, `${url} returned ${response.status}`);
  return response.json() as Promise<T>;
}

async function eventually<T>(read: () => Promise<T | undefined>, label: string): Promise<T> {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    const result = await read();
    if (result !== undefined) return result;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error(`Timed out waiting for ${label}`);
}
