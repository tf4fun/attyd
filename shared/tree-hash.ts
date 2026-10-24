// Content hashes of protocol revision 4 (docs/protocol-r4-design.md §4.6, §5), the client half of
// src/protocol/hash.rs. Both reproduce tests/fixtures/protocol-r4-hash-vectors.json.
//
// Text is hashed as a chain over fixed 4 KiB UTF-8 segments, so a window of a text can be
// verified and continued from a 32-byte chain state. Node hashes are 32 bytes and go on the
// wire as `wire(hash)` (the first 16 bytes). SHA-256 and JCS are those of `./digest`.

import { canonicalJson, sha256, type Hash } from "./digest";

export type { Hash };

/** The segment length of a text position, in UTF-8 bytes. It is part of every text hash. */
export const SEGMENT_LENGTH = 4096;

/** The chain state before the first segment. */
export const ZERO: Hash = new Uint8Array(32);

const encoder = new TextEncoder();

function concat(parts: readonly Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((total, part) => total + part.length, 0));
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}

function u64be(value: number): Uint8Array {
  const out = new Uint8Array(8);
  const view = new DataView(out.buffer);
  view.setUint32(0, Math.floor(value / 2 ** 32));
  view.setUint32(4, value >>> 0);
  return out;
}

/** The segments of `text` (greedy, at most `SEGMENT_LENGTH` UTF-8 bytes, on code point boundaries). */
export function* segments(text: string): Generator<Uint8Array> {
  const bytes = encoder.encode(text);
  let start = 0;
  while (start < bytes.length) {
    let end = Math.min(start + SEGMENT_LENGTH, bytes.length);
    // A continuation byte is 10xxxxxx: step back to the start of the code point.
    while (end < bytes.length && (bytes[end] & 0xc0) === 0x80) end -= 1;
    yield bytes.subarray(start, end);
    start = end;
  }
}

/** `c_i = H(c_{i-1} ‖ s_i)`. */
export function chainStep(before: Hash, segment: Uint8Array): Hash {
  return sha256(concat([before, segment]));
}

/** The chain states after each segment of `text`. */
export function chainStates(text: string): Hash[] {
  const states: Hash[] = [];
  let state = ZERO;
  for (const segment of segments(text)) {
    state = chainStep(state, segment);
    states.push(state);
  }
  return states;
}

function finish(length: number, last: Hash): Hash {
  return sha256(concat([encoder.encode("t"), u64be(length), last]));
}

/** The hash of a whole text. */
export function textHash(text: string): Hash {
  const states = chainStates(text);
  return finish(encoder.encode(text).length, states.length === 0 ? ZERO : states[states.length - 1]);
}

/**
 * The hash of a text of `length` bytes of which the tail `text` is held, starting at a segment
 * boundary whose chain state is `before`; equal to `textHash` of the whole text.
 */
export function windowHash(before: Hash, length: number, text: string): Hash {
  let state = before;
  for (const segment of segments(text)) state = chainStep(state, segment);
  return finish(length, state);
}

/** Where a value node keeps its text positions (§4.6). */
export type TextRule = "none" | "entry" | "terminal" | "outputView";

function isTextBlock(block: unknown): block is { type: "text"; text: string } {
  return (
    typeof block === "object" &&
    block !== null &&
    (block as { type?: unknown }).type === "text" &&
    typeof (block as { text?: unknown }).text === "string"
  );
}

/** Replaces `holder[key]` by the placeholder of its chain hash. */
function replaceText(holder: Record<string, unknown>): void {
  holder.text = `#${base64url(textHash(holder.text as string))}`;
}

function base64url(hash: Uint8Array): string {
  let binary = "";
  for (const byte of hash) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** A copy of `value` with each text position of `rule` replaced by `"#" + b64(h(text))`. */
function withTextHashes(rule: TextRule, value: unknown): unknown {
  if (rule === "none" || typeof value !== "object" || value === null) return value;
  const copy = JSON.parse(JSON.stringify(value)) as Record<string, unknown>;
  const blocks = (holders: unknown, key: string): void => {
    for (const holder of Array.isArray(holders) ? holders : []) {
      const block = (holder as Record<string, unknown> | null)?.[key];
      if (isTextBlock(block)) replaceText(block as unknown as Record<string, unknown>);
    }
  };
  if (rule === "terminal") {
    if (typeof copy.output === "string") copy.output = `#${base64url(textHash(copy.output))}`;
  } else if (rule === "outputView") {
    if (typeof copy.text === "string") copy.text = `#${base64url(textHash(copy.text))}`;
  } else if (copy.kind === "message") {
    blocks(copy.chunks, "content");
  } else if (copy.kind === "compaction") {
    blocks(copy.summary, "content");
  } else if (copy.kind === "tool") {
    const content = (copy.call as { content?: unknown } | undefined)?.content;
    blocks(
      Array.isArray(content) ? content.filter((item) => (item as { type?: unknown })?.type === "content") : [],
      "content",
    );
  }
  return copy;
}

/** `h = H("v" ‖ canon(value'))`. */
export function valueHash(rule: TextRule, value: unknown): Hash {
  return sha256(concat([encoder.encode("v"), encoder.encode(canonicalJson(withTextHashes(rule, value)))]));
}

function byUtf16(a: string, b: string): number {
  // JavaScript compares strings by UTF-16 code units.
  return a < b ? -1 : a > b ? 1 : 0;
}

function named(children: ReadonlyMap<string, Hash> | Record<string, Hash>): Uint8Array[] {
  const entries = children instanceof Map ? [...children] : Object.entries(children);
  return entries
    .sort(([a], [b]) => byUtf16(a, b))
    .flatMap(([name, hash]) => [encoder.encode(canonicalJson(name)), hash]);
}

/** `h = H("o" ‖ canon(own) ‖ ∥ (canon(name) ‖ h(child)))`. */
export function objectHash(own: Record<string, unknown>, children: ReadonlyMap<string, Hash> | Record<string, Hash>): Hash {
  return sha256(concat([encoder.encode("o"), encoder.encode(canonicalJson(own)), ...named(children)]));
}

/** `h = H("m" ‖ ∥ (canon(key) ‖ h(child)))`. */
export function mapHash(children: ReadonlyMap<string, Hash> | Record<string, Hash>): Hash {
  return sha256(concat([encoder.encode("m"), ...named(children)]));
}

/** `h = H("l" ‖ u64be(n) ‖ h(child_0) ‖ … ‖ h(child_{n-1}))`. */
export function listHash(children: readonly Hash[]): Hash {
  return sha256(concat([encoder.encode("l"), u64be(children.length), ...children]));
}

const cached = new WeakMap<object, Map<TextRule, Hash>>();

/**
 * `valueHash` remembered by the identity of the value, which a replica freezes: a value that did
 * not change is not hashed again. Primitives are hashed each time.
 */
export function cachedValueHash(rule: TextRule, value: unknown): Hash {
  if (typeof value !== "object" || value === null) return valueHash(rule, value);
  let byRule = cached.get(value);
  if (byRule === undefined) {
    byRule = new Map();
    cached.set(value, byRule);
  }
  let hash = byRule.get(rule);
  if (hash === undefined) {
    hash = valueHash(rule, value);
    byRule.set(rule, hash);
  }
  return hash;
}
