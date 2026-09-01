import { describe, expect, it } from "vitest";
import { collectReviewChanges } from "../src/lib/review-changes";
import type { TimelineItem } from "../src/lib/state";

describe("ACP change review aggregation", () => {
  it("groups Agent-reported diffs by file and computes exact line changes", () => {
    const summary = collectReviewChanges([
      tool("edit-one", "/workspace/a.ts", "one\ntwo\nthree\n", "one\nchanged\nthree\n"),
      tool("create", "/workspace/b.ts", null, "alpha\nbeta\n"),
      tool("edit-two", "/workspace/a.ts", "x\n", "x\ny\n"),
    ]);

    expect(summary.fileCount).toBe(2);
    expect(summary.diffCount).toBe(3);
    expect(summary.addedLines).toBe(4);
    expect(summary.removedLines).toBe(1);
    expect(summary.approximate).toBe(false);
    expect(summary.files[0].diffs).toHaveLength(2);
    expect(summary.files[0].diffs[0].lines.map(({ kind }) => kind))
      .toEqual(["context", "removed", "added", "context"]);
  });

  it("ignores tool calls that do not contain ACP diff content", () => {
    const summary = collectReviewChanges([{
      id: "read",
      type: "tool",
      call: {
        toolCallId: "read",
        title: "Read file",
        content: [{ type: "content", content: { type: "text", text: "hello" } }],
      },
      raw: [],
    }]);
    expect(summary).toEqual({
      files: [],
      fileCount: 0,
      diffCount: 0,
      addedLines: 0,
      removedLines: 0,
      approximate: false,
    });
  });

  it("bounds large diff rendering and labels its line matching approximate", () => {
    const oldText = Array.from({ length: 1_200 }, (_, index) => `old ${index}`).join("\n");
    const newText = Array.from({ length: 1_200 }, (_, index) => `new ${index}`).join("\n");
    const summary = collectReviewChanges([tool("large", "/workspace/large.ts", oldText, newText)]);
    const diff = summary.files[0].diffs[0];

    expect(summary.approximate).toBe(true);
    expect(diff.addedLines).toBe(1_200);
    expect(diff.removedLines).toBe(1_200);
    expect(diff.lines.length).toBeLessThanOrEqual(800);
    expect(diff.truncated).toBe(true);
  });
});

function tool(
  toolCallId: string,
  path: string,
  oldText: string | null,
  newText: string,
): Extract<TimelineItem, { type: "tool" }> {
  return {
    id: toolCallId,
    type: "tool",
    call: {
      toolCallId,
      title: toolCallId,
      status: "completed",
      content: [{ type: "diff", path, oldText, newText }],
    },
    raw: [],
  };
}
