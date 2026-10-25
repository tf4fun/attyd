// The primitives of the client's hashes (design §5). SHA-256 is checked against the FIPS 180-4
// vectors and against Node's own implementation at every length that crosses a block boundary;
// the hashes built on them are checked in protocol-r4-hash.test.ts.
import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";
import { canonicalJson, hex, sha256, wire } from "../shared/digest";

const encoder = new TextEncoder();

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

describe("wire", () => {
  it("writes a hash as 22 url-safe characters", () => {
    expect(wire(sha256(encoder.encode("abc")))).toMatch(/^[A-Za-z0-9_-]{22}$/);
  });
});
