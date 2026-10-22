// The write side of the client of revision 3 (docs/protocol.md §6, §7): operations with
// client-chosen ids, every one answered 202, and actions. The transport is faked so each
// case fixes the order of HTTP answers itself.
import { describe, expect, it } from "vitest";
import {
  ActionsClient,
  OpsClient,
  type ActionOutcome,
  type ReplicaSource,
  type WriteOutcome,
} from "../shared/write-client";
import { WriteQueue, type HttpFetch } from "../shared/protocol-client";

const S = "session/S";
const OPS = "/api/v3/scopes/session%2FS/ops";
const ID = "AAAAAAAAAAAAAAAAAAAAAA";

interface Call {
  url: string;
  method: string;
  headers: Record<string, string>;
  body: unknown;
  respond(status: number, body?: unknown): void;
  lose(): void;
}

function http() {
  const calls: Call[] = [];
  const fetch: HttpFetch = (url, init) =>
    new Promise((resolve, reject) => {
      calls.push({
        url,
        method: init.method,
        headers: init.headers,
        body: JSON.parse(init.body),
        respond: (status, body) =>
          resolve({
            status,
            json: () => (body === undefined ? Promise.reject(new SyntaxError("no body")) : Promise.resolve(body)),
          }),
        lose: () => reject(new TypeError("network error")),
      });
    });
  return { calls, fetch };
}

/** The documents the writer reads `expect` from, set directly by each case. */
function shown() {
  const docs = new Map<string, { doc: { live: unknown } }>();
  const source: ReplicaSource = { get: (scope) => docs.get(scope) };
  return {
    source,
    show(op: Record<string, unknown>, scope = S) {
      docs.set(scope, {
        doc: {
          live: {
            op: { id: "op-A", scope, after: null, kind: "none", status: "completed", input: null, cancelRequested: false, ...op },
          },
        },
      });
    },
  };
}

const prompt = { kind: "prompt", args: { prompt: [{ type: "text", text: "hi" }] } } as const;

describe("operation writes (§6)", () => {
  it("sends the id it chose and the op the document shows as expect, and answers 202 with that op", async () => {
    const { calls, fetch } = http();
    const { source, show } = shown();
    show({ id: "op-A" });
    const ops = new OpsClient(fetch, source, () => ID);
    const outcome = ops.write(S, prompt);
    expect(calls).toHaveLength(1);
    expect(calls[0]).toMatchObject({
      url: OPS,
      method: "POST",
      body: { id: ID, expect: "op-A", kind: "prompt", args: prompt.args },
    });
    calls[0].respond(202, { op: ID });
    expect(await outcome).toEqual({ kind: "accepted", op: ID });
  });

  it("makes ids that are new for each intent", async () => {
    const { calls, fetch } = http();
    const { source, show } = shown();
    show({});
    const ops = new OpsClient(fetch, source);
    void ops.write(S, prompt);
    void ops.write(S, prompt);
    const ids = calls.map((call) => (call.body as { id: string }).id);
    expect(ids[0]).toMatch(/^[A-Za-z0-9_-]{22}$/u);
    expect(ids[0]).not.toBe(ids[1]);
  });

  it("sends nothing to a scope it holds no aligned document of", async () => {
    const { calls, fetch } = http();
    const { source } = shown();
    const outcome = await new OpsClient(fetch, source, () => ID).write(S, prompt);
    expect(outcome).toMatchObject({ kind: "blocked" });
    expect(calls).toEqual([]);
  });

  it("tells a definite refusal from a write without a definite answer", async () => {
    const { calls, fetch } = http();
    const { source, show } = shown();
    show({});
    const ops = new OpsClient(fetch, source, () => ID);

    const stale = ops.write(S, prompt);
    calls[0].respond(409, { error: { code: "stale", message: "not current" } });
    expect(await stale).toEqual({ kind: "refused", error: { code: "stale", message: "not current" } });

    const lost = ops.write(S, prompt);
    calls[1].lose();
    expect(await lost).toMatchObject({ kind: "uncertain" });

    const proxy = ops.write(S, prompt);
    calls[2].respond(502);
    expect(await proxy).toMatchObject({ kind: "uncertain" });
  });

  it("answers 202 without a body as uncertain: the write may have taken effect", async () => {
    const { calls, fetch } = http();
    const { source, show } = shown();
    show({});
    const ops = new OpsClient(fetch, source, () => ID);
    const outcome = ops.write(S, prompt);
    calls[0].respond(202, {});
    expect(await outcome).toMatchObject({ kind: "uncertain" });
  });

  it("repeats a write with the same id and expect, and the answer is the same operation", async () => {
    const { calls, fetch } = http();
    const { source, show } = shown();
    show({ id: "op-A" });
    const ops = new OpsClient(fetch, source, () => ID);
    const first = ops.write(S, prompt);
    calls[0].lose();
    const lost = (await first) as Extract<WriteOutcome, { kind: "uncertain" }>;
    expect(lost.write).toEqual({ id: ID, expect: "op-A", scope: S });

    // The document moved on meanwhile; the repeat is still the same write.
    show({ id: "op-B" });
    const repeat = ops.repeat(lost.write, prompt);
    expect(calls[1].body).toEqual({ id: ID, expect: "op-A", kind: "prompt", args: prompt.args });
    calls[1].respond(202, { op: ID });
    expect(await repeat).toEqual({ kind: "accepted", op: ID });
  });

  it("resolves a lost write from the document of the scope it was written to", async () => {
    const { calls, fetch } = http();
    const { source, show } = shown();
    show({ id: "op-A" });
    const ops = new OpsClient(fetch, source, () => ID);
    const first = ops.write(S, prompt);
    calls[0].lose();
    const lost = (await first) as Extract<WriteOutcome, { kind: "uncertain" }>;

    show({ id: ID, after: "op-A", scope: S });
    expect(ops.resolve(lost.write)).toBe("admitted");
    show({ id: "op-A", status: "completed" });
    expect(ops.resolve(lost.write)).toBe("not_admitted");
    show({ id: "op-X", scope: "global", after: "op-A" });
    expect(ops.resolve(lost.write)).toBe("unknown");
  });

  it("cancels the op the document shows, with no id of its own", async () => {
    const { calls, fetch } = http();
    const { source, show } = shown();
    show({ id: "op-A", status: "running", kind: "prompt" });
    const ops = new OpsClient(fetch, source, () => ID);
    const outcome = ops.cancel(S);
    expect(calls[0].body).toEqual({ expect: "op-A", kind: "cancel", args: {} });
    calls[0].respond(202, { op: "op-A" });
    expect(await outcome).toEqual({ kind: "accepted", op: "op-A" });
  });
});

describe("actions (§7)", () => {
  function actions() {
    const { calls, fetch } = http();
    return { calls, client: new ActionsClient(fetch) };
  }

  it("answers one interaction at its resource and is done on 204", async () => {
    const { calls, client } = actions();
    const outcome = client.answer(S, "int 1", { outcome: { outcome: "selected", optionId: "allow" } });
    expect(calls[0]).toMatchObject({
      url: "/api/v3/scopes/session%2FS/resources/int%201/answer",
      body: { outcome: { outcome: "selected", optionId: "allow" } },
    });
    calls[0].respond(204);
    expect(await outcome).toEqual({ kind: "done" });
  });

  it("reports an interaction that is gone as refused", async () => {
    const { calls, client } = actions();
    const outcome = client.answer(S, "i", { outcome: { outcome: "cancelled" } });
    calls[0].respond(404, { error: { code: "not_found", message: "gone" } });
    expect(await outcome).toEqual({ kind: "refused", error: { code: "not_found", message: "gone" } });
  });

  it("never resends an answer whose response was lost", async () => {
    const { calls, client } = actions();
    const outcome = client.answer(S, "i", { outcome: { outcome: "cancelled" } });
    calls[0].lose();
    expect(await outcome).toEqual({ kind: "uncertain" });
    expect(calls).toHaveLength(1);
  });

  it("sets a mode and an option, held until the Agent answers", async () => {
    const { calls, client } = actions();
    const mode = client.setMode(S, "plan");
    expect(calls[0]).toMatchObject({ url: "/api/v3/scopes/session%2FS/actions/set_mode", body: { modeId: "plan" } });
    calls[0].respond(200, {});
    expect(await mode).toEqual({ kind: "done" });

    const option = client.setConfig(S, "verbose", true);
    expect(calls[1]).toMatchObject({
      url: "/api/v3/scopes/session%2FS/actions/set_config",
      body: { configId: "verbose", value: true },
    });
    calls[1].respond(200, {});
    expect(await option).toEqual({ kind: "done" });
  });

  it("reports the error the Agent answered with as failed, not as done", async () => {
    const { calls, client } = actions();
    const mode = client.setMode(S, "plan");
    calls[0].respond(200, { error: { code: "agent_error", message: "refused" } });
    const outcome: ActionOutcome = await mode;
    expect(outcome).toEqual({ kind: "refused", error: { code: "agent_error", message: "refused" } });
  });

  it("closes a session, and a refusal before anything was sent is refused", async () => {
    const { calls, client } = actions();
    const closed = client.close(S);
    expect(calls[0]).toMatchObject({ url: "/api/v3/scopes/session%2FS/actions/close", body: {} });
    calls[0].respond(409, { error: { code: "not_allowed", message: "closing" } });
    expect(await closed).toEqual({ kind: "refused", error: { code: "not_allowed", message: "closing" } });
  });

  it("sends terminal input in order, one request at a time, and dismisses a resource", async () => {
    const { calls, client } = actions();
    const first = client.input("global", "t1", "a");
    const second = client.input("global", "t1", "b");
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe("/api/v3/scopes/global/resources/t1/input");
    calls[0].respond(204);
    await first;
    await Promise.resolve();
    await Promise.resolve();
    expect(calls).toHaveLength(2);
    calls[1].respond(204);
    expect(await second).toEqual({ kind: "done" });

    const dismissed = client.dismiss("global", "t1");
    expect(calls[2].url).toBe("/api/v3/scopes/global/resources/t1/dismiss");
    calls[2].respond(204);
    expect(await dismissed).toEqual({ kind: "done" });
  });
});

describe("queued writes on revision 3 (C2, C3)", () => {
  // The queue sends through the revision 3 client and reads the `live` of the documents.
  function queued() {
    const { calls, fetch } = http();
    const { source, show } = shown();
    const ids = ["AAAAAAAAAAAAAAAAAAAAAA", "BBBBBBBBBBBBBBBBBBBBBB", "CCCCCCCCCCCCCCCCCCCCCC"];
    let n = 0;
    const ops = new OpsClient(fetch, source, () => ids[n++]);
    const queue = new WriteQueue(ops, { get: (scope) => {
      const replica = source.get(scope);
      return replica === undefined ? undefined : { live: replica.doc.live };
    } });
    return { calls, show, queue };
  }

  it("holds a write while the operation runs and sends it against the op that ended", async () => {
    const { calls, show, queue } = queued();
    show({ id: "op-A", kind: "prompt", status: "running" });
    queue.enqueue(S, prompt);
    expect(calls).toHaveLength(0);
    show({ id: "op-A", kind: "prompt", status: "completed" });
    queue.update(S);
    expect(calls).toHaveLength(1);
    expect(calls[0].body).toMatchObject({ id: "AAAAAAAAAAAAAAAAAAAAAA", expect: "op-A", kind: "prompt" });
    calls[0].respond(202, { op: "AAAAAAAAAAAAAAAAAAAAAA" });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(queue.items(S)).toEqual([]);
  });

  it("sends the next write only against a later op that ended", async () => {
    const { calls, show, queue } = queued();
    show({ id: "op-A", status: "completed" });
    queue.enqueue(S, prompt);
    queue.enqueue(S, prompt);
    expect(calls).toHaveLength(1);
    calls[0].respond(202, { op: "AAAAAAAAAAAAAAAAAAAAAA" });
    await new Promise((resolve) => setTimeout(resolve, 0));
    // The same ended op: a second write against it could only be stale.
    expect(calls).toHaveLength(1);
    show({ id: "AAAAAAAAAAAAAAAAAAAAAA", status: "completed" });
    queue.update(S);
    expect(calls).toHaveLength(2);
    expect(calls[1].body).toMatchObject({ expect: "AAAAAAAAAAAAAAAAAAAAAA", id: "BBBBBBBBBBBBBBBBBBBBBB" });
  });

  it("never sends again a write without a definite answer", async () => {
    const { calls, show, queue } = queued();
    show({ id: "op-A", status: "completed" });
    queue.enqueue(S, prompt);
    calls[0].lose();
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(queue.items(S).map((item) => item.state)).toEqual(["uncertain"]);
    show({ id: "op-B", status: "completed" });
    queue.update(S);
    expect(calls).toHaveLength(1);
  });
});
