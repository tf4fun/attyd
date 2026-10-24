// The client's revision 4 hashes (design §4.6, §5) against the vectors an independent
// implementation computed (tests/fixtures/protocol_r4_vectors.py); src/protocol/hash.rs
// reproduces the same file.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { hex } from "../shared/digest";
import {
  SEGMENT_LENGTH,
  type Hash,
  type TextRule,
  cachedValueHash,
  chainStates,
  listHash,
  mapHash,
  objectHash,
  segments,
  textHash,
  valueHash,
  windowHash,
} from "../shared/tree-hash";

interface NodeVector {
  kind: "value" | "object" | "map" | "list";
  rule?: TextRule;
  value?: unknown;
  fields?: Record<string, unknown>;
  children?: Record<string, NodeVector>;
  items?: NodeVector[];
}

const vectors = JSON.parse(readFileSync(new URL("./fixtures/protocol-r4-hash-vectors.json", import.meta.url), "utf8")) as {
  L: number;
  text: { name: string; text: string; length: number; segments: number[]; chain: string[]; hash: string }[];
  windows: { name: string; length: number; offset: number; before: string; text: string; hash: string }[];
  values: { name: string; rule: TextRule; value: unknown; hash: string }[];
  nodes: { name: string; node: NodeVector; hash: string }[];
};

function unhex(text: string): Hash {
  return Uint8Array.from(text.match(/../g) ?? [], (byte) => Number.parseInt(byte, 16));
}

function node(tree: NodeVector): Hash {
  switch (tree.kind) {
    case "value":
      return valueHash(tree.rule ?? "none", tree.value);
    case "object":
      return objectHash(tree.fields ?? {}, new Map(Object.entries(tree.children ?? {}).map(([k, v]) => [k, node(v)])));
    case "map":
      return mapHash(new Map(Object.entries(tree.children ?? {}).map(([k, v]) => [k, node(v)])));
    case "list":
      return listHash((tree.items ?? []).map(node));
  }
}

describe("revision 4 hashes", () => {
  it("uses the vectors' segment length", () => {
    expect(SEGMENT_LENGTH).toBe(vectors.L);
  });

  it.each(vectors.text)("splits and chains text: $name", (vector) => {
    expect([...segments(vector.text)].map((segment) => segment.length)).toEqual(vector.segments);
    expect(chainStates(vector.text).map(hex)).toEqual(vector.chain);
    expect(hex(textHash(vector.text))).toBe(vector.hash);
  });

  it.each(vectors.windows)("continues a window from a chain state: $name", (vector) => {
    expect(hex(windowHash(unhex(vector.before), vector.length, vector.text))).toBe(vector.hash);
  });

  it.each(vectors.values)("hashes a value: $name", (vector) => {
    expect(hex(valueHash(vector.rule, vector.value))).toBe(vector.hash);
  });

  it.each(vectors.nodes)("hashes a node: $name", (vector) => {
    expect(hex(node(vector.node))).toBe(vector.hash);
  });

  it("leaves a value without the members a rule looks at as plain JSON", () => {
    for (const entry of [{ kind: "message", role: "user" }, { kind: "tool" }, { kind: "tool", call: {} }, { kind: "plan" }]) {
      expect(hex(valueHash("entry", entry))).toBe(hex(valueHash("none", entry)));
    }
  });

  it("keeps the kinds in their own domains", () => {
    expect(hex(objectHash({}, new Map()))).not.toBe(hex(mapHash(new Map())));
    expect(hex(mapHash(new Map()))).not.toBe(hex(listHash([])));
  });

  it("remembers the hash of a value by its identity", () => {
    const entry = Object.freeze({ kind: "message", role: "agent", chunks: [] });
    const first = cachedValueHash("entry", entry);
    expect(cachedValueHash("entry", entry)).toBe(first);
    expect(hex(first)).toBe(hex(valueHash("entry", entry)));
    expect(cachedValueHash("none", entry)).not.toBe(first);
    expect(hex(cachedValueHash("none", { ...entry }))).toBe(hex(valueHash("none", entry)));
  });
});
