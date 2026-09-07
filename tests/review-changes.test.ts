import { describe, expect, it } from "vitest";
import { collectReviewChanges, collectTurnReviewChanges } from "../web/src/lib/review-changes";
import type { TimelineItem } from "../web/src/lib/state";

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

describe("turn-scoped ACP change reviews", () => {
  it("keeps edits to the same file in separate turns and leaves unmodified turns empty", () => {
    const turns = collectTurnReviewChanges([
      prompt("first"),
      tool("first-edit", "/workspace/a.ts", "one", "two"),
      { id: "first-stop", type: "stop", response: { stopReason: "end_turn" } },
      prompt("second"),
      tool("second-edit", "/workspace/a.ts", "two", "three\nfour"),
      { id: "second-stop", type: "stop", response: { stopReason: "cancelled" } },
      prompt("third"),
    ]);

    expect(turns.map(({ id }) => id)).toEqual(["first", "second", "third"]);
    expect(turns.map(({ summary }) => summary.diffCount)).toEqual([1, 1, 0]);
    expect(turns[0].summary.files[0].diffs[0].newText).toBe("two");
    expect(turns[1].summary.files[0].diffs[0].newText).toBe("three\nfour");
    expect(turns[1].items.at(-1)?.id).toBe("second-stop");
  });

  it("separates loaded prompts even when their history has no turn outcomes", () => {
    const turns = collectTurnReviewChanges([
      { ...prompt("loaded-first"), role: "protocol-user" },
      tool("first-edit", "/workspace/a.ts", null, "one"),
      { ...prompt("loaded-second"), role: "protocol-user" },
      tool("second-edit", "/workspace/b.ts", null, "two"),
    ]);

    expect(turns.map(({ id }) => id)).toEqual(["loaded-first", "loaded-second"]);
    expect(turns.map(({ summary }) => summary.files.map(({ path }) => path)))
      .toEqual([["/workspace/a.ts"], ["/workspace/b.ts"]]);
  });

  it("keeps user chunks from the same operation together", () => {
    const turns = collectTurnReviewChanges([
      { ...prompt("first-chunk"), turnOperationId: "operation-one" },
      { ...prompt("second-chunk"), turnOperationId: "operation-one" },
      tool("edit", "/workspace/a.ts", null, "one"),
      { ...prompt("next-prompt"), turnOperationId: "operation-two" },
    ]);

    expect(turns.map(({ id }) => id)).toEqual(["first-chunk", "next-prompt"]);
    expect(turns[0].items.map(({ id }) => id)).toEqual(["first-chunk", "second-chunk", "edit"]);
  });

  it("uses outcomes and prompt failures as boundaries when user messages are absent", () => {
    const turns = collectTurnReviewChanges([
      tool("first-edit", "/workspace/a.ts", null, "one"),
      { id: "stop", type: "stop", response: { stopReason: "end_turn" } },
      tool("second-edit", "/workspace/b.ts", null, "two"),
      { id: "failure", type: "error", operation: "session/prompt", message: "Turn failed" },
      tool("third-edit", "/workspace/c.ts", null, "three"),
    ]);

    expect(turns.map(({ summary }) => summary.diffCount)).toEqual([1, 1, 1]);
    expect(turns[1].items.at(-1)?.id).toBe("failure");
  });
});

function prompt(id: string): Extract<TimelineItem, { type: "message" }> {
  return { id, type: "message", role: "user", blocks: [{ type: "text", text: id }], raw: [] };
}

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
