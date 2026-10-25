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
const decoder = new TextDecoder("utf-8", { ignoreBOM: true });

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

/** A text position held as a window (docs/protocol.md §5.4). */
export interface TextWindow {
  readonly length: number;
  readonly offset: number;
  /** The chain state before the window's first segment: 32 bytes, unpadded base64url. */
  readonly before: string;
  readonly text: string;
}

export function isTextWindow(value: unknown): value is TextWindow {
  if (typeof value !== "object" || value === null) return false;
  const window = value as Record<string, unknown>;
  return (
    Number.isInteger(window.length) &&
    Number.isInteger(window.offset) &&
    typeof window.before === "string" &&
    typeof window.text === "string"
  );
}

function fromBase64url(text: string): Uint8Array {
  const binary = atob(text.replace(/-/g, "+").replace(/_/g, "/"));
  return Uint8Array.from(binary, (char) => char.charCodeAt(0));
}

/** The hash of the text a text position holds, whole or as a window. */
function positionHash(held: unknown): Hash {
  if (typeof held === "string") return textHash(held);
  const window = held as TextWindow;
  const before = fromBase64url(window.before);
  if (before.length !== 32) throw new Error("the chain state of a window is not 32 bytes");
  return windowHash(before, window.length, window.text);
}

function isTextBlock(block: unknown): block is { type: "text"; text: string | TextWindow } {
  return (
    typeof block === "object" &&
    block !== null &&
    (block as { type?: unknown }).type === "text" &&
    (typeof (block as { text?: unknown }).text === "string" || isTextWindow((block as { text?: unknown }).text))
  );
}

/** Replaces `holder[key]` by the placeholder of its chain hash. */
function base64url(hash: Uint8Array): string {
  let binary = "";
  for (const byte of hash) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** A copy of `value` with each text position of `rule` replaced by `"#" + b64(h(text))`. */
function withTextHashes(rule: TextRule, value: unknown, hashText = (_pointer: string, held: unknown): Hash => positionHash(held)): unknown {
  if (rule === "none" || typeof value !== "object" || value === null || Array.isArray(value)) return value;
  const copy = { ...value } as Record<string, unknown>;
  const blocks = (holders: unknown, key: string, pointer: string): unknown => {
    if (!Array.isArray(holders)) return holders;
    return holders.map((holder, index) => {
      const block = (holder as Record<string, unknown> | null)?.[key];
      if (!isTextBlock(block)) return holder;
      return { ...holder, [key]: { ...block, text: `#${base64url(hashText(`${pointer}/${index}/${key}/text`, block.text))}` } };
    });
  };
  if (rule === "terminal") {
    if (typeof copy.output === "string" || isTextWindow(copy.output)) copy.output = `#${base64url(hashText("/output", copy.output))}`;
  } else if (rule === "outputView") {
    if (typeof copy.text === "string" || isTextWindow(copy.text)) copy.text = `#${base64url(hashText("/text", copy.text))}`;
  } else if (copy.kind === "message") {
    if (Array.isArray(copy.chunks)) copy.chunks = blocks(copy.chunks, "content", "/chunks");
  } else if (copy.kind === "compaction") {
    if (Array.isArray(copy.summary)) copy.summary = blocks(copy.summary, "content", "/summary");
  } else if (copy.kind === "tool") {
    const call = copy.call as Record<string, unknown> | undefined;
    if (call !== undefined && call !== null && Array.isArray(call.content)) {
      copy.call = { ...call, content: call.content.map((item, index) => {
        if (item?.type !== "content" || !isTextBlock(item.content)) return item;
        return { ...item, content: { ...item.content,
          text: `#${base64url(hashText(`/call/content/${index}/content/text`, item.content.text))}` } };
      }) };
    }
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

interface TextChain {
  /** Start of the last segment, which is the only old segment an append can change. */
  readonly units: number;
  readonly bytes: number;
  readonly before: Hash;
  readonly hash: Hash;
}

function continuedText(text: string, seed: TextChain | undefined): TextChain {
  let units = seed?.units ?? 0;
  let bytes = seed?.bytes ?? 0;
  let state = seed?.before ?? ZERO;
  let lastUnits = units;
  let lastBytes = bytes;
  let before = state;
  for (const segment of segments(text.slice(units))) {
    lastUnits = units;
    lastBytes = bytes;
    before = state;
    state = chainStep(state, segment);
    units += decoder.decode(segment).length;
    bytes += segment.length;
  }
  return { units: lastUnits, bytes: lastBytes, before, hash: finish(bytes, state) };
}

interface ValueMemo {
  hash?: Hash;
  readonly chains: Map<string, TextChain>;
  readonly seed?: ReadonlyMap<string, TextChain>;
}

const cached = new WeakMap<object, Map<TextRule, ValueMemo>>();

/** Carry only computed chain states through proven text appends. No old value is retained. */
export function inheritTextHashes(rule: TextRule, before: unknown, after: unknown, paths: readonly string[]): void {
  if (typeof before !== "object" || before === null || typeof after !== "object" || after === null ||
      !Object.isFrozen(before) || !Object.isFrozen(after)) return;
  const previous = cached.get(before)?.get(rule);
  const seed = previous?.hash === undefined ? previous?.seed : previous.chains;
  if (seed === undefined) return;
  // A generic JSON string append is not necessarily a protocol text position.
  const positions = new Set<string>();
  withTextHashes(rule, before, (pointer, held) => {
    if (typeof held === "string") positions.add(pointer);
    return ZERO;
  });
  if (!paths.every((path) => positions.has(path))) return;
  let byRule = cached.get(after);
  if (byRule === undefined) {
    byRule = new Map();
    cached.set(after, byRule);
  }
  byRule.set(rule, { chains: new Map(), seed });
}

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
  let memo = byRule.get(rule);
  if (memo === undefined) {
    memo = { chains: new Map() };
    byRule.set(rule, memo);
  }
  if (memo.hash === undefined) {
    const held = memo;
    const replaced = withTextHashes(rule, value, (pointer, text) => {
      if (typeof text !== "string") return positionHash(text);
      const chain = continuedText(text, held.seed?.get(pointer));
      held.chains.set(pointer, chain);
      return chain.hash;
    });
    memo.hash = sha256(concat([encoder.encode("v"), encoder.encode(canonicalJson(replaced))]));
  }
  return memo.hash;
}
