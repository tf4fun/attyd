import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { join } from "node:path";
import { mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { SEGMENT_LENGTH } from "../shared/tree-hash.js";
import { ReferenceClient } from "./reference-client.js";
import { entriesOf, promptsOf, replyText, sessionLive, sessionScope, SmokeClient } from "./protocol-smoke-client.js";
import { startRustTestServer } from "./rust-test-server.js";

// Scenarios S1 to S5 of docs/protocol-r4-design.md §10, from a client that is not the page: a small
// device that follows the latest output of a session as a window and the head of its operation,
// beside the page's own client (the desktop) following the whole session. Fixtures report;
// the assertions live here.
const N = 4096;
const bound = N + SEGMENT_LENGTH;
// The text `output-window-flow` writes: 40 chunks of 9 bytes, 1 MiB of two-byte characters, one
// byte, 12,000 bytes and a last line.
const FLOW_BYTES = 40 * 9 + 524_288 * 2 + 1 + 12_000 + "end of output".length;

const cwd = process.cwd();
const sessionCwd = await realpath(await mkdtemp(join(tmpdir(), "attyd-reference-client-")));
const server = await startRustTestServer({
  cwd,
  command: [process.execPath, "--import", "tsx", join(cwd, "tests/fixtures/fake-agent.ts")],
});
const origin = `http://127.0.0.1:${server.port}`;
const desktop = new SmokeClient(origin);
const clients: ReferenceClient[] = [];
const device = (session: string, options: { window?: number } = {}): ReferenceClient => {
  const client = new ReferenceClient(origin, session, { cwd: sessionCwd, ...options });
  clients.push(client);
  return client;
};

try {
  desktop.follow([{ scope: "global" }]);
  await desktop.until("global", (replica) => ((replica?.doc.live as { agent?: { phase?: string } } | undefined)?.agent?.phase === "ready" ? true : undefined), "Agent initialization");
  const created = await desktop.run("global", { kind: "create", args: { cwd: sessionCwd } });
  assert.equal(created.status, "completed", JSON.stringify(created));
  const sessionId = (created.result as { sessionId: string }).sessionId;
  const scope = sessionScope(sessionId);
  desktop.follow([{ scope: "global" }, { scope }]);
  await desktop.ended(scope);

  // S1. A device showing the latest output, on a stream.
  const stream = device(sessionId);
  stream.stream();
  await stream.until(() => (stream.op !== undefined && stream.output !== undefined ? true : undefined), "the head and the output");
  const first = await stream.prompt("output-window-flow");
  assert.equal(first.status, 202, JSON.stringify(first.body));
  const turn = (first.body as { op: string }).op;
  // The version at which the whole text was first held: the turn ends, and the view changes besides
  // its text, after it, and neither makes the segments of that text stale.
  const complete = await stream.until(() => (stream.output?.text.length === FLOW_BYTES ? stream.version : undefined), "the whole output");
  await stream.until(() => (stream.op?.id === turn && stream.op.status !== "running" ? true : undefined), "the turn to end");
  assert.equal(stream.op?.status, "completed");
  // A checkpoint confirms what the device holds, hashed over the window from its 32-byte state.
  await stream.until(() => (stream.verified !== undefined && stream.verified === stream.version ? true : undefined), "a checkpoint");
  const held = stream.output!;
  assert.ok(held.text.text.endsWith("end of output"));
  assert.equal(held.text.offset + new TextEncoder().encode(held.text.text).length, FLOW_BYTES);
  assert.ok(held.text.text.length < bound, `the window holds ${held.text.text.length} characters`);

  // Paging: from the version it held when the text was complete, the device reads the segments before
  // its window one at a time, however many commits followed, and the chain reaches the window's
  // 32-byte state (design §7.1).
  let state = Buffer.alloc(32);
  const textPath = `${stream.outputPath}/text`;
  assert.ok(held.text.offset > 0 && held.text.offset % SEGMENT_LENGTH === 0, `the window starts at ${held.text.offset}`);
  for (let index = 0; index < held.text.offset / SEGMENT_LENGTH; index++) {
    const response = await fetch(`${origin}/api/v2/nodes${textPath}/segments/${index}?at=${complete}`);
    assert.equal(response.status, 200, `segment ${index}: ${await response.clone().text()}`);
    const read = await response.json() as { before: string; segment: string };
    assert.equal(read.before, state.toString("base64url"), `the chain before segment ${index}`);
    state = createHash("sha256").update(state).update(read.segment, "utf8").digest();
  }
  assert.equal(state.toString("base64url"), held.text.before, "the chain reaches the state the window starts from");

  // The figures: no event carries a window's text of N + L bytes, nothing is held beyond it, and
  // applying an event needs under 2 (N + L) bytes of text, however long the output is.
  const { stats } = stream;
  console.log(`reference client on a stream: ${JSON.stringify(stats)} (N + L = ${bound}, output ${FLOW_BYTES} bytes)`);
  assert.ok(stats.maxEventTextBytes < bound, `an event carried ${stats.maxEventTextBytes} bytes of text`);
  assert.ok(stats.maxHeldTextBytes < bound, `the window held ${stats.maxHeldTextBytes} bytes of text`);
  assert.ok(stats.peakTextBytes < 2 * bound, `applying an event needed ${stats.peakTextBytes} bytes of text`);
  assert.ok(stats.maxEventBytes < 4 * bound, `an event was ${stats.maxEventBytes} bytes`);
  assert.deepEqual(stream.streamErrors, []);

  // S1, polled: a device without a stream asks for the same paths; with nothing changed it is told so.
  const poller = device(sessionId);
  assert.equal(await poller.poll(), "changed");
  assert.deepEqual(poller.output, stream.output);
  assert.equal(poller.version, stream.version);
  assert.equal(await poller.poll(), "unchanged");
  assert.ok(poller.stats.peakTextBytes < 2 * bound);

  // S2. The desktop holds the whole session; the device's window is the end of its latest message.
  const reply = entriesOf(desktop.replicas.get(scope)).filter((entry) => entry.kind === "message" && entry.role === "agent").map(replyText).at(-1)!;
  const bytes = new TextEncoder().encode(reply);
  assert.equal(bytes.length, FLOW_BYTES);
  assert.ok(reply.endsWith(held.text.text));
  assert.equal(new TextEncoder().encode(reply.slice(0, reply.length - held.text.text.length)).length, held.text.offset);

  // S3. A change that does not advance the path is ignored; a request repeated with its own
  // identity executes once.
  const turnsBefore = promptsOf(desktop.replicas.get(scope)).length;
  const again = await stream.prompt("output-window-flow");
  assert.equal(again.status, 202);
  const second = (again.body as { op: string }).op;
  const repeated = await stream.prompt("output-window-flow", { id: again.id, expect: again.expect });
  assert.equal(repeated.status, 202, JSON.stringify(repeated.body));
  assert.equal((repeated.body as { op: string }).op, second);
  await stream.until(() => (stream.op?.id === second && stream.op.status !== "running" ? true : undefined), "the second turn to end");
  await desktop.ended(scope, second);
  assert.equal(promptsOf(desktop.replicas.get(scope)).length, turnsBefore + 1, "the repeated request ran once");

  // S4. Two clients write prompts with the same expect: one is admitted, the other is stale and
  // its outcome is superseded.
  const rival = device(sessionId);
  rival.stream();
  await rival.until(() => (rival.op !== undefined ? true : undefined), "the rival's head");
  await stream.until(() => (stream.op?.id === rival.op?.id ? true : undefined), "the same head");
  const expect = stream.op!.id;
  const at = stream.opVersion!;
  const [a, b] = await Promise.all([
    stream.prompt("output-window-flow", { expect }),
    rival.prompt("output-window-flow", { expect }),
  ]);
  const [won, lost] = a.status === 202 ? [a, b] : [b, a];
  assert.equal(won.status, 202, JSON.stringify([a, b]));
  assert.equal(lost.status, 409, JSON.stringify([a, b]));
  assert.equal((lost.body as { error: { code: string } }).error.code, "stale");
  await stream.until(() => (stream.op?.id === (won.body as { op: string }).op && stream.op.status !== "running" ? true : undefined), "the winner to end");
  const query = new URLSearchParams({ expect: lost.expect, instance: stream.instance!, at: String(at) });
  const outcome = await (await fetch(`${origin}/api/v2/targets/sessions/${encodeURIComponent(sessionId)}/ops/${lost.id}?${query}`)).json();
  assert.equal((outcome as { outcome: string }).outcome, "superseded", JSON.stringify(outcome));

  // S5. An interaction is answered at most once; a cancel acts on the running prompt.
  const answered = await promptWithPermission(scope);
  assert.equal(answered.filter((outcome) => outcome.kind === "done").length, 1, JSON.stringify(answered));
  assert.equal(answered.filter((outcome) => outcome.kind === "refused").length, 1, JSON.stringify(answered));
  await desktop.ended(scope);
  const running = await desktop.ops.write(scope, { kind: "prompt", args: { prompt: [{ type: "text", text: "permission-flow" }] } });
  assert.ok(running.kind === "accepted", JSON.stringify(running));
  await desktop.until(scope, (replica) => (Object.keys(sessionLive(replica)?.interactions ?? {}).length > 0 ? true : undefined), "the permission request");
  const cancel = await desktop.ops.cancel(scope);
  assert.ok(cancel.kind === "accepted" && cancel.op === running.op, JSON.stringify(cancel));
  await desktop.ended(scope, running.op);

  console.log("reference client smoke passed (S1 stream and poll with the figures of a window, S2 the window against the whole session, S3 a repeated request, S4 conflicting writes, S5 answers and cancel)");
} finally {
  for (const client of clients) client.close();
  desktop.close();
  await server.close();
  await rm(sessionCwd, { recursive: true, force: true });
}

/** Starts the permission prompt and answers its request twice at once. */
async function promptWithPermission(scope: string) {
  const started = await desktop.ops.write(scope, { kind: "prompt", args: { prompt: [{ type: "text", text: "permission-flow" }] } });
  assert.ok(started.kind === "accepted", JSON.stringify(started));
  const id = await desktop.until(scope, (replica) => Object.keys(sessionLive(replica)?.interactions ?? {})[0], "the permission request");
  return Promise.all([
    desktop.actions.answer(scope, id, { outcome: { outcome: "selected", optionId: "yes" } }),
    desktop.actions.answer(scope, id, { outcome: { outcome: "selected", optionId: "yes" } }),
  ]);
}
