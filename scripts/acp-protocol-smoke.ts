import assert from "node:assert/strict";
import { join } from "node:path";
import { mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import type {
  BridgeSessionView,
  CreatedSessionResult,
  RuntimeView,
} from "../web/src/lib/business-api.js";
import { startRustTestServer } from "./rust-test-server.js";

// SDK Agent -> ACP client terminal/MCP services, with the same REST/SSE
// observation used by the browser. Fixtures report results; assertions live here.
const cwd = process.cwd();
const sessionCwd = await realpath(await mkdtemp(join(tmpdir(), "attyd-session-services-")));
await writeFile(join(sessionCwd, "input.txt"), "session workspace input");
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
    body: JSON.stringify({ cwd: sessionCwd }),
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
    const services = await runFlow(sessionUrl, "client-services-flow", "client-services-result", async (view) => {
      for (const pending of Object.values(view.interactions.elicitations)) {
        const response = pending.request.mode === "form"
          ? { action: "accept", content: { value: "😀" } }
          : { action: pending.request.message === "Decline URL" ? "decline" : "accept" };
        const responded = await fetch(`${sessionUrl}/interactions/${encodeURIComponent(pending.interactionId)}/response`, {
          method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ kind: "elicitation", response }),
        });
        assert.equal(responded.status, 200, await responded.text());
      }
    });
    assert.deepEqual(services.read, { content: "session workspace input" });
    assert.equal(await readFile(join(sessionCwd, "output.txt"), "utf8"), "written in session workspace");
    assert.deepEqual(services.directories, [sessionCwd, sessionCwd]);
    assert.equal(asRecord(services.unsupportedMode).code, -32602);
    assert.deepEqual(services.unicode, { action: "accept", content: { value: "😀" } });
    assert.equal(asRecord(services.outstandingDuplicate).code, -32602);
    assert.deepEqual(services.urls, [{ action: "accept" }, { action: "accept" }, { action: "decline" }, { action: "accept" }]);
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
    assert.deepEqual(relay.nestedRoundTrip, { clientResult: { nested: true } },
      "the MCP reader must process nested responses while an Agent callback is pending");
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
    const terminals = await runFlow(sessionUrl, "terminal-command-flow", "terminal-command-result");
    const compound = terminalOutput(terminals.compound, 0).trimEnd().split("\n");
    assert.equal(compound.length, 3, "every statement in the compound shell command must run");
    assert.equal(compound[0], "shell-ok");
    assert.ok(compound[1].length > 0, "uname -a must produce system information");
    assert.match(compound[2], /(?:^|\/)sh$/u, "command -v must resolve the shell builtin's argument");
    assert.equal(
      terminalOutput(terminals.literalArguments, 0),
      "<two words>\n<$(printf expanded)>\n<a'b>\n<a\"b>\n<>\n<semi;colon>\n<*>\n",
      "explicit arguments must retain literal spaces, substitutions, quotes and empty strings",
    );
    assert.match(terminalOutput(terminals.missing, 127), /attyd-fixture-command-does-not-exist/u);
    assert.equal(terminalOutput(terminals.recovered, 0), "terminal-recovered\n");
    const lifecycle = await runFlow(sessionUrl, "terminal-lifecycle-flow", "terminal-lifecycle-result");
    assert.equal(asRecord(lifecycle.otherOutput).output, "OTHER_TASK_FINISHED\n");
    assert.equal(asRecord(asRecord(lifecycle.otherOutput).exitStatus).exitCode, 0);
    assert.equal(asRecord(lifecycle.beforeKill).output, "LONG_TASK_READY\n");
    assert.ok(asRecord(lifecycle.beforeKill).exitStatus == null,
      "a long-running terminal must survive other terminal requests");
    assert.equal(asRecord(lifecycle.killedOutput).output, "LONG_TASK_READY\n",
      "terminal/kill must retain the terminal ID and its output");
    assert.equal(asRecord(lifecycle.killedOutput).truncated, false);
    assert.deepEqual(asRecord(lifecycle.killedOutput).exitStatus, lifecycle.killedStatus);
    assert.ok(asRecord(lifecycle.killedStatus).signal != null ||
      asRecord(lifecycle.killedStatus).exitCode != null, "terminal/kill must publish an exit status");
    assert.ok(Array.isArray(lifecycle.releasedErrors));
    assert.equal(lifecycle.releasedErrors.length, 3);
    for (const error of lifecycle.releasedErrors) {
      assert.equal(asRecord(error).code, -32602,
        "terminal/release must invalidate the ID for output, wait_for_exit and kill");
    }
    assert.equal(asRecord(lifecycle.shellStatus).exitCode, 0);
    assert.equal(lifecycle.backgroundAfterExit, true,
      "a redirected nohup service must survive its launching shell's normal exit");
    assert.equal(lifecycle.backgroundAfterRelease, true,
      "releasing an already completed terminal must not kill its background service");
    console.log("ACP SDK protocol smoke passed (session workspace services; elicitation modes, Unicode and URL ID reuse; terminal lifecycle; nested MCP relay, cancellation and reconnect)");
  } finally {
    observer.abort();
    await drain;
  }
} finally {
  observer.abort();
  await server.close();
  await rm(sessionCwd, { recursive: true, force: true });
}

async function runFlow(
  sessionUrl: string,
  flow: string,
  resultMessageId: string,
  respond?: (view: BridgeSessionView) => Promise<void>,
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
    await respond?.(view);
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

function terminalOutput(value: unknown, exitCode: number): string {
  const terminal = asRecord(value);
  assert.equal(asRecord(terminal.waitStatus).exitCode, exitCode);
  assert.equal(asRecord(terminal.exitStatus).exitCode, exitCode);
  assert.equal(terminal.truncated, false);
  assert.equal(typeof terminal.output, "string");
  return terminal.output as string;
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
