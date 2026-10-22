// The thread of a session projected from the entries of its document
// (docs/protocol.md §3, bridge memory model §3–§5): messages, tool calls, plans and
// compactions in the order they were received, the turns they form, and how each prompt
// ended. Every value is computed afresh from the document.
import { describe, expect, it } from "vitest";
import { projectEntries } from "../web/src/lib/entry-projection";
import type { Entry, PromptState } from "../shared/entries";

const text = (value: string) => ({ type: "text", text: value });
const user = (value: string, messageId?: string): Entry => ({
  kind: "message", role: "user", ...(messageId ? { messageId } : {}), chunks: [{ content: text(value) }],
});
const agent = (...parts: string[]): Entry => ({
  kind: "message", role: "agent", chunks: parts.map((part) => ({ content: text(part) })),
});
const thought = (value: string): Entry => ({ kind: "message", role: "thought", chunks: [{ content: text(value) }] });
const tool = (toolCallId: string, extra: Record<string, unknown> = {}): Entry => ({
  kind: "tool", call: { toolCallId, ...extra },
});
const legacyPlan = (...statuses: string[]): Entry => ({
  kind: "plan", legacy: { entries: statuses.map((status) => ({ content: "step", priority: "high", status })) },
});
const ended = (entry: number, end: PromptState["end"]): PromptState => ({ entry, ...(end ? { end } : {}) });

const project = (entries: Entry[], prompts: PromptState[] = []) => projectEntries("S", entries, prompts);

describe("entries in a thread", () => {
  it("shows each message in order, an agent reply as one assistant item", () => {
    const { timeline } = project([user("hi"), agent("Hel", "lo"), thought("hmm"), agent("done")]);
    expect(timeline.map((item) => item.type)).toEqual(["message", "assistant"]);
    const [message, assistant] = timeline;
    expect(message).toMatchObject({ role: "user", blocks: [text("hi")] });
    // Consecutive assistant messages are one item, one chunk each; adjacent text is joined.
    expect(assistant).toMatchObject({
      type: "assistant",
      chunks: [
        { role: "agent", blocks: [text("Hello")] },
        { role: "thought", blocks: [text("hmm")] },
        { role: "agent", blocks: [text("done")] },
      ],
    });
  });

  it("keeps the tool calls between messages, which a log of final messages lost", () => {
    const { timeline } = project([
      user("run it"),
      agent("Running"),
      tool("t1", { title: "Run", status: "completed" }),
      agent("Done"),
    ]);
    expect(timeline.map((item) => item.type)).toEqual(["message", "assistant", "tool", "assistant"]);
    expect(timeline[2]).toMatchObject({ type: "tool", call: { toolCallId: "t1", title: "Run", status: "completed" } });
  });

  it("gives a tool call without a title the one the thread shows for it", () => {
    const { timeline } = project([tool("t1")]);
    expect(timeline[0]).toMatchObject({ type: "tool", call: { toolCallId: "t1", title: "Tool" } });
  });

  it("identifies an item by the entry it comes from", () => {
    const { timeline } = project([user("a"), agent("b")]);
    expect(timeline.map((item) => item.id)).toEqual(["entry:0", "entry:1"]);
  });

  it("shows a compaction with its summary and status", () => {
    const { timeline } = project([
      {
        kind: "compaction", compactionId: "c", status: "completed",
        summary: [{ content: text("Sum") }, { content: text("mary") }],
      },
    ]);
    expect(timeline[0]).toMatchObject({
      type: "compaction", compactionId: "c", status: "completed", blocks: [text("Summary")],
    });
  });

  it("shows the plan an id names and a removal", () => {
    const { timeline } = project([
      { kind: "plan", update: { plan: { planId: "p", entries: [{ content: "a", priority: "high", status: "completed" }] } } },
      { kind: "plan", update: { planId: "p" } },
    ]);
    expect(timeline[0]).toMatchObject({ type: "plan", update: { sessionUpdate: "plan_update", plan: { planId: "p" } } });
    expect(timeline[1]).toMatchObject({ type: "plan", update: { sessionUpdate: "plan_removed", planId: "p" } });
  });
});

describe("turns", () => {
  it("marks every item of a turn with the turn its prompt opened", () => {
    const { timeline } = project([user("one"), agent("a"), user("two"), agent("b")]);
    expect(timeline.map((item) => item.historyTurnId)).toEqual(["turn:0", "turn:0", "turn:2", "turn:2"]);
  });

  it("does not split a turn at a user message with the same messageId", () => {
    const { timeline } = project([user("one", "m"), agent("a"), user("again", "m"), agent("b")]);
    expect(new Set(timeline.map((item) => item.historyTurnId))).toEqual(new Set(["turn:0"]));
  });

  it("puts entries before the first user message in a turn of their own", () => {
    const { timeline } = project([agent("earlier"), user("hi")]);
    expect(timeline.map((item) => item.historyTurnId)).toEqual(["turn:0", "turn:1"]);
  });
});

describe("how a prompt ended", () => {
  it("closes its turn with the Agent's stop reason", () => {
    const { timeline } = project(
      [user("hi"), agent("hello"), user("next")],
      [ended(0, { response: { stopReason: "end_turn" } })],
    );
    expect(timeline.map((item) => item.type)).toEqual(["message", "assistant", "stop", "message"]);
    expect(timeline[2]).toMatchObject({ type: "stop", response: { stopReason: "end_turn" }, historyTurnId: "turn:0" });
  });

  it("closes a turn the Agent answered with an error, which retries what was asked", () => {
    const { timeline } = project(
      [user("hi")],
      [ended(0, { error: { code: -32_000, message: "auth needed", data: { x: 1 } } })],
    );
    expect(timeline[1]).toMatchObject({
      type: "error", message: "auth needed", code: -32_000, operation: "session/prompt",
      retryBlocks: [text("hi")],
    });
  });

  it("says a prompt the connection cut short got no answer", () => {
    const { timeline } = project([user("hi"), agent("part")], [ended(0, { interrupted: true })]);
    expect(timeline[2]).toMatchObject({ type: "error", operation: "session/prompt" });
    expect((timeline[2] as { message: string }).message).toMatch(/connection/i);
  });

  it("adds nothing for a prompt that is still running", () => {
    const { timeline } = project([user("hi"), agent("part")], [ended(0)]);
    expect(timeline.map((item) => item.type)).toEqual(["message", "assistant"]);
  });
});

describe("what the Agent is doing", () => {
  it("is nothing while no prompt runs", () => {
    expect(project([user("hi"), agent("done")], [ended(0, { response: { stopReason: "end_turn" } })]).activity).toBeUndefined();
  });

  it("follows the last entry of a running prompt", () => {
    const running = [ended(0)];
    expect(project([user("hi")], running).activity).toEqual({ kind: "waiting" });
    expect(project([user("hi"), agent("x")], running).activity).toEqual({ kind: "responding" });
    expect(project([user("hi"), thought("x")], running).activity).toEqual({ kind: "thinking", timelineId: "entry:1" });
    expect(project([user("hi"), tool("t", { title: "Run", status: "in_progress" })], running).activity)
      .toEqual({ kind: "tool", toolCallId: "t", title: "Run" });
    expect(project([user("hi"), tool("t", { status: "completed" })], running).activity).toEqual({ kind: "waiting" });
    expect(project([user("hi"), legacyPlan("pending")], running).activity).toEqual({ kind: "planning" });
    expect(project([user("hi"), { kind: "compaction", compactionId: "c", status: "in_progress", summary: [] }], running).activity)
      .toEqual({ kind: "compacting" });
  });
});

describe("the plan above the composer", () => {
  it("is the last plan of a running prompt while it has unfinished steps, and not in the thread", () => {
    const { timeline, activePlan } = project([user("hi"), legacyPlan("completed"), legacyPlan("completed", "pending")], [ended(0)]);
    expect(activePlan).toMatchObject({ type: "plan", id: "entry:2" });
    expect(timeline.filter((item) => item.type === "plan").map((item) => item.id)).toEqual(["entry:1"]);
  });

  it("is none once every step is done or no prompt runs: the plan stays in its turn", () => {
    const entries = [user("hi"), legacyPlan("pending")];
    expect(project(entries, [ended(0, { response: { stopReason: "end_turn" } })]).activePlan).toBeUndefined();
    expect(project(entries, [ended(0, { response: { stopReason: "end_turn" } })]).timeline.some((item) => item.type === "plan")).toBe(true);
    expect(project([user("hi"), legacyPlan("completed")], [ended(0)]).activePlan).toBeUndefined();
  });
});
