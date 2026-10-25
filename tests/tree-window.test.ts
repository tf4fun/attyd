import { describe, expect, it } from "vitest";
import { hex } from "../shared/digest";
import { valueHash } from "../shared/tree-hash";
import { windowOf } from "./fixtures/text-window";

const texts = [
  "",
  "short",
  "x".repeat(4096),
  "y".repeat(4097),
  "a".repeat(10_000) + "é".repeat(3000) + "😀".repeat(1500),
  "line\n".repeat(5000),
];

describe("a value that holds its text positions as windows", () => {
  it.each(texts.flatMap((text) => [4096, 5000, 20_000].map((bytes) => [text.length, bytes, text] as const)))(
    "hashes as the value that holds the whole text (%i characters, N = %i)",
    (_length, bytes, text) => {
      const view = { turn: null, entry: 3, text, other: 0 };
      const windowed = { ...view, text: windowOf(text, bytes) };
      expect(hex(valueHash("outputView", windowed))).toBe(hex(valueHash("outputView", view)));
    },
  );

  it("does so for the text blocks of an entry and for a terminal's output", () => {
    const text = "m".repeat(9000);
    const entry = { kind: "message", role: "agent", messageId: "m", chunks: [{ content: { type: "text", text } }] };
    const windowed = { ...entry, chunks: [{ content: { type: "text", text: windowOf(text, 4096) } }] };
    expect(hex(valueHash("entry", windowed))).toBe(hex(valueHash("entry", entry)));
    const terminal = { output: text, truncated: false };
    expect(hex(valueHash("terminal", { ...terminal, output: windowOf(text, 4096) }))).toBe(hex(valueHash("terminal", terminal)));
  });

  it("does not hash a window whose bytes were altered as the text it stands for", () => {
    const text = "z".repeat(9000);
    const window = windowOf(text, 4096);
    const altered = { ...window, text: window.text.slice(0, -1) + "!" };
    expect(hex(valueHash("outputView", { text: altered }))).not.toBe(hex(valueHash("outputView", { text })));
  });
});
