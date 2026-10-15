// The generic replica engine (protocol §5, migration step 7; acceptance 3 and
// 4, client half). Every case checks the document after each event.
import { describe, expect, it } from "vitest";
import type { Event, LogEntry, Op } from "../shared/protocol";
import {
  H0,
  PatchError,
  ReplicaSet,
  applyEvent,
  applyOps,
  createReplica,
  headOf,
  type Applied,
  type Replica,
} from "../shared/replica";

const S = "session/S";

function entry(text: string, hash: string): LogEntry {
  return { turn: { prompt: [{ type: "text", text }], reply: [{ type: "text", text: `re ${text}` }] }, hash };
}

const e1 = entry("one", "h1-aaaaaaaaaaaaaaaaaaa");
const e2 = entry("two", "h2-aaaaaaaaaaaaaaaaaaa");
const e3 = entry("three", "h3-aaaaaaaaaaaaaaaaaa");
const e2x = entry("two, replayed differently", "h2-xxxxxxxxxxxxxxxxxxx");
const e3x = entry("three, rebuilt", "h3-xxxxxxxxxxxxxxxxxx");

function live(op: Record<string, unknown> = {}) {
  return {
    op: { id: "op-1", kind: "none", status: "completed", input: null, cancelRequested: false, interactions: {}, ...op },
    urlFlows: {},
    sessionId: "S",
    cwd: "/work",
    terminals: {},
  };
}

function applied(result: Applied): Replica {
  expect(result.kind).toBe("applied");
  if (result.kind !== "applied") throw new Error("unreachable");
  return result.replica;
}

function aligned(log: LogEntry[], value: unknown = live()): Replica {
  return applied(applyEvent(createReplica(S), {
    scope: S, log: { from: 0, prev: H0, entries: log }, live: { replace: value },
  }));
}

describe("live patches (§5.3)", () => {
  const base = () => ({
    op: { id: "a", process: [{ text: "x" }], interactions: { p1: { kind: "permission" } } },
    urlFlows: {},
    output: "abcdef",
    "a/b": { "c~d": 1 },
  });

  it("sets, removes, appends and trims, in order", () => {
    const ops: Op[] = [
      { op: "set", path: "/op/id", value: "b" },
      { op: "append", path: "/op/process", values: [{ text: "y" }, { text: "z" }] },
      { op: "set", path: "/op/process/0", value: { text: "x2" } },
      { op: "remove", path: "/op/interactions/p1" },
      { op: "set", path: "/urlFlows/f1", value: { url: "https://example.test" } },
      { op: "trim", path: "/output", count: 2 },
      { op: "append", path: "/output", text: "gh" },
    ];
    expect(applyOps(base(), ops)).toEqual({
      op: { id: "b", process: [{ text: "x2" }, { text: "y" }, { text: "z" }], interactions: {} },
      urlFlows: { f1: { url: "https://example.test" } },
      output: "cdefgh",
      "a/b": { "c~d": 1 },
    });
  });

  it("trims leading array items and UTF-16 units", () => {
    expect(applyOps({ a: [1, 2, 3], s: "😀xy" }, [
      { op: "trim", path: "/a", count: 2 },
      { op: "trim", path: "/s", count: 2 },
    ])).toEqual({ a: [3], s: "xy" });
  });

  it("decodes escaped pointer tokens", () => {
    expect(applyOps(base(), [{ op: "set", path: "/a~1b/c~0d", value: 2 }])).toMatchObject({ "a/b": { "c~d": 2 } });
  });

  it("does not modify its input and shares untouched values", () => {
    const before = base();
    const snapshot = structuredClone(before);
    const after = applyOps(before, [{ op: "append", path: "/output", text: "!" }]) as typeof before;
    expect(before).toEqual(snapshot);
    expect(after.output).toBe("abcdef!");
    expect(after.op).toBe(before.op);
  });

  it.each<[string, Op]>([
    ["set under a missing parent", { op: "set", path: "/missing/key", value: 1 }],
    ["set on an array index past the end", { op: "set", path: "/op/process/1", value: 1 }],
    ["remove of a missing key", { op: "remove", path: "/op/interactions/p2" }],
    ["append to a missing target", { op: "append", path: "/op/missing", values: [1] }],
    ["append values to a string", { op: "append", path: "/output", values: [1] }],
    ["append text to an array", { op: "append", path: "/op/process", text: "y" }],
    ["trim past the end", { op: "trim", path: "/output", count: 7 }],
    ["a path without a leading slash", { op: "set", path: "op/id", value: "b" }],
  ])("refuses %s", (_name, op) => {
    expect(() => applyOps(base(), [op])).toThrow(PatchError);
  });
});

describe("a replica's head (S3)", () => {
  it("is (0, h_0) for an empty log and the last entry otherwise", () => {
    expect(headOf(createReplica(S))).toEqual({ n: 0, hash: H0 });
    expect(headOf(createReplica(S, [e1, e2]))).toEqual({ n: 2, hash: e2.hash });
  });

  it("starts with its held log unconfirmed and no live", () => {
    const replica = createReplica(S, [e1]);
    expect(replica).toMatchObject({ scope: S, log: [e1], confirmed: false, live: undefined });
  });
});

describe("alignment (S3, §5.2; acceptance 3)", () => {
  it("with a matching head appends only the suffix and confirms the log", () => {
    const held = createReplica(S, [e1, e2]);
    const replica = applied(applyEvent(held, {
      scope: S, log: { from: 2, prev: e2.hash, entries: [e3] }, live: { replace: live() },
    }));
    expect(replica.log).toEqual([e1, e2, e3]);
    expect(replica.confirmed).toBe(true);
    expect(replica.live).toEqual(live());
  });

  it("from 0 replaces a diverged or longer log", () => {
    const held = createReplica(S, [e1, e2x, e3x]);
    const replica = applied(applyEvent(held, {
      scope: S, log: { from: 0, prev: H0, entries: [e1, e2] }, live: { replace: live() },
    }));
    expect(replica.log).toEqual([e1, e2]);
    expect(replica.confirmed).toBe(true);
  });

  it("from a shorter matched index truncates, then appends", () => {
    const held = createReplica(S, [e1, e2x, e3x]);
    const replica = applied(applyEvent(held, {
      scope: S, log: { from: 1, prev: e1.hash, entries: [e2, e3] }, live: { replace: live() },
    }));
    expect(replica.log).toEqual([e1, e2, e3]);
  });

  it("with the same replay keeps the client's log", () => {
    const held = createReplica(S, [e1, e2]);
    const replica = applied(applyEvent(held, {
      scope: S, log: { from: 2, prev: e2.hash, entries: [] }, live: { replace: live() },
    }));
    expect(replica.log).toEqual([e1, e2]);
    expect(replica.confirmed).toBe(true);
  });

  it("of live only keeps a cold reopen's log, unconfirmed, until the load's end confirms it", () => {
    const held = createReplica(S, [e1, e2]);
    const loading = live({ id: "load-1", kind: "load", status: "running", input: "/work" });
    const during = applied(applyEvent(held, { scope: S, live: { replace: loading } }));
    expect(during.log).toEqual([e1, e2]);
    expect(during.confirmed).toBe(false);
    expect(during.live).toEqual(loading);

    // An interaction of the load is shown while the log stays.
    const asked = applied(applyEvent(during, {
      scope: S,
      live: { patch: [{ op: "set", path: "/op/interactions/p1", value: { kind: "permission", interactionId: "p1" } }] },
    }));
    expect(asked.log).toEqual([e1, e2]);
    expect((asked.live as ReturnType<typeof live>).op.interactions).toHaveProperty("p1");

    const loaded = applied(applyEvent(asked, {
      scope: S,
      log: { from: 2, prev: e2.hash, entries: [] },
      live: { patch: [{ op: "set", path: "/op/status", value: "completed" }, { op: "set", path: "/op/interactions", value: {} }] },
    }));
    expect(loaded.log).toEqual([e1, e2]);
    expect(loaded.confirmed).toBe(true);
    expect((loaded.live as ReturnType<typeof live>).op).toMatchObject({ status: "completed", interactions: {} });
  });

  it("at a load's end replaces a history that changed", () => {
    const during = applied(applyEvent(createReplica(S, [e1, e2]), {
      scope: S, live: { replace: live({ kind: "load", status: "running" }) },
    }));
    const loaded = applied(applyEvent(during, {
      scope: S,
      log: { from: 0, prev: H0, entries: [e1, e2x] },
      live: { patch: [{ op: "set", path: "/op/status", value: "completed" }] },
    }));
    expect(loaded.log).toEqual([e1, e2x]);
    expect(loaded.confirmed).toBe(true);
  });
});

describe("log changes (§5.2 step 1)", () => {
  it("applies a commit's entry and its live patch together", () => {
    const running = aligned([e1], live({ id: "op-2", kind: "prompt", status: "running", process: [{ sessionUpdate: "plan" }] }));
    const committed = applied(applyEvent(running, {
      scope: S,
      log: { from: 1, prev: e1.hash, entries: [e2] },
      live: { patch: [{ op: "set", path: "/op/status", value: "completed" }, { op: "remove", path: "/op/process" }] },
    }));
    expect(committed.log).toEqual([e1, e2]);
    expect((committed.live as ReturnType<typeof live>).op).not.toHaveProperty("process");
    expect((committed.live as ReturnType<typeof live>).op.status).toBe("completed");
  });

  it("applies a rebuilt log from the common prefix", () => {
    const replica = applied(applyEvent(aligned([e1, e2, e3]), {
      scope: S, log: { from: 1, prev: e1.hash, entries: [e2x] },
    }));
    expect(replica.log).toEqual([e1, e2x]);
  });

  it.each<[string, Event]>([
    ["from past its log", { scope: S, log: { from: 3, prev: e3.hash, entries: [] } }],
    ["a prev it does not hold", { scope: S, log: { from: 2, prev: e2x.hash, entries: [e3] } }],
    ["a prev other than h_0 at 0", { scope: S, log: { from: 0, prev: e1.hash, entries: [] } }],
  ])("with %s asks to subscribe again and changes nothing", (_name, event) => {
    const before = aligned([e1, e2]);
    const result = applyEvent(before, event);
    expect(result.kind).toBe("resubscribe");
    expect(before.log).toEqual([e1, e2]);
  });

  it("is not applied when its live patch does not apply", () => {
    const before = aligned([e1]);
    const result = applyEvent(before, {
      scope: S,
      log: { from: 1, prev: e1.hash, entries: [e2] },
      live: { patch: [{ op: "remove", path: "/op/process" }] },
    });
    expect(result.kind).toBe("resubscribe");
    expect(before.log).toEqual([e1]);
    expect(before.live).toEqual(live());
  });
});

describe("live changes (§5.2 step 2)", () => {
  it("replace installs the value and patch applies to it", () => {
    const replica = applied(applyEvent(aligned([]), {
      scope: S, live: { patch: [{ op: "set", path: "/cwd", value: "/other" }] },
    }));
    expect((replica.live as ReturnType<typeof live>).cwd).toBe("/other");
    expect(replica.log).toEqual([]);
  });

  it("a patch before any alignment asks to subscribe again", () => {
    const result = applyEvent(createReplica(S), { scope: S, live: { patch: [{ op: "set", path: "/cwd", value: "/x" }] } });
    expect(result.kind).toBe("resubscribe");
  });

  it("a patch that does not apply asks to subscribe again", () => {
    expect(applyEvent(aligned([]), {
      scope: S, live: { patch: [{ op: "trim", path: "/cwd", count: 99 }] },
    }).kind).toBe("resubscribe");
  });
});

describe("end (§5.2 step 3)", () => {
  it("drops the document with its reason", () => {
    const result = applyEvent(aligned([e1]), { scope: S, end: { reason: "deleted" } });
    expect(result).toEqual({ kind: "ended", end: { reason: "deleted" } });
  });
});

function streamParams(url: string): URLSearchParams {
  const parsed = new URL(url, "http://bridge.test");
  expect(parsed.pathname).toBe("/api/v2/stream");
  return parsed.searchParams;
}

describe("the stream of a replica set (§5.1, §5.4; acceptance 4)", () => {
  it("subscribes to its scopes with the head held for each session and its cwd", () => {
    const set = new ReplicaSet();
    const { url } = set.open([{ scope: "global" }, { scope: S, cwd: "/work" }]);
    const params = streamParams(url);
    expect(params.getAll("scope")).toEqual(["global", S]);
    expect(params.getAll("cwd")).toEqual([`${S}:/work`]);
    // Only a session scope's head is named, and only one is held.
    expect(params.getAll("have").filter((have) => !have.startsWith(`${S}:`))).toEqual([]);
  });

  it("subscribes again with the head it holds then, not the one it opened with", () => {
    const set = new ReplicaSet();
    const first = set.open([{ scope: S, cwd: "/work" }]);
    expect(set.receive(first.stream, {
      scope: S, log: { from: 0, prev: H0, entries: [e1, e2] }, live: { replace: live() },
    })).toBe("applied");
    expect(set.receive(first.stream, {
      scope: S, log: { from: 2, prev: e2.hash, entries: [e3] },
    })).toBe("applied");
    // The stream breaks: the next one names the head held now.
    const second = set.open([{ scope: S, cwd: "/work" }]);
    expect(streamParams(second.url).getAll("have")).toEqual([`${S}:3:${e3.hash}`]);
    expect(set.get(S)?.log).toEqual([e1, e2, e3]);
  });

  it("ignores every event of a replaced stream", () => {
    const set = new ReplicaSet();
    const old = set.open([{ scope: S, cwd: "/work" }]);
    expect(set.receive(old.stream, { scope: S, log: { from: 0, prev: H0, entries: [e1] }, live: { replace: live() } })).toBe("applied");
    const current = set.open([{ scope: S, cwd: "/work" }]);
    expect(set.receive(old.stream, { scope: S, log: { from: 1, prev: e1.hash, entries: [e2] } })).toBe("ignored");
    expect(set.receive(old.stream, { scope: S, end: { reason: "deleted" } })).toBe("ignored");
    expect(set.get(S)?.log).toEqual([e1]);
    expect(set.receive(current.stream, { scope: S, log: { from: 1, prev: e1.hash, entries: [e2] }, live: { replace: live() } })).toBe("applied");
    expect(set.get(S)?.log).toEqual([e1, e2]);
  });

  it("applies a coalesced alignment directly, without subscribing again", () => {
    const set = new ReplicaSet();
    const { stream } = set.open([{ scope: S, cwd: "/work" }]);
    set.receive(stream, { scope: S, log: { from: 0, prev: H0, entries: [e1] }, live: { replace: live() } });
    // Consumption paused; a queued commit of e2 was coalesced into an alignment
    // from the baseline that the bridge handed to the output stream.
    const alignment: Event = {
      scope: S, log: { from: 1, prev: e1.hash, entries: [e2, e3] }, live: { replace: live({ id: "op-3" }) },
    };
    expect(set.receive(stream, alignment)).toBe("applied");
    expect(set.get(S)).toMatchObject({ log: [e1, e2, e3], confirmed: true, live: live({ id: "op-3" }) });
  });

  it("asks to subscribe again on an event it cannot apply, keeping what it holds", () => {
    const set = new ReplicaSet();
    const { stream } = set.open([{ scope: S, cwd: "/work" }]);
    set.receive(stream, { scope: S, log: { from: 0, prev: H0, entries: [e1, e2] }, live: { replace: live() } });
    expect(set.receive(stream, { scope: S, log: { from: 2, prev: e2x.hash, entries: [e3] } })).toBe("resubscribe");
    expect(set.get(S)?.log).toEqual([e1, e2]);
    expect(streamParams(set.open([{ scope: S, cwd: "/work" }]).url).getAll("have")).toEqual([`${S}:2:${e2.hash}`]);
  });

  it("drops a scope at its end and ignores whatever follows for it on that stream", () => {
    const set = new ReplicaSet();
    const { stream } = set.open([{ scope: "global" }, { scope: S, cwd: "/work" }]);
    set.receive(stream, { scope: S, log: { from: 0, prev: H0, entries: [e1] }, live: { replace: live() } });
    expect(set.receive(stream, { scope: S, end: { reason: "load_failed", error: { code: "invalid_request", message: "no cwd" } } })).toBe("ended");
    expect(set.get(S)).toBeUndefined();
    expect(set.receive(stream, { scope: S, live: { replace: live() } })).toBe("ignored");
    expect(set.get(S)).toBeUndefined();
  });

  it("ignores events of a scope the stream does not follow", () => {
    const set = new ReplicaSet();
    const { stream } = set.open([{ scope: "global" }]);
    expect(set.receive(stream, { scope: "catalog", live: { replace: { op: {}, urlFlows: {}, revision: "r" } } })).toBe("ignored");
    expect(set.get("catalog")).toBeUndefined();
  });
});

describe("patch ops on the root and on arrays (§5.3)", () => {
  it("set on the root replaces the whole value", () => {
    expect(applyOps({ a: 1 }, [{ op: "set", path: "", value: { b: 2 } }])).toEqual({ b: 2 });
  });

  it("remove of an array index drops that item", () => {
    expect(applyOps({ a: [1, 2, 3] }, [{ op: "remove", path: "/a/1" }])).toEqual({ a: [1, 3] });
  });

  it("refuses remove of an array index past the end", () => {
    expect(() => applyOps({ a: [1] }, [{ op: "remove", path: "/a/1" }])).toThrow(PatchError);
  });
});

describe("a confirmed replica subscribed again while the history is unknown (S1, S3)", () => {
  const loading = live({ id: "load-2", kind: "load", status: "running", input: "/work" });

  function confirmedThenReopened() {
    const set = new ReplicaSet();
    const first = set.open([{ scope: S, cwd: "/work" }]);
    expect(set.receive(first.stream, {
      scope: S, log: { from: 0, prev: H0, entries: [e1, e2] }, live: { replace: live() },
    })).toBe("applied");
    expect(set.get(S)?.confirmed).toBe(true);
    const second = set.open([{ scope: S, cwd: "/work" }]);
    expect(streamParams(second.url).getAll("have")).toEqual([`${S}:2:${e2.hash}`]);
    expect(set.receive(second.stream, { scope: S, live: { replace: loading } })).toBe("applied");
    return { set, stream: second.stream };
  }

  it("keeps its log but no longer counts it as confirmed", () => {
    const { set } = confirmedThenReopened();
    expect(set.get(S)).toMatchObject({ log: [e1, e2], confirmed: false, live: loading });
  });

  it("stays unconfirmed through live patches, the load's interactions included", () => {
    const { set, stream } = confirmedThenReopened();
    expect(set.receive(stream, {
      scope: S,
      live: { patch: [{ op: "set", path: "/op/interactions/p1", value: { kind: "permission", interactionId: "p1" } }] },
    })).toBe("applied");
    expect(set.receive(stream, {
      scope: S, live: { patch: [{ op: "remove", path: "/op/interactions/p1" }] },
    })).toBe("applied");
    expect(set.get(S)).toMatchObject({ log: [e1, e2], confirmed: false });
  });

  it("clears its log only on an explicitly empty log, and confirms it then", () => {
    const { set, stream } = confirmedThenReopened();
    expect(set.receive(stream, {
      scope: S,
      log: { from: 0, prev: H0, entries: [] },
      live: { patch: [{ op: "set", path: "/op/status", value: "completed" }] },
    })).toBe("applied");
    expect(set.get(S)).toMatchObject({ log: [], confirmed: true });
  });

  it("a live patch on a confirmed replica keeps it confirmed", () => {
    const replica = applied(applyEvent(aligned([e1]), {
      scope: S, live: { patch: [{ op: "set", path: "/cwd", value: "/other" }] },
    }));
    expect(replica.confirmed).toBe(true);
  });
});

describe("an event that fails part-way takes no effect (§5.2)", () => {
  const nested = () => live({
    id: "op-2", kind: "prompt", status: "running",
    process: [{ sessionUpdate: "plan", entries: [] }],
    input: { prompt: [{ type: "text", text: "hi" }] },
  });

  function heldSet() {
    const set = new ReplicaSet();
    const { stream } = set.open([{ scope: S, cwd: "/work" }]);
    expect(set.receive(stream, {
      scope: S, log: { from: 0, prev: H0, entries: [e1] }, live: { replace: nested() },
    })).toBe("applied");
    return { set, stream };
  }

  it("leaves log and live unchanged when a later op fails after earlier ones applied", () => {
    const { set, stream } = heldSet();
    const before = set.get(S)!;
    const snapshot = structuredClone({ log: before.log, live: before.live });
    expect(set.receive(stream, {
      scope: S,
      log: { from: 1, prev: e1.hash, entries: [e2] },
      live: { patch: [
        { op: "set", path: "/op/input/prompt/0/text", value: "changed" },
        { op: "append", path: "/op/process", values: [{ sessionUpdate: "plan", entries: [1] }] },
        { op: "set", path: "/op/process/0/entries", value: [9] },
        { op: "remove", path: "/op/interactions/missing" },
      ] },
    })).toBe("resubscribe");
    expect({ log: before.log, live: before.live }).toEqual(snapshot);
    const after = set.get(S)!;
    expect({ log: after.log, live: after.live, confirmed: after.confirmed }).toEqual({ ...snapshot, confirmed: true });
  });

  it("leaves live unchanged when its log part has a bad prev and its patch is valid", () => {
    const { set, stream } = heldSet();
    const snapshot = structuredClone(set.get(S)!.live);
    expect(set.receive(stream, {
      scope: S,
      log: { from: 1, prev: e1x(), entries: [e2] },
      live: { patch: [{ op: "set", path: "/op/status", value: "completed" }] },
    })).toBe("resubscribe");
    expect(set.get(S)!.live).toEqual(snapshot);
    expect(set.get(S)!.log).toEqual([e1]);
  });

  function e1x() {
    return "h1-xxxxxxxxxxxxxxxxxxx";
  }
});

describe("end acts on its own scope and stream only (§5.2 step 3)", () => {
  const globalLive = () => ({ op: { id: "g-1", kind: "none", status: "completed", interactions: {} }, urlFlows: {}, sessions: {} });

  it("keeps the other scopes following, and lets a new stream align the ended scope again", () => {
    const set = new ReplicaSet();
    const first = set.open([{ scope: "global" }, { scope: S, cwd: "/work" }]);
    expect(set.receive(first.stream, { scope: "global", live: { replace: globalLive() } })).toBe("applied");
    expect(set.receive(first.stream, {
      scope: S, log: { from: 0, prev: H0, entries: [e1] }, live: { replace: live() },
    })).toBe("applied");

    expect(set.receive(first.stream, { scope: S, end: { reason: "deleted" } })).toBe("ended");
    expect(set.get(S)).toBeUndefined();
    expect(set.receive(first.stream, {
      scope: "global", live: { patch: [{ op: "set", path: "/sessions/T", value: { sessionId: "T" } }] },
    })).toBe("applied");
    expect(set.get("global")?.live).toMatchObject({ sessions: { T: { sessionId: "T" } } });
    expect(set.receive(first.stream, { scope: S, live: { replace: live() } })).toBe("ignored");

    const second = set.open([{ scope: "global" }, { scope: S, cwd: "/work" }]);
    expect(set.receive(second.stream, {
      scope: S, log: { from: 0, prev: H0, entries: [e2x] }, live: { replace: live({ id: "op-9" }) },
    })).toBe("applied");
    expect(set.get(S)).toMatchObject({ log: [e2x], confirmed: true, live: live({ id: "op-9" }) });

    // A late end of the replaced stream changes nothing.
    expect(set.receive(first.stream, { scope: S, end: { reason: "deleted" } })).toBe("ignored");
    expect(set.get(S)?.log).toEqual([e2x]);
  });
});
