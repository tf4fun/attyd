// The stream client of revision 4 (design §6.1, §6.5). The transport is faked so each case fixes
// the order of stream events and reconnects itself.
import { describe, expect, it } from "vitest";
import { wire } from "../shared/digest";
import { TreeFeedClient, type FeedStatus, type StreamSource } from "../shared/tree-feed";
import { hashAt, tokensOf, type Change, type TreeEvent } from "../shared/tree-replica";

const S = "/sessions/S/op";
const T = "/sessions/T/op";

class FakeSource implements StreamSource {
  onmessage: ((message: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;
  constructor(readonly url: string) {}
  close() {
    this.closed = true;
  }
  emit(event: TreeEvent) {
    this.onmessage?.({ data: JSON.stringify(event) });
  }
  emitRaw(data: string) {
    this.onmessage?.({ data });
  }
  fail() {
    this.onerror?.();
  }
}

function harness() {
  const sources: FakeSource[] = [];
  const scheduled: Array<{ run: () => void; delay: number }> = [];
  const statuses: FeedStatus[] = [];
  const client = new TreeFeedClient(
    (url) => {
      const source = new FakeSource(url);
      sources.push(source);
      return source;
    },
    (run, delay) => void scheduled.push({ run, delay }),
  );
  client.onStatus((status) => statuses.push(status));
  return {
    client,
    sources,
    scheduled,
    statuses,
    last: () => sources[sources.length - 1],
    runScheduled() {
      scheduled.splice(0).forEach(({ run }) => run());
    },
  };
}

const op = (id: string) => ({ id, scope: "x", after: null, kind: "none", status: "completed", cancelRequested: false });

function snapshot(path: string, value: unknown, at: number): Change {
  return { path, snapshot: { base: null, at, hash: wire(hashAt(tokensOf(path), value)), node: { v: value } } };
}

function first(path = S, id = "o1", at = 3, rev = 10): TreeEvent {
  return { instance: "I", rev, changes: [snapshot(path, op(id), at)] };
}

function params(url: string) {
  return new URL(url, "http://bridge.test").searchParams;
}

describe("following paths", () => {
  it("opens the revision 4 stream for the paths and keeps what it delivers", () => {
    const { client, last } = harness();
    const changed: string[] = [];
    client.subscribe((path) => changed.push(path));
    client.follow([{ path: S }]);
    expect(new URL(last().url, "http://bridge.test").pathname).toBe("/api/v2/stream");
    expect(params(last().url).getAll("follow")).toEqual([S]);
    expect(params(last().url).has("have")).toBe(false);

    last().emit(first());
    expect(changed).toEqual([S]);
    expect(client.get(S)).toMatchObject({ value: op("o1"), version: 3 });
    expect(client.instance).toBe("I");
  });

  it("closes the stream it replaces and lets nothing of it through", () => {
    const { client, sources } = harness();
    client.follow([{ path: S }]);
    const earlier = sources[0];
    client.follow([{ path: S }, { path: "/global" }]);
    expect(earlier.closed).toBe(true);
    expect(sources).toHaveLength(2);
    earlier.emit(first());
    expect(client.get(S)).toBeUndefined();
  });

  it("forgets a path that ended, tells its listeners and remembers why until it is followed again", () => {
    const { client, last } = harness();
    const changed: string[] = [];
    client.subscribe((path) => changed.push(path));
    client.follow([{ path: S }]);
    last().emit(first());
    expect(client.endOf(S)).toBeUndefined();
    last().emit({ rev: 11, changes: [{ path: S, end: { reason: "load_failed", error: { code: "agent_error", agentCode: -32000 } } }] });
    expect(client.get(S)).toBeUndefined();
    expect(client.endOf(S)).toEqual({ reason: "load_failed", error: { code: "agent_error", agentCode: -32000 } });
    expect(changed).toEqual([S, S]);
    client.follow([{ path: S }]);
    expect(client.endOf(S)).toBeUndefined();
  });

  it("opens nothing after it is closed", () => {
    const { client, sources, scheduled } = harness();
    client.follow([{ path: S }]);
    client.close();
    expect(sources[0].closed).toBe(true);
    client.follow([{ path: S }]);
    expect(sources).toHaveLength(1);
    expect(scheduled).toEqual([]);
  });
});

describe("a stream that fails", () => {
  it("subscribes again later with the instance and the version it holds", () => {
    const h = harness();
    h.client.follow([{ path: S }]);
    h.last().emit(first());
    h.last().fail();
    expect(h.sources[0].closed).toBe(true);
    expect(h.scheduled).toHaveLength(1);
    h.runScheduled();
    expect(params(h.last().url).get("instance")).toBe("I");
    expect(params(h.last().url).getAll("have")).toEqual([`${S}:3`]);
  });

  it("closes the stream on an event that does not apply and keeps the replica it trusts", () => {
    const h = harness();
    h.client.follow([{ path: S }]);
    h.last().emit(first());
    h.last().emit({ rev: 99, changes: [{ path: S, from: 7, ops: [] }] });
    expect(h.sources[0].closed).toBe(true);
    expect(h.client.get(S)?.version).toBe(3);
    h.runScheduled();
    expect(params(h.last().url).getAll("have")).toHaveLength(1);
  });

  it("treats data that is not an event as a broken stream", () => {
    const h = harness();
    h.client.follow([{ path: S }]);
    h.last().emitRaw("not json");
    expect(h.sources[0].closed).toBe(true);
    expect(h.scheduled).toHaveLength(1);
  });

  it("drops a replica that disagrees with the bridge and asks for the whole subtree", () => {
    const h = harness();
    h.client.follow([{ path: S }]);
    h.last().emit(first());
    h.last().emit({ rev: 11, check: [{ path: S, at: 3, hash: "x".repeat(22) }] });
    expect(h.client.get(S)).toBeUndefined();
    h.runScheduled();
    expect(params(h.last().url).has("have")).toBe(false);
  });

  it("retries with a growing delay and says so, until a stream delivers again", () => {
    const h = harness();
    h.client.follow([{ path: S }]);
    for (const delay of [1000, 2000, 4000, 8000]) {
      h.last().fail();
      expect(h.scheduled[h.scheduled.length - 1].delay).toBe(delay);
      h.runScheduled();
    }
    expect(h.statuses[h.statuses.length - 1]).toMatchObject({ state: "retrying", attempt: 4 });
    h.last().emit(first());
    expect(h.statuses[h.statuses.length - 1]).toEqual({ state: "synchronized" });
    h.last().fail();
    expect(h.scheduled[h.scheduled.length - 1].delay).toBe(1000);
  });

  // #177: a path whose snapshot never verifies is not retried every second because another path
  // on the same stream delivers.
  it("backs off a path whose content keeps failing, while another path delivers", () => {
    const h = harness();
    h.client.follow([{ path: S }, { path: T }]);
    const disagreeing = (): TreeEvent => ({
      instance: "I",
      rev: 10,
      changes: [
        snapshot(S, op("o1"), 3),
        { path: T, snapshot: { base: null, at: 3, hash: "x".repeat(22), node: { v: op("o2") } } },
      ],
    });
    for (const delay of [1000, 2000, 4000, 8000]) {
      h.last().emit(disagreeing());
      expect(h.sources[h.sources.length - 1].closed).toBe(true);
      expect(h.scheduled[h.scheduled.length - 1].delay).toBe(delay);
      h.runScheduled();
    }
  });

  it("never waits longer than thirty seconds", () => {
    const h = harness();
    h.client.follow([{ path: S }]);
    for (let i = 0; i < 12; i += 1) {
      h.last().fail();
      h.runScheduled();
    }
    h.last().fail();
    expect(h.scheduled[h.scheduled.length - 1].delay).toBe(30_000);
  });

  it("drops a reconnect queued by a stream once a new set is followed", () => {
    const h = harness();
    h.client.follow([{ path: S }]);
    h.last().fail();
    h.client.follow([{ path: "/global" }]);
    const opened = h.sources.length;
    h.runScheduled();
    expect(h.sources).toHaveLength(opened);
  });
});
