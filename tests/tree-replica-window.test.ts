// A replica that holds a text position as a window (docs/protocol.md §5.4): it asks for it, installs
// it verified, and applies the bounded ops the bridge sends for appends, without ever holding the text
// before the window. The windows come from `windowOf`, a port of the bridge's rule; the hashes are
// those of the whole text, which the window must reproduce.
import { describe, expect, it } from "vitest";
import { wire } from "../shared/digest";
import { type Change, TreeReplicaSet, type TreeEvent, hashAt, tokensOf } from "../shared/tree-replica";
import { SEGMENT_LENGTH } from "../shared/tree-hash";
import { windowOf } from "./fixtures/text-window";

const PATH = "/views/s/output/latest";
const N = 4096;
const H = (value: unknown): string => wire(hashAt(tokensOf(PATH), value));

const view = (text: string) => ({ turn: null, entry: 3, text, other: 0 });
const windowed = (text: string, bytes = N) => ({ ...view(text), text: windowOf(text, bytes) });

function snapshot(text: string, at: number, bytes = N): Change {
  return { path: PATH, snapshot: { base: null, at, hash: H(view(text)), node: { v: windowed(text, bytes) } } };
}

function event(changes: Change[], rev: number, instance?: string, check?: TreeEvent["check"]): TreeEvent {
  return { ...(instance === undefined ? {} : { instance }), rev, changes, ...(check === undefined ? {} : { check }) };
}

function opened(bytes = N) {
  const set = new TreeReplicaSet();
  const { stream, url } = set.open([{ path: PATH, window: bytes }]);
  return { set, stream, url };
}

describe("following a path as a window", () => {
  it("asks for the window of the path on the stream", () => {
    const { url } = opened(8192);
    const params = new URL(url, "http://x").searchParams;
    expect(params.getAll("follow")).toEqual([PATH]);
    expect(params.getAll("window")).toEqual([`${PATH}:8192`]);
  });

  it("asks for no window of a path followed whole", () => {
    const set = new TreeReplicaSet();
    const { url } = set.open([{ path: PATH }]);
    expect(new URL(url, "http://x").searchParams.has("window")).toBe(false);
  });

  it("installs a window that hashes as the text it stands for, and holds only the window", () => {
    const text = "w".repeat(100_000);
    const { set, stream } = opened();
    expect(set.receive(stream, event([snapshot(text, 5)], 100, "I"))).toEqual({ kind: "applied", changed: [PATH] });
    const held = (set.get(PATH)!.value as { text: { text: string; length: number; offset: number } }).text;
    expect(held.length).toBe(100_000);
    expect(held.text.length).toBeLessThan(N + SEGMENT_LENGTH);
    expect(held.offset + held.text.length).toBe(100_000);
  });

  it("refuses a window whose text is not the one the hash is of", () => {
    const text = "w".repeat(20_000);
    const { set, stream } = opened();
    const forged: Change = {
      path: PATH,
      snapshot: { base: null, at: 5, hash: H(view(text)), node: { v: windowed("v".repeat(20_000)) } },
    };
    expect(set.receive(stream, event([forged], 100, "I")).kind).toBe("resubscribe");
    expect(set.get(PATH)).toBeUndefined();
  });

  it("applies an append of text and the new length, and verifies the result at a checkpoint", () => {
    const text = "a".repeat(2000);
    const more = "b".repeat(500);
    const { set, stream } = opened();
    set.receive(stream, event([snapshot(text, 5)], 100, "I"));
    const ops: Change = {
      path: PATH,
      from: 5,
      ops: [
        { op: "append", path: "/text/text", text: more },
        { op: "set", path: "/text/length", value: 2500 },
      ],
    };
    expect(set.receive(stream, event([ops], 120)).kind).toBe("applied");
    const check = { path: PATH, at: 120, hash: H(view(text + more)) };
    expect(set.receive(stream, { rev: 121, check: [check] }).kind).toBe("ignored");
    expect(set.get(PATH)).toMatchObject({ version: 120, verified: 120 });
  });

  it("applies the trim that moves the window over whole segments, and still verifies", () => {
    let text = "a".repeat(N + SEGMENT_LENGTH - 10);
    const { set, stream } = opened();
    set.receive(stream, event([snapshot(text, 5)], 100, "I"));
    // 3000 more bytes: the window then holds more than the rule allows, so its head is trimmed.
    const more = "é".repeat(1500);
    const next = text + more;
    const after = windowOf(next, N);
    const before = windowOf(text, N);
    expect(after.offset).toBeGreaterThan(before.offset);
    const units = next.slice(before.offset, after.offset).length;
    const ops: Change = {
      path: PATH,
      from: 5,
      ops: [
        { op: "append", path: "/text/text", text: more },
        { op: "set", path: "/text/length", value: after.length },
        { op: "trim", path: "/text/text", count: units },
        { op: "set", path: "/text/offset", value: after.offset },
        { op: "set", path: "/text/before", value: after.before },
      ],
    };
    expect(set.receive(stream, event([ops], 120)).kind).toBe("applied");
    expect(set.get(PATH)!.value).toEqual(windowed(next, N));
    const check = { path: PATH, at: 120, hash: H(view(next)) };
    expect(set.receive(stream, { rev: 121, check: [check] }).kind).toBe("ignored");
    expect(set.get(PATH)!.verified).toBe(120);
    text = next;
  });

  it("resubscribes whole when the held window disagrees with a checkpoint", () => {
    const text = "a".repeat(5000);
    const { set, stream } = opened();
    set.receive(stream, event([snapshot(text, 5)], 100, "I"));
    const wrong = { path: PATH, at: 5, hash: H(view(text + "x")) };
    expect(set.receive(stream, { rev: 101, check: [wrong] }).kind).toBe("resubscribe");
    expect(set.lastResubscribe).toMatchObject({ full: true, path: PATH });
  });
});

describe("a window that was asked for", () => {
  it("is not kept as what a replica holds when the path is followed with another window or none", () => {
    const text = "w".repeat(30_000);
    const set = new TreeReplicaSet();
    const first = set.open([{ path: PATH, window: N }]);
    set.receive(first.stream, event([snapshot(text, 5)], 100, "I"));
    expect(set.get(PATH)).toBeDefined();
    for (const window of [8192, undefined]) {
      const next = set.open([{ path: PATH, ...(window === undefined ? {} : { window }) }]);
      const params = new URL(next.url, "http://x").searchParams;
      expect(params.getAll("have"), `window ${window}`).toEqual([]);
      expect(set.get(PATH)).toBeUndefined();
    }
  });

  it("is kept when the same window is asked for again", () => {
    const set = new TreeReplicaSet();
    const first = set.open([{ path: PATH, window: N }]);
    set.receive(first.stream, event([snapshot("w".repeat(30_000), 5)], 100, "I"));
    const again = set.open([{ path: PATH, window: N }]);
    expect(new URL(again.url, "http://x").searchParams.getAll("have")).toEqual([`${PATH}:5`]);
  });

  it("is refused when the bridge sends a window of N + L bytes or more", () => {
    const text = "w".repeat(30_000);
    const { set, stream } = opened();
    const tooLong = { ...windowed(text), text: { ...windowOf(text, N), text: "w".repeat(N + SEGMENT_LENGTH) } };
    const change: Change = { path: PATH, snapshot: { base: null, at: 5, hash: H(view(text)), node: { v: tooLong } } };
    expect(set.receive(stream, event([change], 100, "I")).kind).toBe("resubscribe");
    expect(set.lastResubscribe?.reason).toMatch(/window/);
  });

  it("is refused when an append makes the held window N + L bytes or more, leaving the replica as it was", () => {
    const text = "a".repeat(N + SEGMENT_LENGTH - 10);
    const { set, stream } = opened();
    set.receive(stream, event([snapshot(text, 5)], 100, "I"));
    const ops: Change = {
      path: PATH,
      from: 5,
      ops: [
        { op: "append", path: "/text/text", text: "b".repeat(20) },
        { op: "set", path: "/text/length", value: text.length + 20 },
      ],
    };
    expect(set.receive(stream, event([ops], 120)).kind).toBe("resubscribe");
    expect(set.get(PATH)).toBeUndefined();
  });
});
