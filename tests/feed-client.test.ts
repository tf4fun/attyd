// The stream client of revision 3 (docs/protocol.md §5.1, §5.4). The transport is
// faked so each case fixes the order of stream events and reconnects itself; the
// events and their digests come from the independent scenario of
// tests/fixtures/protocol_vectors.py.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { FeedClient, type FeedStatus, type StreamSource } from "../shared/feed-client";
import type { StreamEvent } from "../shared/document-replica";

interface Step {
  event: StreamEvent;
  doc: { rev: number };
}

const session = (
  JSON.parse(readFileSync(new URL("./fixtures/protocol-replica-scenario.json", import.meta.url), "utf8")) as {
    scenarios: { session: Step[] };
  }
).scenarios.session;

const S = "session/S";

class FakeSource implements StreamSource {
  onmessage: ((message: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;
  constructor(readonly url: string) {}
  close() {
    this.closed = true;
  }
  emit(event: StreamEvent) {
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
  const client = new FeedClient(
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

/** A fresh copy of a step's event: the replica freezes what it installs. */
function event(index: number): StreamEvent {
  return JSON.parse(JSON.stringify(session[index].event)) as StreamEvent;
}

function params(url: string) {
  return new URL(url, "http://bridge.test").searchParams;
}

describe("following scopes", () => {
  it("opens the revision 3 stream for the scopes and keeps what it delivers", () => {
    const { client, last } = harness();
    const changed: string[] = [];
    client.subscribe((scope) => changed.push(scope));
    client.follow([{ scope: S }]);
    expect(new URL(last().url, "http://bridge.test").pathname).toBe("/api/v3/stream");
    expect(params(last().url).getAll("scope")).toEqual([S]);
    expect(params(last().url).has("have")).toBe(false);

    last().emit(event(0));
    last().emit(event(1));
    expect(changed).toEqual([S, S]);
    expect(client.get(S)?.rev).toBe(session[1].doc.rev);
  });

  it("closes the stream it replaces and lets nothing of it through", () => {
    const { client, sources } = harness();
    client.follow([{ scope: S }]);
    const first = sources[0];
    client.follow([{ scope: S }, { scope: "global" }]);
    expect(first.closed).toBe(true);
    expect(sources).toHaveLength(2);
    first.emit(event(0));
    expect(client.get(S)).toBeUndefined();
  });

  it("forgets a scope that ended and tells its listeners", () => {
    const { client, last } = harness();
    const changed: string[] = [];
    client.subscribe((scope) => changed.push(scope));
    client.follow([{ scope: S }]);
    last().emit(event(0));
    last().emit({ scope: S, end: { reason: "closed" } });
    expect(client.get(S)).toBeUndefined();
    expect(changed).toEqual([S, S]);
  });

  it("opens nothing after it is closed", () => {
    const { client, sources, scheduled } = harness();
    client.follow([{ scope: S }]);
    client.close();
    expect(sources[0].closed).toBe(true);
    client.follow([{ scope: S }]);
    expect(sources).toHaveLength(1);
    expect(scheduled).toEqual([]);
  });
});

describe("a stream that fails", () => {
  it("subscribes again later with the version it holds", () => {
    const h = harness();
    h.client.follow([{ scope: S }]);
    h.last().emit(event(0));
    h.last().emit(event(1));
    h.last().fail();
    expect(h.sources[0].closed).toBe(true);
    expect(h.scheduled).toHaveLength(1);
    h.runScheduled();
    const have = params(h.last().url).getAll("have");
    expect(have).toEqual([`${S}:${(h.client.get(S) as { epoch: string }).epoch}:${session[1].doc.rev}`]);
  });

  it("closes the stream on an event that does not apply and keeps the replica it trusts", () => {
    const h = harness();
    h.client.follow([{ scope: S }]);
    h.last().emit(event(0));
    const gap = { ...(event(2) as object), rev: 99 } as StreamEvent;
    h.last().emit(gap);
    expect(h.sources[0].closed).toBe(true);
    expect(h.client.get(S)?.rev).toBe(session[0].doc.rev);
    h.runScheduled();
    expect(params(h.last().url).getAll("have")).toHaveLength(1);
  });

  it("treats data that is not an event as a broken stream", () => {
    const h = harness();
    h.client.follow([{ scope: S }]);
    h.last().emitRaw("not json");
    expect(h.sources[0].closed).toBe(true);
    expect(h.scheduled).toHaveLength(1);
  });

  it("drops a replica that disagrees with the bridge and asks for the whole document", () => {
    const h = harness();
    h.client.follow([{ scope: S }]);
    h.last().emit(event(0));
    h.last().emit({ scope: S, rev: session[0].doc.rev, digest: "x".repeat(22) });
    expect(h.client.get(S)).toBeUndefined();
    h.runScheduled();
    expect(params(h.last().url).has("have")).toBe(false);
  });

  it("retries with a growing delay and says so, until a stream delivers again", () => {
    const h = harness();
    h.client.follow([{ scope: S }]);
    for (const delay of [1000, 2000, 4000, 8000]) {
      h.last().fail();
      expect(h.scheduled[h.scheduled.length - 1].delay).toBe(delay);
      h.runScheduled();
    }
    const last = h.statuses[h.statuses.length - 1];
    expect(last).toMatchObject({ state: "retrying", attempt: 4 });

    h.last().emit(event(0));
    expect(h.statuses[h.statuses.length - 1]).toEqual({ state: "synchronized" });
    h.last().fail();
    expect(h.scheduled[h.scheduled.length - 1].delay).toBe(1000);
  });

  it("never waits longer than thirty seconds", () => {
    const h = harness();
    h.client.follow([{ scope: S }]);
    for (let i = 0; i < 12; i += 1) {
      h.last().fail();
      h.runScheduled();
    }
    h.last().fail();
    expect(h.scheduled[h.scheduled.length - 1].delay).toBe(30_000);
  });

  it("drops a reconnect queued by a stream once a new set is followed", () => {
    const h = harness();
    h.client.follow([{ scope: S }]);
    h.last().fail();
    h.client.follow([{ scope: "global" }]);
    const opened = h.sources.length;
    h.runScheduled();
    expect(h.sources).toHaveLength(opened);
  });
});
