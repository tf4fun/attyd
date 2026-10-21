// The replica engine of the client protocol (docs/protocol.md §5; acceptance: rev
// gaps, inapplicable ops, wrong content with valid framing, a corrupted replica,
// differential snapshots, a new epoch). The scenario, the documents after each
// event and their digests come from tests/fixtures/protocol_vectors.py, an
// implementation that shares no code with the replica.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  DocumentReplicaSet,
  applyEvent,
  type Applied,
  type DocumentReplica,
  type StreamEvent,
} from "../shared/document-replica";

interface Step {
  name: string;
  event: StreamEvent;
  doc: Record<string, unknown> & { epoch: string; rev: number };
  digest: string;
}

const scenarios = (
  JSON.parse(readFileSync(new URL("./fixtures/protocol-replica-scenario.json", import.meta.url), "utf8")) as {
    scenarios: { session: Step[]; global: Step[] };
  }
).scenarios;

/** A fresh copy: the replica freezes what it installs, and a test reuses events. */
function copy<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

function applied(result: Applied): DocumentReplica {
  expect(result).toMatchObject({ kind: "applied" });
  return (result as Extract<Applied, { kind: "applied" }>).replica;
}

function body(replica: DocumentReplica) {
  return { epoch: replica.epoch, rev: replica.rev, ...replica.doc };
}

/** The replica after the first `count` steps of a scenario. */
function after(steps: Step[], count: number): DocumentReplica | undefined {
  let replica: DocumentReplica | undefined;
  for (const step of steps.slice(0, count)) replica = applied(applyEvent(replica, copy(step.event)));
  return replica;
}

const session = scenarios.session;
const checkpointAt = (step: Step): StreamEvent => ({
  scope: "session/S",
  rev: step.doc.rev,
  digest: step.digest,
});

describe("replaying the independent scenario", () => {
  it.each([
    ["session", session],
    ["global", scenarios.global],
  ] as const)("a %s scope matches the expected document after every event", (_, steps) => {
    let replica: DocumentReplica | undefined;
    let verified = -1;
    for (const step of steps) {
      replica = applied(applyEvent(replica, copy(step.event)));
      expect(body(replica), step.name).toEqual(step.doc);
      const event = step.event;
      if ("snapshot" in event || "digest" in event) verified = replica.rev;
      expect(replica.verifiedRev, step.name).toBe(verified);
    }
  });

  it("holds a document that cannot be changed under the replica's feet", () => {
    const replica = after(session, 2)!;
    expect(Object.isFrozen(replica.doc)).toBe(true);
    expect(Object.isFrozen((replica.doc.entries as unknown[])[0])).toBe(true);
    expect(() => {
      (replica.doc.entries as unknown[]).push({});
    }).toThrow(TypeError);
  });

  it("leaves the replica it was given as it was when an event is applied", () => {
    const before = after(session, 1)!;
    const snapshot = JSON.stringify(body(before));
    applied(applyEvent(before, copy(session[1].event)));
    expect(JSON.stringify(body(before))).toBe(snapshot);
  });

  it("ends a scope", () => {
    expect(applyEvent(after(session, 1), { scope: "session/S", end: { reason: "closed" } })).toEqual({
      kind: "ended",
      end: { reason: "closed" },
    });
  });
});

describe("differential snapshots", () => {
  it("keep the entries not listed and share them with the replica that held them", () => {
    const before = after(session, 5)!;
    const next = applied(applyEvent(before, copy(session[5].event)));
    expect(body(next)).toEqual(session[5].doc);
    expect((next.doc.entries as unknown[])[0]).toBe((before.doc.entries as unknown[])[0]);
  });

  it("are refused for another epoch, for entries not held and for shrunk entries", () => {
    const update = copy(session[5].event) as Extract<StreamEvent, { snapshot: unknown }>;
    // Another epoch.
    const otherEpoch = copy(update);
    otherEpoch.snapshot = { ...otherEpoch.snapshot, epoch: "E-other" };
    expect(applyEvent(after(session, 5), otherEpoch)).toMatchObject({ kind: "resubscribe", full: true });
    // Nothing held.
    expect(applyEvent(undefined, copy(update))).toMatchObject({ kind: "resubscribe", full: true });
    // Fewer entries than held.
    const shrunk = copy(update);
    shrunk.snapshot = { ...shrunk.snapshot, entries: { base: "update", length: 1, changed: [] } };
    expect(applyEvent(after(session, 5), shrunk)).toMatchObject({ kind: "resubscribe", full: true });
  });

  it("are refused when an entry is neither held nor listed", () => {
    const update = copy(session[5].event) as Extract<StreamEvent, { snapshot: unknown }>;
    update.snapshot = { ...update.snapshot, entries: { base: "update", length: 3, changed: [[1, {}]] } };
    expect(applyEvent(after(session, 5), update)).toMatchObject({ kind: "resubscribe", full: true });
  });
});

describe("a snapshot that cannot be confirmed", () => {
  it("is not installed when its digest does not match what it carries", () => {
    const event = copy(session[0].event) as Extract<StreamEvent, { snapshot: unknown }>;
    const entries = event.snapshot.entries!;
    // Valid framing and the supplied digest, with one entry changed.
    const tampered = { ...entries, changed: [[0, { kind: "message", role: "agent", chunks: [] }]] as const };
    event.snapshot = { ...event.snapshot, entries: tampered };
    expect(applyEvent(undefined, event)).toEqual({
      kind: "resubscribe",
      reason: expect.stringContaining("digest"),
      full: true,
    });
  });

  it("is refused when it lists an index twice, outside the entries, or leaves a hole", () => {
    for (const changed of [
      [[0, {}], [0, {}]],
      [[5, {}]],
      [],
    ]) {
      const event = copy(session[0].event) as Extract<StreamEvent, { snapshot: unknown }>;
      event.snapshot = { ...event.snapshot, entries: { base: "replace", length: 1, changed } as never };
      expect(applyEvent(undefined, event), JSON.stringify(changed)).toMatchObject({ kind: "resubscribe", full: true });
    }
  });
});

describe("ops", () => {
  it("must be the next rev: a gap, a repeat and an old rev are discarded", () => {
    const replica = after(session, 2)!; // rev 11
    for (const rev of [13, 11, 5]) {
      const event = { ...copy(session[2].event), rev } as StreamEvent;
      expect(applyEvent(replica, event), `rev ${rev}`).toMatchObject({ kind: "resubscribe", full: false });
    }
  });

  it("that do not apply leave the replica unchanged", () => {
    const replica = after(session, 2)!;
    const event: StreamEvent = {
      scope: "session/S",
      rev: 12,
      ops: [{ op: "set", path: "/entries/9/role", value: "agent" }],
    };
    expect(applyEvent(replica, event)).toMatchObject({ kind: "resubscribe", full: false });
    expect(body(replica)).toEqual(session[1].doc);
  });

  it("need a snapshot before them", () => {
    expect(applyEvent(undefined, copy(session[1].event))).toMatchObject({ kind: "resubscribe", full: false });
    expect(applyEvent(undefined, checkpointAt(session[4]))).toMatchObject({ kind: "resubscribe", full: false });
  });
});

describe("checkpoints", () => {
  it("detect wrong content under continuous revs, and a full snapshot repairs it", () => {
    // The ops of rev 11 with a different value: they apply, the revs are continuous.
    const tamperedOps = copy(session[1].event) as Extract<StreamEvent, { ops: unknown }>;
    const op = tamperedOps.ops[0] as { values: Array<{ chunks: Array<{ content: { text: string } }> }> };
    op.values[0].chunks[0].content.text = "tampered";
    const tampered = applied(applyEvent(after(session, 1), tamperedOps));
    expect(tampered.rev).toBe(11);

    const result = applyEvent(tampered, checkpointAt(session[1]));
    expect(result).toMatchObject({ kind: "resubscribe", full: true });
  });

  it("detect a replica changed without an event, also after its hashes were cached", () => {
    const replica = after(session, 5)!; // verified at rev 13: every entry hash is cached
    expect(applied(applyEvent(replica, checkpointAt(session[4]))).verifiedRev).toBe(13);
    const entries = replica.doc.entries as unknown[];
    const corrupted: DocumentReplica = {
      ...replica,
      doc: { ...replica.doc, entries: [{ ...(entries[0] as object), role: "agent" }, ...entries.slice(1)] },
    };
    expect(applyEvent(corrupted, checkpointAt(session[4]))).toMatchObject({ kind: "resubscribe", full: true });
  });

  it("ahead of the replica mean events were missed", () => {
    expect(applyEvent(after(session, 2), checkpointAt(session[3]))).toMatchObject({
      kind: "resubscribe",
      full: false,
    });
  });
});

describe("DocumentReplicaSet", () => {
  const steps = (count: number) => session.slice(0, count);

  function follow(set: DocumentReplicaSet, upTo: number): number {
    const { stream } = set.open([{ scope: "session/S" }]);
    for (const step of steps(upTo)) expect(set.receive(stream, copy(step.event))).toBe("applied");
    return stream;
  }

  it("asks for the version it holds and for nothing it holds nothing of", () => {
    const set = new DocumentReplicaSet();
    expect(set.open([{ scope: "global" }, { scope: "session/S", cwd: "/w" }]).url).toBe(
      "/api/v2/stream?scope=global&scope=session%2FS&cwd=session%2FS%3A%2Fw",
    );
    const stream = set.open([{ scope: "session/S" }]).stream;
    set.receive(stream, copy(session[0].event));
    set.receive(stream, copy(session[1].event));
    expect(new URL(`http://x${set.open([{ scope: "session/S" }]).url}`).searchParams.get("have")).toBe("session/S:E1:11");
  });

  it("ignores the events of a replaced stream", () => {
    const set = new DocumentReplicaSet();
    const first = follow(set, 2);
    set.open([{ scope: "session/S" }]);
    expect(set.receive(first, copy(session[2].event))).toBe("ignored");
    expect(set.get("session/S")!.rev).toBe(11);
  });

  it("keeps the version after a gap but drops a replica whose content disagrees", () => {
    const set = new DocumentReplicaSet();
    let stream = follow(set, 2);
    expect(set.receive(stream, { ...copy(session[3].event), rev: 20 } as StreamEvent)).toBe("resubscribe");
    expect(set.lastResubscribe).toMatchObject({ full: false });
    expect(set.get("session/S")!.rev).toBe(11);

    stream = set.open([{ scope: "session/S" }]).stream;
    expect(set.receive(stream, { scope: "session/S", rev: 11, digest: "x".repeat(22) })).toBe("resubscribe");
    expect(set.lastResubscribe).toMatchObject({ full: true });
    expect(set.get("session/S")).toBeUndefined();
    expect(new URL(`http://x${set.open([{ scope: "session/S" }]).url}`).searchParams.has("have")).toBe(false);
  });

  it("counts failed verifications of a scope until a snapshot installs", () => {
    const set = new DocumentReplicaSet();
    const bad = copy(session[0].event) as Extract<StreamEvent, { snapshot: unknown }>;
    bad.snapshot = { ...bad.snapshot, digest: "y".repeat(22) };
    for (const expected of [1, 2]) {
      const { stream } = set.open([{ scope: "session/S" }]);
      expect(set.receive(stream, copy(bad))).toBe("resubscribe");
      expect(set.consecutiveFailures("session/S")).toBe(expected);
    }
    const { stream } = set.open([{ scope: "session/S" }]);
    expect(set.receive(stream, copy(session[0].event))).toBe("applied");
    expect(set.consecutiveFailures("session/S")).toBe(0);
  });

  it("forgets a scope that ended", () => {
    const set = new DocumentReplicaSet();
    const stream = follow(set, 1);
    expect(set.receive(stream, { scope: "session/S", end: { reason: "deleted" } })).toBe("ended");
    expect(set.get("session/S")).toBeUndefined();
    expect(set.receive(stream, copy(session[1].event))).toBe("ignored");
  });
});
