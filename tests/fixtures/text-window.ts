// The window of a text as the bridge sends it (docs/protocol.md §5.4), for the cases of a client
// that holds text as windows: the shortest run of whole trailing segments of at least `bytes`.
import { segments, chainStep, ZERO, type TextWindow } from "../../shared/tree-hash";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** The window of `text` for `bytes = N`. */
export function windowOf(text: string, bytes: number): TextWindow {
  const all = [...segments(text)];
  let held = 0;
  let first = all.length;
  while (first > 0 && held < bytes) {
    first -= 1;
    held += all[first].length;
  }
  if (held < bytes) first = 0;
  let before = ZERO;
  for (const segment of all.slice(0, first)) before = chainStep(before, segment);
  const offset = all.slice(0, first).reduce((total, segment) => total + segment.length, 0);
  return {
    length: encoder.encode(text).length,
    offset,
    // The chain state goes whole on the wire (32 bytes), not as the 16-byte node hash.
    before: toBase64Url(before),
    text: decoder.decode(all.slice(first).reduce((tail, segment) => concat(tail, segment), new Uint8Array(0))),
  };
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a);
  out.set(b, a.length);
  return out;
}

export function toBase64Url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

