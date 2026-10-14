import { describe, expect, it } from "vitest";
import type { Entry, GlobalState, Op, Position, SessionState } from "../shared/protocol";
import {
  Replica,
  ReplicaError,
  applyOp,
  applyOps,
  entryScope,
  sessionSubscription,
} from "../shared/replica";

const EPOCH = "e1";

function at(seq: number, epoch = EPOCH): Position {
  return { epoch, seq };
}

function globalState(overrides: Partial<GlobalState> = {}): GlobalState {
  return {
    agent: {
      phase: "ready",
      error: null,
      launch: {
        transport: "stdio", command: ["agent"], cwd: "/work", readOnly: false,
        additionalDirectories: [], mcpServers: [],
      },
      initialize: null,
      authenticatedMethodId: null,
      predecessor: null,
    },
    catalog: { revision: "1" },
    sessions: {},
    requests: {},
    interactions: {},
    urlFlows: {},
    ...overrides,
  };
}

function sessionState(overrides: Partial<SessionState> = {}): SessionState {
  return {
    sessionId: "S",
    incarnation: 1,
    cwd: "/work",
    controls: { modes: null, configOptions: null, commands: [], info: { title: null, updatedAt: null, meta: null }, usage: null },
    history: { revision: "r1", phase: "ready", error: null, notice: null, timeline: [], collapsedTurns: [] },
    turn: null,
    outcomes: {},
    operation: null,
    interactions: {},
    urlFlows: {},
    terminals: {},
    ...overrides,
  };
}

const S = sessionSubscription("S", EPOCH, 1);

function send(replica: Replica, entry: Entry, withId = true): void {
  replica.receive(JSON.stringify(entry), withId ? `${entry.at.epoch}:${entry.at.seq}` : "");
}

function snapshot(scope: string, seq: number, state: unknown): Entry {
  return { type: "snapshot", scope, at: at(seq), state } as Entry;
}

function ops(scope: string, seq: number, prev: number, list: Op[]): Entry {
  return { type: "ops", scope, at: at(seq), prev, ops: list };
}

describe("JSON Pointer ops", () => {
  it("creates and replaces with set, and modifies an array index in place", () => {
    const doc = { a: { b: 1 }, list: [1, 2, 3] };
    expect(applyOp(doc, { op: "set", path: "/a/c", value: 2 })).toEqual({ a: { b: 1, c: 2 }, list: [1, 2, 3] });
    expect(applyOp(doc, { op: "set", path: "/a/b", value: 5 })).toEqual({ a: { b: 5 }, list: [1, 2, 3] });
    expect(applyOp(doc, { op: "set", path: "/list/1", value: 9 })).toEqual({ a: { b: 1 }, list: [1, 9, 3] });
    expect(applyOp(doc, { op: "set", path: "", value: 7 })).toBe(7);
  });

  it("never mutates its input and shares untouched subtrees", () => {
    const untouched = { deep: [1] };
    const doc = { a: { b: 1 }, untouched };
    const next = applyOp(doc, { op: "set", path: "/a/b", value: 2 }) as typeof doc;
    expect(doc.a.b).toBe(1);
    expect(next.untouched).toBe(untouched);
    expect(next).not.toBe(doc);
  });

  it("removes keys and array items, appends values and text, and trims", () => {
    const doc = { map: { x: 1, y: 2 }, list: [1, 2, 3], text: "héllo" };
    expect(applyOp(doc, { op: "remove", path: "/map/x" })).toMatchObject({ map: { y: 2 } });
    expect(applyOp(doc, { op: "remove", path: "/list/0" })).toMatchObject({ list: [2, 3] });
    expect(applyOp(doc, { op: "append", path: "/list", values: [4, 5] })).toMatchObject({ list: [1, 2, 3, 4, 5] });
    expect(applyOp(doc, { op: "append", path: "/text", text: "!" })).toMatchObject({ text: "héllo!" });
    expect(applyOp(doc, { op: "trim", path: "/list", count: 2 })).toMatchObject({ list: [3] });
    expect(applyOp(doc, { op: "trim", path: "/text", count: 2 })).toMatchObject({ text: "llo" });
  });

  it("decodes ~0 and ~1 in pointer tokens", () => {
    const doc = { "a/b": { "c~d": 1 } };
    expect(applyOp(doc, { op: "set", path: "/a~1b/c~0d", value: 2 })).toEqual({ "a/b": { "c~d": 2 } });
  });

  it("rejects every op whose target or parent is missing or mistyped", () => {
    const doc = { map: {}, list: [1], text: "😀x" };
    const invalid: Op[] = [
      { op: "set", path: "/missing/leaf", value: 1 },
      { op: "set", path: "/list/1", value: 1 },
      { op: "set", path: "/list/01", value: 1 },
      { op: "remove", path: "/map/x" },
      { op: "remove", path: "" },
      { op: "append", path: "/map", values: [1] },
      { op: "append", path: "/list", text: "x" },
      { op: "append", path: "/nothing", text: "x" },
      { op: "trim", path: "/list", count: 2 },
      { op: "trim", path: "/text", count: 1 },
      { op: "trim", path: "/text", count: 4 },
      { op: "set", path: "no-slash", value: 1 },
      { op: "set", path: "/bad~2", value: 1 },
    ];
    for (const op of invalid) expect(() => applyOp(doc, op), JSON.stringify(op)).toThrow(ReplicaError);
  });

  it("applies a list of ops in order", () => {
    expect(applyOps({ list: [] }, [
      { op: "append", path: "/list", values: ["a"] },
      { op: "append", path: "/list/0", text: "b" },
    ])).toEqual({ list: ["ab"] });
  });
});

describe("Replica", () => {
  it("names session subscriptions with their full owner", () => {
    expect(S).toBe("session/S@e1:1");
    expect(entryScope(S)).toBe("session/S");
    expect(entryScope("global")).toBe("global");
  });

  it("starts every scope pending: applied waits for the session snapshot", () => {
    const replica = new Replica();
    replica.subscribe(["global", S]);
    send(replica, snapshot("global", 20, globalState()), false);
    send(replica, { type: "reset", scope: "session/S", at: at(20) });
    expect(replica.received).toBe(20);
    expect(replica.applied()).toBe(0);
    expect(replica.fetches()).toEqual([S]);
    send(replica, ops("session/S", 21, 20, [{ op: "set", path: "/cwd", value: "/other" }]));
    expect(replica.applied()).toBe(0);
    expect(replica.document("session/S")).toBeUndefined();

    expect(replica.install(S, { at: at(20), state: sessionState() })).toBe(true);
    expect(replica.fetches()).toEqual([]);
    expect(replica.applied()).toBe(21);
    expect(replica.document<SessionState>("session/S")?.cwd).toBe("/other");
  });

  it("ignores entries covered by an HTTP snapshot that overtook them", () => {
    const replica = new Replica();
    replica.subscribe([S]);
    send(replica, { type: "reset", scope: "session/S", at: at(10) });
    replica.install(S, { at: at(20), state: sessionState({ cwd: "/at-20" }) });
    for (let seq = 11; seq <= 20; seq += 1) {
      send(replica, ops("session/S", seq, seq - 1, [{ op: "set", path: "/cwd", value: `/stale-${seq}` }]));
    }
    expect(replica.document<SessionState>("session/S")?.cwd).toBe("/at-20");
    expect(replica.fetches()).toEqual([]);
    // After a snapshot, any earlier prev continues the scope.
    send(replica, ops("session/S", 22, 15, [{ op: "set", path: "/cwd", value: "/at-22" }]));
    expect(replica.document<SessionState>("session/S")?.cwd).toBe("/at-22");
    expect(replica.applied()).toBe(22);
  });

  it("treats a prev gap as a reset and fetches the scope again", () => {
    const replica = new Replica();
    replica.subscribe(["global"]);
    send(replica, snapshot("global", 5, globalState()));
    send(replica, ops("global", 6, 5, [{ op: "set", path: "/catalog/revision", value: "2" }]));
    send(replica, ops("global", 9, 7, [{ op: "set", path: "/catalog/revision", value: "3" }]));
    expect(replica.fetches()).toEqual(["global"]);
    expect(replica.applied()).toBe(6);
    expect(replica.document<GlobalState>("global")?.catalog.revision).toBe("2");
    replica.install("global", { at: at(9), state: globalState({ catalog: { revision: "3" } }) });
    expect(replica.applied()).toBe(9);
  });

  it("resets a scope whose ops do not apply or whose result is invalid", () => {
    const replica = new Replica();
    replica.subscribe(["global"]);
    send(replica, snapshot("global", 1, globalState()));
    send(replica, ops("global", 2, 1, [{ op: "remove", path: "/sessions/missing" }]));
    expect(replica.fetches()).toEqual(["global"]);
    replica.install("global", { at: at(2), state: globalState() });
    send(replica, ops("global", 3, 2, [{ op: "set", path: "/sessions", value: 5 }]));
    expect(replica.fetches()).toEqual(["global"]);
    expect(replica.document<GlobalState>("global")?.sessions).toEqual({});
  });

  it("rejects invalid snapshots", () => {
    const replica = new Replica();
    replica.subscribe(["global"]);
    send(replica, snapshot("global", 1, { agent: null }));
    expect(replica.document("global")).toBeUndefined();
    expect(replica.fetches()).toEqual(["global"]);
    expect(replica.install("global", { at: at(1), state: { nope: true } })).toBe(false);
  });

  it("never advances applied on a reset: applied 10, commit 15, reset@20", () => {
    const replica = new Replica();
    replica.subscribe(["global", S]);
    send(replica, snapshot("global", 10, globalState()), false);
    send(replica, snapshot("session/S", 10, sessionState()));
    expect(replica.applied()).toBe(10);
    send(replica, { type: "reset", scope: "session/S", at: at(20) });
    expect(replica.applied()).toBe(10);
    let applied = false;
    void replica.whenApplied(at(15)).then(() => { applied = true; });
    replica.install(S, { at: at(20), state: sessionState() });
    return Promise.resolve().then(() => expect(applied).toBe(true));
  });

  it("only counts positions whose last entry carried an SSE id", () => {
    const replica = new Replica();
    replica.subscribe(["global", S]);
    send(replica, snapshot("global", 10, globalState()));
    send(replica, snapshot("session/S", 10, sessionState()));
    send(replica, ops("global", 20, 10, [{ op: "set", path: "/catalog/revision", value: "2" }]), false);
    expect(replica.applied()).toBe(10);
    send(replica, { type: "reset", scope: "session/S", at: at(20) });
    expect(replica.applied()).toBe(10);
  });

  it("ends a scope for good and ignores nothing after it silently", () => {
    const replica = new Replica();
    replica.subscribe([S]);
    send(replica, snapshot("session/S", 3, sessionState()));
    send(replica, { type: "end", scope: "session/S", at: at(4), reason: "closed" });
    expect(replica.ended("session/S")).toBe("closed");
    expect(replica.document("session/S")).toBeUndefined();
    expect(replica.pending("session/S")).toBe(false);
    expect(() => send(replica, ops("session/S", 5, 4, []))).toThrow(ReplicaError);
  });

  it("ignores entries a resumed stream replays, and never rewinds to an older snapshot", () => {
    const replica = new Replica();
    replica.subscribe(["global"]);
    send(replica, snapshot("global", 1, globalState()));
    send(replica, ops("global", 2, 1, [{ op: "set", path: "/catalog/revision", value: "2" }]));
    replica.reconnect("e1:1");
    send(replica, snapshot("global", 1, globalState()));
    send(replica, ops("global", 2, 1, [{ op: "set", path: "/catalog/revision", value: "2" }]));
    expect(replica.document<GlobalState>("global")?.catalog.revision).toBe("2");
    expect(replica.applied()).toBe(2);
  });

  it("drops everything from an old epoch and rejects its pending waits", async () => {
    const replica = new Replica();
    replica.subscribe(["global", S]);
    send(replica, snapshot("global", 5, globalState()));
    send(replica, snapshot("session/S", 5, sessionState()));
    const wait = replica.whenApplied(at(9));
    replica.reconnect("e1:5");
    send(replica, { type: "snapshot", scope: "global", at: at(1, "e2"), state: globalState() });
    expect(replica.epoch).toBe("e2");
    expect(replica.document("session/S")).toBeUndefined();
    await expect(wait).rejects.toThrow(/epoch/);
    await expect(replica.whenApplied(at(1, "e1"))).rejects.toThrow(/epoch/);
  });

  it("keeps showing a re-subscribed scope while it is pending again", () => {
    const replica = new Replica();
    replica.subscribe(["global"]);
    send(replica, snapshot("global", 5, globalState()));
    replica.subscribe(["global", S]);
    expect(replica.document("global")).toBeDefined();
    expect(replica.pending("global")).toBe(true);
    expect(replica.applied()).toBe(0);
    send(replica, snapshot("global", 8, globalState()));
    send(replica, snapshot("session/S", 8, sessionState()));
    expect(replica.applied()).toBe(8);
  });

  it("notifies listeners with the scopes that changed", () => {
    const replica = new Replica();
    const seen: string[][] = [];
    replica.onChange((scopes) => seen.push([...scopes].sort()));
    replica.subscribe(["global"]);
    send(replica, snapshot("global", 1, globalState()));
    send(replica, { type: "position", at: at(2) });
    send(replica, ops("global", 3, 1, [{ op: "set", path: "/catalog/revision", value: "2" }]));
    expect(seen).toEqual([["global"], ["global"]]);
  });

  it("finds an intent in the running turn, then the outcomes, then global requests", () => {
    const replica = new Replica();
    replica.subscribe(["global", S]);
    send(replica, snapshot("global", 1, globalState({ requests: { R1: {
      requestId: "R1", operation: "set_mode", method: "session/set_mode", clientIntentId: "k-mode",
      target: { sessionId: "S", incarnation: 1 }, interactionId: null, methodId: null, params: {}, status: "running",
    } } })));
    send(replica, snapshot("session/S", 1, sessionState({
      turn: { operationId: "op-2", clientIntentId: "k-turn", prompt: [], updates: [], cancelRequested: false },
      outcomes: { "op-1": { operationId: "op-1", clientIntentId: "k-old", status: "completed", afterUpdate: 1 } },
    })));
    expect(replica.findIntent("k-turn", "session/S")).toEqual({ kind: "turn", operationId: "op-2" });
    expect(replica.findIntent("k-old", "session/S")).toMatchObject({ kind: "outcome", outcome: { operationId: "op-1" } });
    expect(replica.findIntent("k-mode", "session/S")).toMatchObject({ kind: "request", request: { requestId: "R1" } });
    expect(replica.findIntent("unknown", "session/S")).toBeUndefined();
  });
});
