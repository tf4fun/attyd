// The write queue and the resource client (§6.4, §7, C2, C3; acceptance 15, 16). The transport
// is faked so each case fixes the order of HTTP answers itself; the queue sends through the
// revision 3 operations client against the documents it shows.
import { afterEach, describe, expect, it, vi } from "vitest";
import type { OpArgs } from "../shared/protocol";
import { ResourceClient, WriteQueue, type HttpFetch } from "../shared/protocol-client";
import { OpsClient, type ReplicaSource } from "../shared/write-client";

const S = "session/S";

const flush = async () => {
  for (let i = 0; i < 20; i += 1) await Promise.resolve();
};

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
  const fetch: HttpFetch = (url, init) => new Promise((resolve, reject) => {
    calls.push({
      url,
      method: init.method,
      headers: init.headers,
      body: JSON.parse(init.body),
      respond: (status, body) => resolve({
        status,
        json: () => (body === undefined ? Promise.reject(new SyntaxError("no body")) : Promise.resolve(body)),
      }),
      lose: () => reject(new TypeError("network error")),
    });
  });
  return { calls, fetch };
}

function sessionLive(op: Record<string, unknown> = {}) {
  return {
    op: { id: "op-A", scope: S, after: null, kind: "none", status: "completed", input: null, cancelRequested: false, ...op },
    interactions: {}, urlFlows: {}, sessionId: "S", cwd: "/work", terminals: {},
  };
}

/** The documents the writers read, set directly by each case. */
function shown() {
  const docs = new Map<string, { doc: { live: unknown } }>();
  const source: ReplicaSource = { get: (scope) => docs.get(scope) };
  return {
    source,
    show(op: Record<string, unknown>, scope = S) {
      docs.set(scope, { doc: { live: sessionLive(op) } });
    },
  };
}

const prompt = (text: string): Extract<OpArgs, { kind: "prompt" }> => ({ kind: "prompt", args: { prompt: [{ type: "text", text }] } });

afterEach(() => {
  vi.useRealTimers();
});

describe("queued writes (C2, C3; acceptance 15)", () => {
  function queue() {
    const net = http();
    const view = shown();
    view.show({ id: "op-A", kind: "prompt", status: "running" });
    const ops = new OpsClient(net.fetch, view.source);
    const live = { get: (scope: string) => { const doc = view.source.get(scope); return doc === undefined ? undefined : { live: doc.doc.live }; } };
    return { net, view, queue: new WriteQueue(ops, live) };
  }

  const texts = (calls: Call[]) => calls.map((call) => (call.body as { args: { prompt: Array<{ text: string }> } }).args.prompt[0].text);

  it("sends a queued write once the replica shows op ended, then lets it leave the queue", async () => {
    const { net, view, queue: q } = queue();
    const id = q.enqueue(S, prompt("next"));
    q.update(S);
    expect(net.calls).toHaveLength(0);
    expect(q.items(S)).toEqual([{ id, write: prompt("next"), state: "queued" }]);

    view.show({ id: "op-A", kind: "prompt", status: "completed" });
    q.update(S);
    expect(net.calls).toHaveLength(1);
    expect(net.calls[0].body).toMatchObject({ expect: "op-A", kind: "prompt" });
    expect(q.items(S)[0].state).toBe("sending");
    q.update(S);
    expect(net.calls).toHaveLength(1);

    net.calls[0].respond(202, { op: "op-B" });
    await flush();
    expect(q.items(S)).toEqual([]);
  });

  it("sends the next write against the op the replica shows, even one past the admitted one", async () => {
    const { net, view, queue: q } = queue();
    q.enqueue(S, prompt("one"));
    q.enqueue(S, prompt("two"));
    view.show({ id: "op-A", status: "completed" });
    q.update(S);
    net.calls[0].respond(202, { op: "op-B" });
    await flush();
    // The replica still shows op-A: sending against it again would be stale.
    q.update(S);
    expect(net.calls).toHaveLength(1);
    // The stream skips op-B and shows op-C ended: nothing waits for op-B.
    view.show({ id: "op-C", status: "completed" });
    q.update(S);
    expect(net.calls).toHaveLength(2);
    expect(net.calls[1].body).toMatchObject({ expect: "op-C" });
    expect(texts(net.calls)).toEqual(["one", "two"]);
  });

  it("sends the next write when the HTTP answer comes after the stream already showed a later op ended", async () => {
    const { net, view, queue: q } = queue();
    q.enqueue(S, prompt("one"));
    q.enqueue(S, prompt("two"));
    view.show({ id: "op-A", status: "completed" });
    q.update(S);
    expect(net.calls).toHaveLength(1);
    // The stream runs ahead of the HTTP answer: a later op has already ended.
    view.show({ id: "op-C", status: "completed" });
    q.update(S);
    expect(net.calls).toHaveLength(1);
    // The answer arrives; no further stream event or update follows.
    net.calls[0].respond(202, { op: "op-B" });
    await flush();
    expect(net.calls).toHaveLength(2);
    expect(net.calls[1].body).toMatchObject({ expect: "op-C" });
    expect(texts(net.calls)).toEqual(["one", "two"]);
  });

  it("lets an admitted write leave the queue although its operation later fails, never retried as a refusal", async () => {
    const { net, view, queue: q } = queue();
    q.enqueue(S, { kind: "fork", args: {} });
    view.show({ id: "op-A", status: "completed" });
    q.update(S);
    net.calls[0].respond(202, { op: "op-F" });
    await flush();
    expect(q.items(S)).toEqual([]);
    // The document shows the operation failed; that is its result, not a refusal to send again.
    view.show({ id: "op-F", kind: "fork", status: "failed" });
    q.update(S);
    view.show({ id: "op-G", status: "completed" });
    q.update(S);
    expect(net.calls).toHaveLength(1);
  });

  it("sends a definitely refused write again only once a later op has ended", async () => {
    const { net, view, queue: q } = queue();
    q.enqueue(S, prompt("p"));
    view.show({ id: "op-A", status: "completed" });
    q.update(S);
    net.calls[0].respond(409, { error: { code: "stale", message: "stale" } });
    await flush();
    expect(q.items(S)[0]).toMatchObject({ state: "refused", error: { code: "stale" } });

    q.update(S);
    view.show({ id: "op-B", kind: "prompt", status: "running" });
    q.update(S);
    expect(net.calls).toHaveLength(1);
    view.show({ id: "op-B", kind: "prompt", status: "completed" });
    q.update(S);
    expect(net.calls).toHaveLength(2);
    expect(net.calls[1].body).toMatchObject({ expect: "op-B" });
  });

  it("sends new corrected input against the same ended op after the user removes a refused write", async () => {
    const { net, view, queue: q } = queue();
    view.show({ id: "op-A", status: "completed" });
    const missingAttachment: Extract<OpArgs, { kind: "prompt" }> = {
      kind: "prompt", args: { prompt: [{ type: "attachment", id: "0".repeat(64), block: "image" }] },
    };
    const refused = q.enqueue(S, missingAttachment);
    expect(net.calls).toHaveLength(1);
    net.calls[0].respond(400, { error: { code: "invalid_request", message: "unknown attachment" } });
    await flush();
    expect(q.items(S)).toEqual([
      { id: refused, write: missingAttachment, state: "refused", error: { code: "invalid_request", message: "unknown attachment" } },
    ]);

    // The rejected item is not automatically retried against the unchanged op.
    q.update(S);
    await flush();
    expect(net.calls).toHaveLength(1);
    expect(view.source.get(S)?.doc.live).toEqual(sessionLive({ id: "op-A", status: "completed" }));

    q.remove(S, refused);
    expect(q.items(S)).toEqual([]);
    // This is new user input, not a retry of the removed item. No SSE follows.
    q.enqueue(S, prompt("corrected prompt"));
    q.update(S);
    await flush();
    expect(net.calls).toHaveLength(2);
    expect(net.calls[1].body).toMatchObject({
      expect: "op-A", kind: "prompt", args: { prompt: [{ type: "text", text: "corrected prompt" }] },
    });
    net.calls[1].respond(202, { op: "op-B" });
    await flush();
    expect(q.items(S)).toEqual([]);
  });

  it("sends different new input after the user removes an uncertain write while the op stays ended", async () => {
    const { net, view, queue: q } = queue();
    view.show({ id: "op-A", status: "completed" });
    const uncertain = q.enqueue(S, prompt("old input"));
    net.calls[0].lose();
    await flush();
    expect(q.items(S).find((item) => item.id === uncertain)?.state).toBe("uncertain");

    // The request may never have reached the bridge, so no newer op or SSE
    // is guaranteed. Keeping the uncertain item must not resend it.
    q.update(S);
    await flush();
    expect(net.calls).toHaveLength(1);
    q.remove(S, uncertain);
    expect(q.items(S)).toEqual([]);

    // This explicit user input is different from the removed uncertain one.
    q.enqueue(S, prompt("new input"));
    q.update(S);
    await flush();
    expect(texts(net.calls)).toEqual(["old input", "new input"]);
    expect(net.calls[1].body).toMatchObject({
      expect: "op-A", kind: "prompt", args: { prompt: [{ type: "text", text: "new input" }] },
    });
    net.calls[1].respond(202, { op: "op-B" });
    await flush();
    expect(q.items(S)).toEqual([]);
  });

  it("sends different explicit input while an older uncertain write stays for the user to judge", async () => {
    const { net, view, queue: q } = queue();
    view.show({ id: "op-A", status: "completed" });
    const oldInput = prompt("old uncertain input");
    const uncertain = q.enqueue(S, oldInput);
    net.calls[0].lose();
    await flush();
    expect(q.items(S)).toEqual([{ id: uncertain, write: oldInput, state: "uncertain" }]);

    q.update(S);
    await flush();
    expect(net.calls).toHaveLength(1);

    // The old request may never have reached the bridge. The user keeps it
    // for later judgment and explicitly submits different input; no SSE or
    // newer op is guaranteed to arrive before that new input can be sent.
    q.enqueue(S, prompt("different new input"));
    await flush();
    expect(texts(net.calls)).toEqual(["old uncertain input", "different new input"]);
    expect(net.calls[1].body).toMatchObject({
      expect: "op-A", kind: "prompt", args: { prompt: [{ type: "text", text: "different new input" }] },
    });
    net.calls[1].respond(202, { op: "op-B" });
    await flush();
    q.update(S);
    await flush();
    expect(texts(net.calls)).toEqual(["old uncertain input", "different new input"]);
    expect(q.items(S)).toEqual([{ id: uncertain, write: oldInput, state: "uncertain" }]);
  });

  it("never sends a write without a definite answer again, whatever is refused later", async () => {
    const { net, view, queue: q } = queue();
    const uncertain = q.enqueue(S, prompt("maybe"));
    q.enqueue(S, prompt("after"));
    view.show({ id: "op-A", status: "completed" });
    q.update(S);
    net.calls[0].lose();
    await flush();
    expect(q.items(S).find((item) => item.id === uncertain)?.state).toBe("uncertain");

    // Later ops end, and a later write is refused as stale: the uncertain one
    // is not judged unadmitted and stays with the user.
    view.show({ id: "op-B", status: "completed" });
    q.update(S);
    net.calls[1].respond(409, { error: { code: "stale", message: "stale" } });
    await flush();
    view.show({ id: "op-C", status: "completed" });
    q.update(S);
    view.show({ id: "op-D", status: "completed" });
    q.update(S);
    expect(texts(net.calls).filter((text) => text === "maybe")).toHaveLength(1);
    expect(q.items(S).find((item) => item.id === uncertain)?.state).toBe("uncertain");
  });

  it("sends an uncertain write again only when the user resubmits it", async () => {
    const { net, view, queue: q } = queue();
    const id = q.enqueue(S, prompt("maybe"));
    view.show({ id: "op-A", status: "completed" });
    q.update(S);
    net.calls[0].lose();
    await flush();
    view.show({ id: "op-B", status: "completed" });
    q.update(S);
    expect(net.calls).toHaveLength(1);

    q.resubmit(S, id);
    q.update(S);
    expect(net.calls).toHaveLength(2);
    expect(net.calls[1].body).toMatchObject({ expect: "op-B", args: { prompt: [{ text: "maybe" }] } });
  });

  it("never sends a write the user took back", () => {
    const { net, view, queue: q } = queue();
    const id = q.enqueue(S, prompt("gone"));
    q.remove(S, id);
    view.show({ id: "op-A", status: "completed" });
    q.update(S);
    expect(net.calls).toHaveLength(0);
    expect(q.items(S)).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// Resource actions

describe("terminal input and resource actions (§7; acceptance 16)", () => {
  const input = (call: Call) => (call.body as { data: string }).data;

  it("posts input to the terminal's resource and answers done on 204", async () => {
    const net = http();
    const resources = new ResourceClient(net.fetch);
    const sent = resources.input("global", "T1", "ls\r");
    expect(net.calls[0]).toMatchObject({ url: "/api/v2/targets/global/resources/T1/input", method: "POST", body: { data: "ls\r" } });
    net.calls[0].respond(204);
    expect(await sent).toEqual({ kind: "done" });
  });

  it("sends one input at a time per terminal, in order", async () => {
    const net = http();
    const resources = new ResourceClient(net.fetch);
    const a = resources.input("global", "T1", "a");
    const b = resources.input("global", "T1", "b");
    await flush();
    expect(net.calls.map(input)).toEqual(["a"]);
    net.calls[0].respond(204);
    await flush();
    expect(net.calls.map(input)).toEqual(["a", "b"]);
    net.calls[1].respond(204);
    expect(await a).toEqual({ kind: "done" });
    expect(await b).toEqual({ kind: "done" });
  });

  it("does not hold one terminal's input behind another's", async () => {
    const net = http();
    const resources = new ResourceClient(net.fetch);
    void resources.input("global", "T1", "a");
    void resources.input("global", "T2", "b");
    await flush();
    expect(net.calls.map((call) => call.url)).toEqual([
      "/api/v2/targets/global/resources/T1/input",
      "/api/v2/targets/global/resources/T2/input",
    ]);
  });

  it("never resends input whose response was lost; the same text typed again is new input", async () => {
    const net = http();
    const resources = new ResourceClient(net.fetch);
    const lost = resources.input("global", "T1", "y\r");
    const next = resources.input("global", "T1", "n");
    await flush();
    net.calls[0].lose();
    expect(await lost).toEqual({ kind: "uncertain" });
    await flush();
    expect(net.calls.map(input)).toEqual(["y\r", "n"]);
    net.calls[1].respond(204);
    expect(await next).toEqual({ kind: "done" });

    const again = resources.input("global", "T1", "y\r");
    await flush();
    expect(net.calls.map(input)).toEqual(["y\r", "n", "y\r"]);
    net.calls[2].respond(204);
    expect(await again).toEqual({ kind: "done" });
    await flush();
    expect(net.calls).toHaveLength(3);
  });

  it("sends resize and dismiss bodies and reports a missing resource as refused", async () => {
    const net = http();
    const resources = new ResourceClient(net.fetch);
    const resized = resources.resize("global", "T1", 120, 40);
    await flush();
    expect(net.calls[0]).toMatchObject({ url: "/api/v2/targets/global/resources/T1/resize", body: { cols: 120, rows: 40 } });
    net.calls[0].respond(204);
    expect(await resized).toEqual({ kind: "done" });

    const dismissed = resources.dismiss(S, "flow-1");
    await flush();
    expect(net.calls[1]).toMatchObject({ url: "/api/v2/targets/sessions/S/resources/flow-1/dismiss", body: {} });
    net.calls[1].respond(404, { error: { code: "not_found", message: "no resource flow-1" } });
    expect(await dismissed).toMatchObject({ kind: "refused", error: { code: "not_found" } });
  });
});
