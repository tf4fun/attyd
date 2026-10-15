// The HTTP and stream client of protocol v2 (§5–§7, C1–C3; migration step 7,
// acceptance 4, 9, 15 and 16, client half). The transport is faked so each
// case fixes the order of HTTP answers and stream events itself.
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Event, LogEntry, OpArgs } from "../shared/protocol";
import {
  OpsClient,
  ReplicaClient,
  ResourceClient,
  WriteQueue,
  type HttpFetch,
  type ReplicaSource,
  type StreamSource,
} from "../shared/protocol-client";
import { H0, type Replica } from "../shared/replica";

const S = "session/S";
const OPS = "/api/v2/scopes/session%2FS/ops";

const flush = async () => {
  for (let i = 0; i < 20; i += 1) await Promise.resolve();
};

// ---------------------------------------------------------------------------
// Fakes

class FakeSource implements StreamSource {
  onmessage: ((message: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;
  constructor(readonly url: string) {}
  close() {
    this.closed = true;
  }
  emit(event: Event) {
    this.onmessage?.({ data: JSON.stringify(event) });
  }
  emitRaw(data: string) {
    this.onmessage?.({ data });
  }
  fail() {
    this.onerror?.();
  }
}

function streams() {
  const sources: FakeSource[] = [];
  const scheduled: Array<() => void> = [];
  return {
    sources,
    scheduled,
    open: (url: string) => {
      const source = new FakeSource(url);
      sources.push(source);
      return source;
    },
    schedule: (reconnect: () => void) => {
      scheduled.push(reconnect);
    },
    runScheduled() {
      scheduled.splice(0).forEach((run) => run());
    },
  };
}

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

function have(url: string): string[] {
  return new URL(url, "http://bridge.test").searchParams.getAll("have");
}

function entry(text: string, hash: string): LogEntry {
  return { turn: { prompt: [{ type: "text", text }], reply: [] }, hash };
}

const e1 = entry("one", "h1-aaaaaaaaaaaaaaaaaaa");
const e2 = entry("two", "h2-aaaaaaaaaaaaaaaaaaa");

function sessionLive(op: Record<string, unknown> = {}) {
  return {
    op: { id: "op-A", kind: "none", status: "completed", input: null, cancelRequested: false, interactions: {}, ...op },
    urlFlows: {}, sessionId: "S", cwd: "/work", terminals: {},
  };
}

/** Replicas the writers read, set directly by each case. */
function shown() {
  const replicas = new Map<string, Replica>();
  const source: ReplicaSource = { get: (scope) => replicas.get(scope) };
  return {
    source,
    replicas,
    show(op: Record<string, unknown>, scope = S) {
      replicas.set(scope, { scope, log: [], confirmed: true, live: sessionLive(op) });
    },
  };
}

const prompt = (text: string): Extract<OpArgs, { kind: "prompt" }> => ({ kind: "prompt", args: { prompt: [{ type: "text", text }] } });

afterEach(() => {
  vi.useRealTimers();
});

// ---------------------------------------------------------------------------
// Stream

describe("the stream client (§5.1, §5.4; acceptance 4)", () => {
  it("closes the stream it replaces and ignores that stream's late callbacks", () => {
    const fake = streams();
    const client = new ReplicaClient(fake.open, fake.schedule);
    client.follow([{ scope: S, cwd: "/work" }]);
    const [old] = fake.sources;
    old.emit({ scope: S, log: { from: 0, prev: H0, entries: [e1] }, live: { replace: sessionLive() } });

    client.follow([{ scope: "global" }, { scope: S, cwd: "/work" }]);
    expect(old.closed).toBe(true);
    expect(fake.sources).toHaveLength(2);
    expect(have(fake.sources[1].url)).toEqual([`${S}:1:${e1.hash}`]);

    old.emit({ scope: S, log: { from: 1, prev: e1.hash, entries: [e2] } });
    old.fail();
    fake.runScheduled();
    expect(client.get(S)?.log).toEqual([e1]);
    expect(fake.sources).toHaveLength(2);
  });

  it("closes a broken stream itself and subscribes again with the head held then", () => {
    const fake = streams();
    const client = new ReplicaClient(fake.open, fake.schedule);
    client.follow([{ scope: S, cwd: "/work" }]);
    const [first] = fake.sources;
    expect(have(first.url)).toEqual([]);
    first.emit({ scope: S, log: { from: 0, prev: H0, entries: [e1, e2] }, live: { replace: sessionLive() } });

    first.fail();
    // EventSource must not retry the original URL, whose head is no longer held.
    expect(first.closed).toBe(true);
    expect(fake.sources).toHaveLength(1);
    fake.runScheduled();
    expect(fake.sources).toHaveLength(2);
    expect(have(fake.sources[1].url)).toEqual([`${S}:2:${e2.hash}`]);
    expect(client.get(S)?.log).toEqual([e1, e2]);
  });

  it.each<[string, (source: FakeSource) => void]>([
    ["an event it cannot apply", (source) => source.emit({ scope: S, log: { from: 5, prev: e2.hash, entries: [] } })],
    ["a message that is not an event", (source) => source.emitRaw("{not json")],
  ])("subscribes again after %s, keeping what it holds", (_name, deliver) => {
    const fake = streams();
    const client = new ReplicaClient(fake.open, fake.schedule);
    client.follow([{ scope: S, cwd: "/work" }]);
    const [first] = fake.sources;
    first.emit({ scope: S, log: { from: 0, prev: H0, entries: [e1] }, live: { replace: sessionLive() } });
    deliver(first);
    expect(first.closed).toBe(true);
    fake.runScheduled();
    expect(fake.sources).toHaveLength(2);
    expect(have(fake.sources[1].url)).toEqual([`${S}:1:${e1.hash}`]);
    expect(client.get(S)?.log).toEqual([e1]);
  });

  it("tells its listeners which scope changed or ended", () => {
    const fake = streams();
    const client = new ReplicaClient(fake.open, fake.schedule);
    const seen: string[] = [];
    client.subscribe((scope) => seen.push(scope));
    client.follow([{ scope: "global" }, { scope: S, cwd: "/work" }]);
    const [source] = fake.sources;
    source.emit({ scope: S, log: { from: 0, prev: H0, entries: [] }, live: { replace: sessionLive() } });
    source.emit({ scope: "global", live: { replace: { op: {}, urlFlows: {}, sessions: {} } } });
    source.emit({ scope: S, end: { reason: "deleted" } });
    expect(seen).toEqual([S, "global", S]);
    expect(client.get(S)).toBeUndefined();
  });

  it("closes a healthy stream and lets nothing of it through afterwards", () => {
    const fake = streams();
    const client = new ReplicaClient(fake.open, fake.schedule);
    client.follow([{ scope: S, cwd: "/work" }]);
    const [source] = fake.sources;
    source.emit({ scope: S, log: { from: 0, prev: H0, entries: [e1] }, live: { replace: sessionLive() } });
    const held = client.get(S);

    client.close();
    expect(source.closed).toBe(true);
    source.emit({ scope: S, log: { from: 1, prev: e1.hash, entries: [e2] } });
    source.fail();
    fake.runScheduled();
    expect(client.get(S)).toBe(held);
    expect(client.get(S)?.log).toEqual([e1]);
    expect(fake.sources).toHaveLength(1);
  });

  it("drops a reconnect queued by a broken stream once a new set is followed", () => {
    const fake = streams();
    const client = new ReplicaClient(fake.open, fake.schedule);
    client.follow([{ scope: S, cwd: "/work" }]);
    const [first] = fake.sources;
    first.emit({ scope: S, log: { from: 0, prev: H0, entries: [e1] }, live: { replace: sessionLive() } });
    first.fail();
    expect(fake.scheduled).toHaveLength(1);

    client.follow([{ scope: "global" }]);
    expect(fake.sources).toHaveLength(2);
    const [, second] = fake.sources;
    // The queued reconnect of the broken stream runs late.
    fake.runScheduled();
    expect(fake.sources).toHaveLength(2);
    expect(second.closed).toBe(false);
    expect(new URL(second.url, "http://bridge.test").searchParams.getAll("scope")).toEqual(["global"]);
    second.emit({ scope: "global", live: { replace: { op: {}, urlFlows: {}, sessions: {} } } });
    expect(client.get("global")).toBeDefined();
  });

  it("opens nothing after it is closed", () => {
    const fake = streams();
    const client = new ReplicaClient(fake.open, fake.schedule);
    client.follow([{ scope: S, cwd: "/work" }]);
    fake.sources[0].fail();
    client.close();
    fake.runScheduled();
    expect(fake.sources).toHaveLength(1);
    expect(fake.sources[0].closed).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Operation writes

describe("operation writes (§6; acceptance 9)", () => {
  it("sends the kind and args with expect set to the op the replica shows, and changes no replica", async () => {
    const net = http();
    const view = shown();
    view.show({ id: "op-A", status: "completed" });
    const before = view.source.get(S);
    const ops = new OpsClient(net.fetch, view.source);

    const sent = ops.write(S, prompt("hello"));
    expect(net.calls).toHaveLength(1);
    expect(net.calls[0]).toMatchObject({
      url: OPS,
      method: "POST",
      body: { expect: "op-A", kind: "prompt", args: { prompt: [{ type: "text", text: "hello" }] } },
    });
    expect(Object.entries(net.calls[0].headers).map(([k, v]) => [k.toLowerCase(), v]))
      .toContainEqual(["content-type", "application/json"]);
    net.calls[0].respond(202, { op: "op-B" });
    // Admitted before the stream shows it: the write waits on nothing more.
    expect(await sent).toEqual({ kind: "accepted", op: "op-B" });
    expect(view.source.get(S)).toBe(before);
    expect(view.source.get(S)?.live).toEqual(sessionLive({ id: "op-A", status: "completed" }));
  });

  it("returns create's session from its own response although op was already replaced", async () => {
    const net = http();
    const view = shown();
    view.show({ id: "g-1", status: "completed" }, "global");
    const ops = new OpsClient(net.fetch, view.source);
    const created = ops.write("global", { kind: "create", args: { cwd: "/work" } });
    expect(net.calls[0]).toMatchObject({ url: "/api/v2/scopes/global/ops", body: { expect: "g-1", kind: "create" } });
    view.show({ id: "g-3", kind: "authenticate", status: "running" }, "global");
    net.calls[0].respond(200, { op: "g-2", result: { sessionId: "NEW" } });
    expect(await created).toEqual({ kind: "ended", op: "g-2", outcome: { result: { sessionId: "NEW" } } });
  });

  it("gives create, fork and list no short timeout", async () => {
    vi.useFakeTimers();
    const net = http();
    const view = shown();
    view.show({ id: "op-A", status: "completed" });
    const ops = new OpsClient(net.fetch, view.source);
    let outcome: unknown;
    void ops.write(S, { kind: "fork", args: {} }).then((value) => {
      outcome = value;
    });
    await vi.advanceTimersByTimeAsync(30 * 60 * 1000);
    expect(outcome).toBeUndefined();
    expect(net.calls).toHaveLength(1);
    net.calls[0].respond(200, { op: "op-F", result: { sessionId: "FORK" } });
    await vi.advanceTimersByTimeAsync(0);
    expect(outcome).toEqual({ kind: "ended", op: "op-F", outcome: { result: { sessionId: "FORK" } } });
  });

  it("tells a definite refusal from a write without a definite answer", async () => {
    const net = http();
    const view = shown();
    view.show({ id: "op-A", status: "completed" });
    const ops = new OpsClient(net.fetch, view.source);

    const stale = ops.write(S, prompt("a"));
    net.calls[0].respond(409, { error: { code: "stale", message: "expect is not op.id" } });
    expect(await stale).toEqual({ kind: "refused", error: { code: "stale", message: "expect is not op.id" } });

    const unavailable = ops.write(S, prompt("b"));
    net.calls[1].respond(503, { error: { code: "unavailable", message: "no Agent connection" } });
    expect(await unavailable).toMatchObject({ kind: "refused", error: { code: "unavailable" } });

    const lost = ops.write(S, prompt("c"));
    net.calls[2].lose();
    expect(await lost).toEqual({ kind: "uncertain" });

    const proxy = ops.write(S, prompt("d"));
    net.calls[3].respond(502);
    expect(await proxy).toEqual({ kind: "uncertain" });
  });

  it("reports an operation that ended in failure as ended, not refused", async () => {
    const net = http();
    const view = shown();
    view.show({ id: "g-1", status: "completed" }, "global");
    const ops = new OpsClient(net.fetch, view.source);
    const created = ops.write("global", { kind: "create", args: { cwd: "/work" } });
    const error = { code: "agent_error", message: "session/new failed", data: { code: -32603, message: "boom" } };
    net.calls[0].respond(200, { op: "g-2", error });
    expect(await created).toEqual({ kind: "ended", op: "g-2", outcome: { error } });
  });

  it.each<[string, number, unknown]>([
    ["a 202 without a body", 202, undefined],
    ["a 202 without its op", 202, {}],
    ["a 200 with neither result nor error", 200, { op: "g-2" }],
  ])("treats %s as a write without a definite answer", async (_name, status, body) => {
    const net = http();
    const view = shown();
    view.show({ id: "g-1", status: "completed" }, "global");
    const ops = new OpsClient(net.fetch, view.source);
    const sent = ops.write("global", { kind: "create", args: { cwd: "/work" } });
    net.calls[0].respond(status, body);
    expect(await sent).toEqual({ kind: "uncertain" });
  });

  it("sends nothing to a scope it holds no aligned replica of", async () => {
    const net = http();
    const ops = new OpsClient(net.fetch, shown().source);
    expect((await ops.write(S, prompt("a"))).kind).toBe("blocked");
    expect(net.calls).toHaveLength(0);
  });
});

describe("answers (§6.4; acceptance 7, client half)", () => {
  const permission = { kind: "permission", interactionId: "p1", request: { sessionId: "S", toolCall: { toolCallId: "t" }, options: [] } };
  const elicitation = { kind: "elicitation", interactionId: "e1", request: { mode: "form", message: "Name?", requestedSchema: {} } };
  const allow = { outcome: { outcome: "selected", optionId: "allow" } } as const;
  const accept = { response: { action: "accept", content: { name: "attyd" } } } as const;
  const full = { p1: allow, e1: accept };

  function running() {
    const net = http();
    const view = shown();
    view.show({ id: "op-P", kind: "prompt", status: "running", interactions: { p1: permission, e1: elicitation } });
    const interactions = structuredClone((view.source.get(S)!.live as ReturnType<typeof sessionLive>).op.interactions);
    const untouched = () => {
      expect((view.source.get(S)!.live as ReturnType<typeof sessionLive>).op.interactions).toEqual(interactions);
    };
    return { net, view, untouched, ops: new OpsClient(net.fetch, view.source) };
  }

  it("sends every pending interaction's whole answer in one write against the running op", async () => {
    const { net, untouched, ops } = running();
    const sent = ops.answer(S, full);
    expect(net.calls).toHaveLength(1);
    expect(net.calls[0].url).toBe(OPS);
    expect(net.calls[0].body).toEqual({ expect: "op-P", kind: "answer", args: { answers: full } });
    net.calls[0].respond(202, { op: "op-P" });
    expect(await sent).toEqual({ kind: "accepted", op: "op-P" });
    // The interactions leave only when the stream says so.
    untouched();
  });

  it.each([
    ["one missing", { p1: allow }],
    ["one more than pending", { ...full, p3: allow }],
  ])("is blocked, unsent, with %s", async (_name, answers) => {
    const { net, ops } = running();
    expect((await ops.answer(S, answers)).kind).toBe("blocked");
    expect(net.calls).toHaveLength(0);
  });

  it("leaves the user's answers and the replica as they were when the bridge refuses them", async () => {
    const { net, untouched, ops } = running();
    const answers = structuredClone(full);
    const sent = ops.answer(S, answers);
    net.calls[0].respond(409, { error: { code: "not_allowed", message: "the pending interactions changed" } });
    expect(await sent).toMatchObject({ kind: "refused", error: { code: "not_allowed" } });
    expect(answers).toEqual(full);
    untouched();
  });
});

// ---------------------------------------------------------------------------
// Queued writes

describe("queued writes (C2, C3; acceptance 15)", () => {
  function queue() {
    const net = http();
    const view = shown();
    view.show({ id: "op-A", kind: "prompt", status: "running" });
    const ops = new OpsClient(net.fetch, view.source);
    return { net, view, queue: new WriteQueue(ops, view.source) };
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

  it("lets an operation that ended in failure leave the queue, never retried as a refusal", async () => {
    const { net, view, queue: q } = queue();
    q.enqueue(S, { kind: "fork", args: {} });
    view.show({ id: "op-A", status: "completed" });
    q.update(S);
    net.calls[0].respond(200, { op: "op-F", error: { code: "agent_error", message: "fork failed" } });
    await flush();
    expect(q.items(S)).toEqual([]);
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
    expect(view.source.get(S)?.live).toEqual(sessionLive({ id: "op-A", status: "completed" }));

    q.remove(S, refused);
    expect(q.items(S)).toEqual([]);
    // This is new user input, not a retry of the removed item. No SSE follows.
    q.enqueue(S, prompt("corrected prompt"));
    q.update(S);
    await flush();
    expect(net.calls).toHaveLength(2);
    expect(net.calls[1].body).toEqual({
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
    expect(net.calls[1].body).toEqual({
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
    expect(net.calls[1].body).toEqual({
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
    expect(net.calls[0]).toMatchObject({ url: "/api/v2/scopes/global/resources/T1/input", method: "POST", body: { data: "ls\r" } });
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
      "/api/v2/scopes/global/resources/T1/input",
      "/api/v2/scopes/global/resources/T2/input",
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
    expect(net.calls[0]).toMatchObject({ url: "/api/v2/scopes/global/resources/T1/resize", body: { cols: 120, rows: 40 } });
    net.calls[0].respond(204);
    expect(await resized).toEqual({ kind: "done" });

    const dismissed = resources.dismiss(S, "flow-1");
    await flush();
    expect(net.calls[1]).toMatchObject({ url: "/api/v2/scopes/session%2FS/resources/flow-1/dismiss", body: {} });
    net.calls[1].respond(404, { error: { code: "not_found", message: "no resource flow-1" } });
    expect(await dismissed).toMatchObject({ kind: "refused", error: { code: "not_found" } });
  });
});
