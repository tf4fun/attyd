import assert from "node:assert/strict";
import { join } from "node:path";
import { mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import type { Answer, Interaction, PromptResult } from "../shared/protocol.js";
import {
  entriesOf, globalLive, replyText, sessionLive, sessionScope, SmokeClient,
} from "./protocol-smoke-client.js";
import { startRustTestServer } from "./rust-test-server.js";

// SDK Agent -> ACP client terminal/MCP services, with the same protocol v2
// stream and writes the browser uses. Fixtures report results; assertions live here.
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
const client = new SmokeClient(`http://127.0.0.1:${server.port}`);

try {
  client.follow([{ scope: "global" }]);
  await client.until("global", (replica) => (globalLive(replica)?.agent.phase === "ready" ? true : undefined),
    "Agent initialization");
  const created = await client.run("global", { kind: "create", args: { cwd: sessionCwd } });
  assert.equal(created.status, "completed", JSON.stringify(created));
  const scope = sessionScope((created.result as { sessionId: string }).sessionId);
  // The stream follows the session until every flow's result is read.
  client.follow([{ scope: "global" }, { scope }]);

  const services = await runFlow(scope, "client-services-flow", (id, pending) => {
    assert.equal(pending.kind, "elicitation", id);
    if (pending.kind !== "elicitation") throw new Error("not an elicitation");
    return { response: pending.request.mode === "form"
      ? { action: "accept", content: { value: "😀" } }
      : { action: pending.request.message === "Decline URL" ? "decline" : "accept" } };
  });
  assert.deepEqual(services.read, { content: "session workspace input" });
  assert.equal(await readFile(join(sessionCwd, "output.txt"), "utf8"), "written in session workspace");
  assert.deepEqual(services.directories, [sessionCwd, sessionCwd]);
  assert.equal(asRecord(services.unsupportedMode).code, -32602);
  assert.deepEqual(services.unicode, { action: "accept", content: { value: "😀" } });
  assert.equal(asRecord(services.outstandingDuplicate).code, -32602);
  assert.deepEqual(services.urls, [{ action: "accept" }, { action: "accept" }, { action: "decline" }, { action: "accept" }]);
  const relay = await runFlow(scope, "mcp-flow");
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

  const cancellation = await runFlow(scope, "mcp-cancel-flow");
  assert.deepEqual(cancellation, {
    messageCancelled: true,
    cancellationNotificationObserved: true,
    disconnectRejectedPending: true,
    recoveredEcho: { recovered: true },
  });
  const terminals = await runFlow(scope, "terminal-command-flow");
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
  const lifecycle = await runFlow(scope, "terminal-lifecycle-flow");
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
  assert.deepEqual(client.streamErrors, []);
} finally {
  client.close();
  await server.close();
  await rm(sessionCwd, { recursive: true, force: true });
}

/**
 * Runs the prompt `flow` and returns the results its reply reports. While it
 * runs, `answer` gives the answer to each interaction it asks, one at a time.
 */
async function runFlow(
  scope: string,
  flow: string,
  answer?: (id: string, interaction: Interaction) => Answer,
): Promise<Record<string, unknown>> {
  await client.ended(scope);
  const held = entriesOf(client.replicas.get(scope)).length;
  const started = await client.ops.write(scope, { kind: "prompt", args: { prompt: [{ type: "text", text: flow }] } });
  assert.ok(started.kind === "accepted", JSON.stringify(started));
  const answered = new Set<string>();
  const answers: Promise<void>[] = [];
  const ended = await client.until(scope, (replica) => {
    const live = sessionLive(replica);
    const op = live?.op;
    if (live == null || op?.id !== started.op) return undefined;
    if (op.status !== "running") return op;
    // Each interaction is answered once; nothing is resent.
    for (const [id, pending] of Object.entries(live.interactions)) {
      if (answered.has(id)) continue;
      assert.ok(answer, `${flow} asked for input: ${JSON.stringify(live.interactions)}`);
      answered.add(id);
      answers.push(client.actions.answer(scope, id, answer(id, pending)).then((outcome) => {
        assert.equal(outcome.kind, "done", `${flow} answer: ${JSON.stringify(outcome)}`);
      }));
    }
    return undefined;
  }, flow);
  await Promise.all(answers);
  assert.equal(ended.status, "completed", JSON.stringify(ended.error));
  assert.equal((ended.result as PromptResult).stopReason, "end_turn");
  // The turn's reply carries the results: the Agent messages the turn added, joined.
  const reply = entriesOf(client.replicas.get(scope)).slice(held)
    .filter((entry) => entry.kind === "message" && entry.role === "agent")
    .map(replyText).join("");
  return asRecord(JSON.parse(reply));
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
