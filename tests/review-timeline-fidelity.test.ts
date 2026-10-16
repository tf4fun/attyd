import { describe, expect, it } from "vitest";
import type { Chunk, LogEntry, Operation } from "../shared/protocol";
import { contentBlocksToMarkdown } from "../web/src/lib/thread-markdown";
import { splitTurnPresentation } from "../web/src/lib/turn-presentation";
import { projectTimeline } from "../web/src/lib/workspace-projection";

const text = (value: string) => ({ type: "text" as const, text: value });
const prompt = "Alpha Beta";
const completed: Operation = {
  id: "prompt-operation", kind: "prompt", status: "completed",
  input: { prompt: [text(prompt)] }, cancelRequested: false, interactions: {},
  result: { stopReason: "end_turn" },
};

function settled(items: LogEntry["turn"]["items"]): LogEntry[] {
  return [{
    hash: "settled-turn",
    turn: {
      items: [{ type: "message", role: "user", chunks: [{ content: text(prompt) }] }, ...items],
      end: { response: { stopReason: "end_turn" } },
    },
  }];
}

const answers: Array<{ name: string; chunks: Chunk[] }> = [
  {
    name: "text fragments with different chunk metadata",
    chunks: [
      { content: text("The answer is "), _meta: { part: "first" } },
      { content: text("42."), _meta: { part: "second" } },
    ],
  },
  {
    name: "text followed by an attachment",
    chunks: [
      { content: text("See the diagram and its explanation.") },
      { content: { type: "attachment", id: "a".repeat(64), block: "image", mimeType: "image/png" } },
    ],
  },
];

describe("settled timeline fidelity review", () => {
  for (const { name, chunks } of answers) {
    for (const phase of ["just settled", "loaded again"] as const) {
      it(`keeps the whole final message visible and copyable: ${name}, ${phase}`, () => {
        const log = settled([{
          type: "message", role: "agent", messageId: "answer", chunks,
        }]);
        const timeline = projectTimeline("session", log, phase === "just settled" ? completed : undefined);
        const presentation = splitTurnPresentation(timeline);
        // Conversation renders output outside the folded process. Its copy
        // button uses the Agent blocks of that output with this same formatter.
        const visibleAnswer = presentation.output?.chunks.flatMap((chunk) =>
          chunk.role === "agent" ? chunk.blocks : []
        ) ?? [];
        const expectedAnswer = chunks.map(({ content }) => content);

        expect.soft(visibleAnswer).toEqual(expectedAnswer);
        expect(contentBlocksToMarkdown(visibleAnswer)).toBe(contentBlocksToMarkdown(expectedAnswer));
      });
    }
  }

  it.each([prompt, "Beta"])("retains an Agent user item whose text is %j", (content) => {
    const log = settled([
      { type: "message", role: "agent", messageId: "before", chunks: [{ content: text("Checking.") }] },
      { type: "message", role: "user", messageId: "agent-user", chunks: [{ content: text(content) }] },
      { type: "message", role: "agent", messageId: "after", chunks: [{ content: text("Done.") }] },
    ]);
    const timeline = projectTimeline("session", log, undefined);
    const agentUser = timeline.filter((item) => item.type === "message" && item.messageId === "agent-user");

    expect(agentUser).toHaveLength(1);
    expect(agentUser[0]).toMatchObject({ blocks: [text(content)] });
    // It must reach the user-visible turn, even when its contents happen to
    // equal the original prompt or a substring of it (acceptance 26 and 30).
    expect(splitTurnPresentation(timeline).prompts).toContain(agentUser[0]);
  });
});
