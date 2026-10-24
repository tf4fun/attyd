// The documents of the scopes as the page reads them, assembled from the tree (design §4.2).
import { describe, expect, it } from "vitest";
import { wire } from "../shared/digest";
import { ScopeFeed, operationOf, pathOf, scopeOf } from "../shared/scope-feed";
import { TreeFeedClient, type StreamSource } from "../shared/tree-feed";
import { hashAt, tokensOf, type Change, type TreeEvent } from "../shared/tree-replica";

class FakeSource implements StreamSource {
  onmessage: ((message: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  constructor(readonly url: string) {}
  close() {}
  emit(event: TreeEvent) {
    this.onmessage?.({ data: JSON.stringify(event) });
  }
}

function harness() {
  const sources: FakeSource[] = [];
  const feed = new TreeFeedClient((url) => {
    const source = new FakeSource(url);
    sources.push(source);
    return source;
  }, () => undefined);
  return { scopes: new ScopeFeed(feed), sources, last: () => sources[sources.length - 1] };
}

const head = (id: string, extra: object = {}) => ({
  id, scope: "global", after: null, kind: "none", status: "completed", cancelRequested: false, ...extra,
});

function snapshot(path: string, value: unknown, at: number): Change {
  return { path, snapshot: { base: null, at, hash: wire(hashAt(tokensOf(path), value)), node: { v: value } } };
}

const global = (extra: object = {}) => ({
  agent: { phase: "ready" }, op: head("g1"), opDetail: { input: null }, ops: [], sessions: {}, loadFailures: {},
  terminals: {}, interactions: {}, urlFlows: {}, ...extra,
});

const session = (extra: object = {}) => ({
  sessionId: "s", cwd: "/w", closing: false, history: "complete", historyRev: 2, heldSince: 2, controls: { modes: null },
  op: head("o1", { scope: "session/s", kind: "prompt", status: "failed", error: { code: "agent_error", agentCode: -32000 } }),
  opDetail: { input: [{ type: "text", text: "hi" }], error: { code: "agent_error", message: "login", data: { code: -32000 } } },
  ops: [], interactions: {}, urlFlows: {}, terminals: {}, entries: [], prompts: [], turns: [], ...extra,
});

describe("the documents of the scopes", () => {
  it("maps scopes to the paths of the tree and back", () => {
    expect(pathOf("global")).toBe("/global");
    expect(pathOf("catalog")).toBe("/catalog");
    expect(pathOf("session/a/b")).toBe("/sessions/a~1b");
    expect(scopeOf("/sessions/a~1b")).toBe("session/a/b");
    expect(scopeOf("/global")).toBe("global");
    expect(scopeOf("/sessions/s/entries")).toBeUndefined();
    expect(() => pathOf("nowhere")).toThrow();
  });

  it("puts an operation's head and detail together, and a failed one's whole error back", () => {
    const op = operationOf(head("o", { status: "failed", error: { code: "unavailable" } }), {
      input: { a: 1 },
      error: { code: "unavailable", message: "no connection" },
    });
    expect(op).toMatchObject({ id: "o", status: "failed", input: { a: 1 }, error: { code: "unavailable", message: "no connection" } });
    const bare = operationOf(head("o", { status: "failed", error: { code: "agent_error", agentCode: -32000 } }), undefined);
    expect(bare).toMatchObject({ input: null, error: { code: "agent_error", data: { code: -32000 } } });
    expect(operationOf(undefined, {})).toBeUndefined();
  });

  it("reads global, catalog and a session as the documents the page used to", () => {
    const { scopes, last } = harness();
    scopes.follow([{ scope: "global" }, { scope: "session/s" }]);
    expect(new URL(last().url, "http://x").searchParams.getAll("follow")).toEqual(["/global", "/sessions/s"]);
    last().emit({ instance: "I", rev: 9, changes: [snapshot("/global", global(), 3), snapshot("/sessions/s", session(), 4)] });

    const g = scopes.get("global")!;
    expect(g).toMatchObject({ scope: "global", epoch: "I", rev: 3, verifiedRev: 3 });
    expect(g.doc.live).toMatchObject({ agent: { phase: "ready" }, op: { id: "g1", input: null }, sessions: {} });
    expect(g.doc.live).not.toHaveProperty("opDetail");

    const s = scopes.get("session/s")!;
    expect(s.doc).toMatchObject({ history: "complete", entries: [], prompts: [], turns: [], historyRev: 2 });
    expect(s.doc.live).toMatchObject({
      sessionId: "s", cwd: "/w", closing: false, controls: { modes: null },
      op: { id: "o1", kind: "prompt", input: [{ type: "text", text: "hi" }], error: { message: "login" } },
    });
    expect(s.doc.live).not.toHaveProperty("entries");
  });

  it("is the same document while the tree under it did not change", () => {
    const { scopes, last } = harness();
    scopes.follow([{ scope: "global" }, { scope: "session/s" }]);
    last().emit({ instance: "I", rev: 9, changes: [snapshot("/global", global(), 3), snapshot("/sessions/s", session(), 4)] });
    const before = scopes.get("session/s")!.doc;
    last().emit({ rev: 10, changes: [{ path: "/global", from: 3, ops: [{ op: "set", path: "/agent", value: { phase: "x" } }] }] });
    expect(scopes.get("session/s")!.doc).toBe(before);
    expect(scopes.get("global")!.doc.live).toMatchObject({ agent: { phase: "x" } });
  });

  it("tells its listeners which scope changed, and nothing of a session that is not there yet", () => {
    const { scopes, last } = harness();
    const changed: string[] = [];
    scopes.subscribe((scope) => changed.push(scope));
    scopes.follow([{ scope: "global" }, { scope: "session/s", cwd: "/w" }]);
    expect(new URL(last().url, "http://x").searchParams.getAll("cwd")).toEqual(["s:/w"]);
    last().emit({
      instance: "I", rev: 9,
      changes: [snapshot("/global", global(), 3), { path: "/sessions/s", end: { reason: "absent" } }],
    });
    expect(changed).toEqual(["global"]);
    expect(scopes.endOf("session/s")).toBeUndefined();
    last().emit({ rev: 10, changes: [snapshot("/sessions/s", session(), 10)] });
    expect(changed).toEqual(["global", "session/s"]);
  });

  it("says why a session ended, with the whole error of a failed load from global", () => {
    const { scopes, last } = harness();
    const changed: string[] = [];
    scopes.subscribe((scope) => changed.push(scope));
    scopes.follow([{ scope: "global" }, { scope: "session/s" }]);
    const failure = { code: "agent_error", message: "authentication required", data: { code: -32000, message: "authentication required" } };
    last().emit({
      instance: "I", rev: 9,
      changes: [
        snapshot("/global", global({ loadFailures: { s: failure } }), 3),
        { path: "/sessions/s", end: { reason: "load_failed", error: { code: "agent_error", agentCode: -32000 } } },
      ],
    });
    expect(scopes.get("session/s")).toBeUndefined();
    expect(scopes.endOf("session/s")).toEqual({ reason: "load_failed", error: failure });
    expect(changed).toContain("session/s");
    last().emit({ rev: 10, changes: [{ path: "/sessions/s", end: { reason: "deleted" } }] });
    expect(scopes.endOf("session/s")).toMatchObject({ reason: "deleted" });
  });
});
