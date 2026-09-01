import { describe, expect, it } from "vitest";
import {
  applyTextEdits,
  assertRange,
  fullOrIncrementalChange,
  offsetAt,
  positionAt,
  unifiedTextDiff,
} from "../shared/nes";

describe("ACP NES UTF-16 document operations", () => {
  it("maps UTF-16 positions and produces a surrogate-safe incremental change", () => {
    expect(positionAt("a😀\nβ", 3)).toEqual({ line: 0, character: 3 });
    expect(offsetAt("a😀\nβ", { line: 1, character: 1 })).toBe(5);
    expect(fullOrIncrementalChange("a😀b", "a😁b", "incremental")).toEqual({
      range: {
        start: { line: 0, character: 1 },
        end: { line: 0, character: 3 },
      },
      text: "😁",
    });
    expect(fullOrIncrementalChange("old", "new", "full")).toEqual({ text: "new" });
  });

  it("applies non-overlapping edits against the original document", () => {
    expect(applyTextEdits("abcdef", [
      {
        range: { start: { line: 0, character: 1 }, end: { line: 0, character: 3 } },
        newText: "X",
      },
      {
        range: { start: { line: 0, character: 5 }, end: { line: 0, character: 6 } },
        newText: "Y",
      },
    ])).toBe("aXdeY");

    expect(() => applyTextEdits("abcdef", [
      {
        range: { start: { line: 0, character: 1 }, end: { line: 0, character: 4 } },
        newText: "X",
      },
      {
        range: { start: { line: 0, character: 3 }, end: { line: 0, character: 5 } },
        newText: "Y",
      },
    ])).toThrow("overlap");
    expect(() => assertRange("one", {
      start: { line: 1, character: 0 },
      end: { line: 1, character: 0 },
    })).toThrow("outside");
  });

  it("produces a bounded-context unified diff for edit history", () => {
    expect(unifiedTextDiff(
      "file:///workspace/sample.ts",
      "one\nold\nthree\n",
      "one\nnew\nthree\n",
    )).toBe([
      "--- file:///workspace/sample.ts",
      "+++ file:///workspace/sample.ts",
      "@@ -1,3 +1,3 @@",
      " one",
      "-old",
      "+new",
      " three",
    ].join("\n"));
    expect(unifiedTextDiff("file:///empty", "", "value\n", 0)).toContain(
      "@@ -0,0 +1,1 @@\n+value",
    );
    expect(unifiedTextDiff("file:///same", "same", "same")).toBe("");
  });
});
