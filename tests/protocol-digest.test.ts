// The client digest (protocol §5.5). SHA-256 is checked against the FIPS 180-4
// vectors and against Node's own implementation at every length that crosses a
// block boundary; the digests against the vectors an independent implementation
// computed, which src/protocol/digest.rs reproduces too.
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  canonicalJson,
  entriesDigest,
  entryHash,
  hex,
  scopeDigest,
  sessionDigest,
  sha256,
  wire,
} from "../shared/digest";

const encoder = new TextEncoder();

interface Vector {
  name: string;
  kind: "session" | "scope";
  doc: Record<string, unknown>;
  entryHashes?: string[] | null;
  entriesDigest?: string;
  digest: string;
}

const vectors = (
  JSON.parse(readFileSync(new URL("./fixtures/protocol-digest-vectors.json", import.meta.url), "utf8")) as {
    cases: Vector[];
  }
).cases;

function split(doc: Record<string, unknown>): { head: Record<string, unknown>; entries: unknown[] | null } {
  const { entries, ...head } = doc;
  return { head, entries: (entries as unknown[] | undefined) ?? null };
}

describe("sha256", () => {
  it.each([
    ["", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"],
    ["abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"],
    [
      "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
      "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
    ],
    ["a".repeat(1_000_000), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"],
  ])("matches the FIPS 180-4 vector for a message of %#", (message, expected) => {
    expect(hex(sha256(encoder.encode(message)))).toBe(expected);
  });

  it("matches Node at every length across the block boundaries", () => {
    for (let length = 0; length <= 200; length += 1) {
      const data = Uint8Array.from({ length }, (_, index) => (index * 31 + length) & 0xff);
      expect(hex(sha256(data)), `length ${length}`).toBe(createHash("sha256").update(data).digest("hex"));
    }
  });
});

describe("canonicalJson", () => {
  it("orders keys by UTF-16 code units and drops undefined members", () => {
    expect(canonicalJson({ b: 1, a: { d: [3, 2], c: null }, u: undefined })).toBe('{"a":{"c":null,"d":[3,2]},"b":1}');
  });

  it("escapes as JCS does and writes numbers as ECMAScript does", () => {
    expect(canonicalJson("a\"b\\c\n\u0001é")).toBe('"a\\"b\\\\c\\n\\u0001é"');
    expect(canonicalJson([1, 1.5, -0, 1e21, 100])).toBe("[1,1.5,0,1e+21,100]");
  });

  it("refuses what is not JSON", () => {
    expect(() => canonicalJson(Number.NaN)).toThrow(TypeError);
    expect(() => canonicalJson(() => 1)).toThrow(TypeError);
  });
});

describe("digests", () => {
  it.each(vectors.map((vector) => [vector.name, vector] as const))("match the independent vector: %s", (_, vector) => {
    if (vector.kind === "scope") {
      expect(scopeDigest(vector.doc)).toBe(vector.digest);
      return;
    }
    const { head, entries } = split(vector.doc);
    const hashes = entries === null ? null : entries.map(entryHash);
    expect(sessionDigest(head, hashes)).toBe(vector.digest);
    expect(hashes === null ? null : hashes.map(hex)).toEqual(vector.entryHashes ?? null);
    expect(hex(entriesDigest(hashes))).toBe(vector.entriesDigest);
  });

  it("tells an unknown history from an empty one", () => {
    expect(hex(entriesDigest(null))).not.toBe(hex(entriesDigest([])));
  });

  it("writes a hash as 22 url-safe characters", () => {
    expect(wire(sha256(encoder.encode("abc")))).toMatch(/^[A-Za-z0-9_-]{22}$/);
  });

  it("changes with any part of the document and not with the key order", () => {
    const document = {
      epoch: "e",
      rev: 1,
      history: "complete",
      entries: [{ kind: "message", role: "user", chunks: [] }],
      prompts: [],
      live: { closing: false },
    };
    const digest = (doc: Record<string, unknown>) => {
      const { head, entries } = split(doc);
      return sessionDigest(head, entries === null ? null : entries.map(entryHash));
    };
    const original = digest(document);
    for (const changed of [
      { ...document, epoch: "other" },
      { ...document, rev: 2 },
      { ...document, history: "partial" },
      { ...document, prompts: [{ entry: 0 }] },
      { ...document, live: { closing: true } },
      { ...document, entries: [{ kind: "message", role: "agent", chunks: [] }] },
    ]) {
      expect(digest(changed)).not.toBe(original);
    }
    const reordered = JSON.parse(
      '{"live":{"closing":false},"prompts":[],"history":"complete","entries":[{"chunks":[],"role":"user","kind":"message"}],"rev":1,"epoch":"e"}',
    ) as Record<string, unknown>;
    expect(digest(reordered)).toBe(original);
  });
});
