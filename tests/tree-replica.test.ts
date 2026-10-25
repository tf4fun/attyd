// The client's replica of the revision 4 tree (design §6): how it applies a stream's events, what
// it verifies and when it asks for the whole subtree again. Hashes are those of `hashAt`, which
// rests on the primitives checked against the independent vectors (protocol-r4-hash.test.ts) and
// the shape checked against the bridge's tree (tree-shape.test.ts).
import { describe, expect, it } from "vitest";
import { wire } from "../shared/digest";
import {
  type Change,
  TreeReplicaSet,
  type TreeEvent,
  hashAt,
  tokensOf,
} from "../shared/tree-replica";

const PATH = "/sessions/s/entries";
const H = (path: string, value: unknown): string => wire(hashAt(tokensOf(path), value));

function entry(text: string) {
  return { kind: "message", role: "agent", chunks: [{ content: { type: "text", text } }] };
}

function snapshot(path: string, value: unknown, at: number, base: number | null = null, node?: object): Change {
  return { path, snapshot: { base, at, hash: H(path, value), node: (node ?? { v: value }) as never } };
}

function open(follow = [PATH]) {
  const set = new TreeReplicaSet();
  const { stream, url } = set.open(follow.map((path) => ({ path })));
  return { set, stream, url };
}

function event(changes: Change[], rev: number, instance?: string, check?: TreeEvent["check"]): TreeEvent {
  return { ...(instance === undefined ? {} : { instance }), rev, changes, ...(check === undefined ? {} : { check }) };
}

describe("a tree replica", () => {
  it("installs a whole snapshot, verified by its hash, at the path version it states", () => {
    const { set, stream } = open();
    const value = [entry("a")];
    const result = set.receive(stream, event([snapshot(PATH, value, 5)], 100, "I"));
    expect(result).toEqual({ kind: "applied", changed: [PATH] });
    expect(set.get(PATH)).toMatchObject({ value, version: 5, verified: 5 });
    expect(set.instance).toBe("I");
    expect(Object.isFrozen(set.get(PATH)!.value)).toBe(true);
  });

  it("drops a snapshot that does not match its hash and asks for the whole subtree", () => {
    const { set, stream } = open();
    const wrong: Change = { path: PATH, snapshot: { base: null, at: 5, hash: H(PATH, [entry("other")]), node: { v: [entry("a")] } } };
    expect(set.receive(stream, event([wrong], 100, "I"))).toEqual({ kind: "resubscribe", changed: [] });
    expect(set.get(PATH)).toBeUndefined();
    expect(set.lastResubscribe).toMatchObject({ full: true, path: PATH });
    expect(set.consecutiveFailures(PATH)).toBe(1);
    // A snapshot that verifies resets the count.
    const second = set.open([{ path: PATH }]);
    set.receive(second.stream, event([snapshot(PATH, [entry("a")], 5)], 100, "I"));
    expect(set.consecutiveFailures(PATH)).toBe(0);
  });

  it("applies ops that chain from its path version, whatever the commit's revision", () => {
    const { set, stream } = open();
    set.receive(stream, event([snapshot(PATH, [entry("a")], 5)], 100, "I"));
    const ops: Change = { path: PATH, from: 5, ops: [{ op: "append", path: "", values: [entry("b")] }] };
    expect(set.receive(stream, event([ops], 120))).toEqual({ kind: "applied", changed: [PATH] });
    expect(set.get(PATH)).toMatchObject({ version: 120, verified: 5 });
    expect(set.get(PATH)!.value).toEqual([entry("a"), entry("b")]);
    const next: Change = { path: PATH, from: 120, ops: [{ op: "set", path: "/0/role", value: "user" }] };
    set.receive(stream, event([next], 130));
    expect((set.get(PATH)!.value as { role: string }[])[0].role).toBe("user");
  });

  it("ignores a repeat and takes any other mismatch for a gap", () => {
    const { set, stream } = open();
    set.receive(stream, event([snapshot(PATH, [entry("a")], 5)], 100, "I"));
    const ops: Change = { path: PATH, from: 5, ops: [{ op: "append", path: "", values: [entry("b")] }] };
    set.receive(stream, event([ops], 120));
    expect(set.receive(stream, event([ops], 120))).toEqual({ kind: "ignored", changed: [] });
    const gap: Change = { path: PATH, from: 7, ops: [{ op: "append", path: "", values: [entry("c")] }] };
    expect(set.receive(stream, event([gap], 130))).toEqual({ kind: "resubscribe", changed: [] });
    expect(set.lastResubscribe).toMatchObject({ full: false });
    expect(set.get(PATH)).toMatchObject({ version: 120 });
  });

  it("treats an op that does not apply as a gap and leaves the replica as it was", () => {
    const { set, stream } = open();
    set.receive(stream, event([snapshot(PATH, [entry("a")], 5)], 100, "I"));
    const bad: Change = { path: PATH, from: 5, ops: [{ op: "set", path: "/9/role", value: "x" }] };
    expect(set.receive(stream, event([bad], 110)).kind).toBe("resubscribe");
    expect(set.get(PATH)).toMatchObject({ version: 5 });
  });

  it("applies the changes of one event together or not at all", () => {
    const { set, stream } = open([PATH, "/global"]);
    set.receive(stream, event([snapshot(PATH, [entry("a")], 5), snapshot("/global", { agent: 1 }, 3)].slice(0, 1), 100, "I"));
    const good: Change = { path: PATH, from: 5, ops: [{ op: "append", path: "", values: [entry("b")] }] };
    const bad: Change = { path: "/global", from: 99, ops: [] };
    expect(set.receive(stream, event([good, bad], 110)).kind).toBe("resubscribe");
    expect(set.get(PATH)).toMatchObject({ version: 5 });
  });

  it("installs a differential snapshot onto what it holds, keeping what the structure names", () => {
    const { set, stream } = open();
    const before = [entry("a"), entry("b"), entry("c")];
    set.receive(stream, event([snapshot(PATH, before, 5)], 100, "I"));
    const after = [entry("a"), entry("B"), entry("c"), entry("d")];
    const node = { l: 4, c: { "1": { v: entry("B") }, "3": { v: entry("d") } } };
    const result = set.receive(stream, event([snapshot(PATH, after, 9, 5, node)], 200));
    expect(result.kind).toBe("applied");
    expect(set.get(PATH)).toMatchObject({ value: after, version: 9 });
    // The unchanged items are the very objects held.
    expect((set.get(PATH)!.value as unknown[])[0]).toBe(before[0]);
  });

  it("installs an object with its own fields, its children's names and the changed ones", () => {
    const path = "/sessions/s";
    const session = (title: string, entries: unknown[]) => ({
      sessionId: "s", title, controls: { modes: null }, entries, prompts: [], turns: [],
    });
    const { set, stream } = open([path]);
    set.receive(stream, event([snapshot(path, session("t", [entry("a")]), 4)], 50, "I"));
    const next = session("u", [entry("a")]);
    const node = { o: { sessionId: "s", title: "u" }, k: ["controls", "entries", "prompts", "turns"], c: {} };
    expect(set.receive(stream, event([snapshot(path, next, 8, 4, node)], 60)).kind).toBe("applied");
    expect(set.get(path)!.value).toEqual(next);
  });

  it("drops a child the structure no longer names", () => {
    const path = "/sessions/s";
    const withEntries = { sessionId: "s", controls: null, entries: [], prompts: [], turns: [] };
    const without = { sessionId: "s", controls: null, prompts: [] };
    const { set, stream } = open([path]);
    set.receive(stream, event([snapshot(path, withEntries, 4)], 50, "I"));
    const node = { o: { sessionId: "s" }, k: ["controls", "prompts"], c: {} };
    expect(set.receive(stream, event([snapshot(path, without, 8, 4, node)], 60)).kind).toBe("applied");
    expect(set.get(path)!.value).toEqual(without);
  });

  it("keeps what it holds for a snapshot that says nothing changed, and refuses one that builds on another version", () => {
    const { set, stream } = open();
    const value = [entry("a")];
    set.receive(stream, event([snapshot(PATH, value, 5)], 100, "I"));
    const held = set.get(PATH)!.value;
    // Nothing to install, but the path is aligned with the stream again: its listeners are told.
    expect(set.receive(stream, event([snapshot(PATH, value, 5, 5, { same: true })], 101))).toEqual({
      kind: "applied",
      changed: [PATH],
    });
    expect(set.get(PATH)!.value).toBe(held);
    const stale = snapshot(PATH, [entry("a"), entry("x")], 9, 4, { l: 2, c: { "1": { v: entry("x") } } });
    expect(set.receive(stream, event([stale], 102)).kind).toBe("resubscribe");
    expect(set.lastResubscribe).toMatchObject({ full: false });
  });

  it("checks the hash at the version it holds, ignores an older check and takes a later one for a missed change", () => {
    const { set, stream } = open();
    const value = [entry("a")];
    set.receive(stream, event([snapshot(PATH, value, 5)], 100, "I"));
    const ops: Change = { path: PATH, from: 5, ops: [{ op: "append", path: "", values: [entry("b")] }] };
    set.receive(stream, event([ops], 120));
    const both = [entry("a"), entry("b")];
    expect(set.receive(stream, { rev: 121, check: [{ path: PATH, at: 120, hash: H(PATH, both) }] }).kind).toBe("ignored");
    expect(set.get(PATH)).toMatchObject({ version: 120, verified: 120 });
    expect(set.receive(stream, { rev: 121, check: [{ path: PATH, at: 5, hash: "old" }] }).kind).toBe("ignored");
    const missed = set.receive(stream, { rev: 140, check: [{ path: PATH, at: 130, hash: "x" }] });
    expect(missed.kind).toBe("resubscribe");
    expect(set.lastResubscribe).toMatchObject({ full: false });
  });

  it("drops a path whose hash disagrees with the checkpoint and asks for the whole subtree", () => {
    const { set, stream } = open();
    set.receive(stream, event([snapshot(PATH, [entry("a")], 5)], 100, "I"));
    expect(set.receive(stream, { rev: 101, check: [{ path: PATH, at: 5, hash: H(PATH, [entry("other")]) }] }).kind).toBe("resubscribe");
    expect(set.lastResubscribe).toMatchObject({ full: true, path: PATH });
    expect(set.get(PATH)).toBeUndefined();
    // The next stream asks for it without a version.
    const { url } = set.open([{ path: PATH }]);
    expect(new URL(url, "http://x").searchParams.getAll("have")).toEqual([]);
  });

  it("drops everything it holds of another instance", () => {
    const { set, stream } = open([PATH, "/global"]);
    set.receive(stream, event([snapshot(PATH, [entry("a")], 5), snapshot("/global", { n: 1 }, 2)], 100, "OLD"));
    const second = set.open([{ path: PATH }, { path: "/global" }]);
    expect(new URL(second.url, "http://x").searchParams.get("instance")).toBe("OLD");
    set.receive(second.stream, event([snapshot(PATH, [entry("z")], 1), snapshot("/global", { n: 2 }, 1)], 3, "NEW"));
    expect(set.instance).toBe("NEW");
    expect(set.get(PATH)).toMatchObject({ value: [entry("z")], version: 1 });
  });

  it("adopts a new instance even when its first event cannot be applied", () => {
    const { set, stream } = open();
    set.receive(stream, event([snapshot(PATH, [entry("a")], 5)], 100, "OLD"));
    const second = set.open([{ path: PATH }]);
    const broken: Change = { path: PATH, from: 5, ops: [] };
    expect(set.receive(second.stream, event([broken], 7, "NEW")).kind).toBe("resubscribe");
    expect(set.instance).toBe("NEW");
    expect(set.get(PATH)).toBeUndefined();
  });

  it("ends a path: it is dropped, and why is kept until it is followed again", () => {
    const { set, stream } = open([PATH]);
    set.receive(stream, event([snapshot(PATH, [entry("a")], 5)], 100, "I"));
    const end: Change = { path: PATH, end: { reason: "deleted" } };
    expect(set.receive(stream, event([end], 101))).toEqual({ kind: "applied", changed: [PATH] });
    expect(set.get(PATH)).toBeUndefined();
    expect(set.endOf(PATH)).toEqual({ reason: "deleted" });
    // The session comes back: the subscription stayed.
    set.receive(stream, event([snapshot(PATH, [], 9)], 120));
    expect(set.get(PATH)).toMatchObject({ version: 9 });
    expect(set.endOf(PATH)).toBeUndefined();
  });

  it("lets nothing of a replaced stream or of a path not followed through", () => {
    const first = open([PATH]);
    const second = first.set.open([{ path: PATH }]);
    expect(first.set.receive(first.stream, event([snapshot(PATH, [entry("a")], 5)], 100, "I"))).toEqual({ kind: "ignored", changed: [] });
    expect(first.set.receive(second.stream, event([snapshot("/global", { n: 1 }, 5)], 100, "I"))).toEqual({ kind: "ignored", changed: [] });
    expect(first.set.get(PATH)).toBeUndefined();
  });

  it("asks for what it follows, from what it holds, and for a session's load with its directory", () => {
    const set = new TreeReplicaSet();
    const one = set.open([{ path: "/global" }, { path: "/sessions/a b/op", cwd: "/work/x:y" }]);
    const first = new URL(one.url, "http://x");
    expect(first.pathname).toBe("/api/v2/stream");
    expect(first.searchParams.getAll("follow")).toEqual(["/global", "/sessions/a b/op"]);
    expect(first.searchParams.get("instance")).toBeNull();
    expect(first.searchParams.getAll("cwd")).toEqual(["a%20b:/work/x:y"]);

    set.receive(one.stream, event([snapshot("/global", { n: 1 }, 7)], 9, "I"));
    const second = new URL(set.open([{ path: "/global" }]).url, "http://x");
    expect(second.searchParams.get("instance")).toBe("I");
    expect(second.searchParams.getAll("have")).toEqual(["/global:7"]);
  });

  it("keeps a few replicas of paths it no longer follows", () => {
    const set = new TreeReplicaSet();
    let stream = set.open([{ path: "/global" }]).stream;
    set.receive(stream, event([snapshot("/global", { n: 1 }, 1)], 2, "I"));
    for (let n = 0; n < 6; n += 1) {
      stream = set.open([{ path: `/sessions/s${n}/op` }]).stream;
      set.receive(stream, event([snapshot(`/sessions/s${n}/op`, { id: "x" }, n + 1)], n + 3));
    }
    expect(set.get("/global")).toBeUndefined();
    expect(set.get("/sessions/s5/op")).toBeDefined();
  });
});
