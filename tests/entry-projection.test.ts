// The thread of a session projected from the entries of its document
// (docs/protocol.md §3, bridge memory model §3–§5): messages, tool calls, plans and
// compactions in the order they were received, the turns they form, and how each prompt
// ended. Every value follows from the document; an item is reused only for the entries it came from.
import { describe, expect, it } from "vitest";
import type { PlanEntry as AcpPlanEntry } from "@agentclientprotocol/sdk";
import { projectEntries } from "../web/src/lib/entry-projection";
import type { Entry, PromptState, TurnHead } from "../shared/entries";
import { turnHeads } from "../web/src/dev/turn-heads";

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
const ended = (entry: number, end?: PromptState["end"]): PromptState => ({ entry, ...(end ? { end } : {}) });

const project = (entries: Entry[], prompts: PromptState[] = [], turns: TurnHead[] = turnHeads(entries, prompts)) =>
  projectEntries("S", entries, prompts, turns);

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
    const step = { content: "a", priority: "high", status: "completed" } satisfies AcpPlanEntry;
    const { timeline } = project([
      { kind: "plan", update: { plan: { planId: "p", entries: [step] } } },
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

  it("follows the partition the bridge published and derives none of its own", () => {
    const entries = [user("a"), agent("1"), user("b"), agent("2")];
    const head = (start: number, length: number): TurnHead => ({
      historyRev: 1, start, length, state: "ended", output: null,
    });
    // The bridge says it is one turn: the thread has one, whatever the user messages suggest.
    expect(project(entries, [], [head(0, 4)]).timeline.map((item) => item.historyTurnId))
      .toEqual(["turn:0", "turn:0", "turn:0", "turn:0"]);
    // And a different partition gives a different thread.
    expect(project(entries, [], [head(0, 1), head(1, 3)]).timeline.map((item) => item.historyTurnId))
      .toEqual(["turn:0", "turn:1", "turn:1", "turn:1"]);
    // Entries with no index are one turn.
    expect(projectEntries("S", entries, [], []).timeline.map((item) => item.historyTurnId))
      .toEqual(["turn:0", "turn:0", "turn:0", "turn:0"]);
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

describe("items an update leaves unchanged", () => {
  it("are the same objects while their entries are", () => {
    const entries = [user("hi"), agent("one"), thought("hmm"), tool("t1", { status: "in_progress" })];
    const before = project(entries).timeline;
    // A stream event replaces the entries it changes and appends new ones (§5.3).
    const after = project([...entries.slice(0, 3), tool("t1", { status: "completed" }), agent("two")]).timeline;
    expect(after[0]).toBe(before[0]);
    expect(after[1]).toBe(before[1]);
    expect(after[2]).not.toBe(before[2]);
    expect(after[2]).toMatchObject({ type: "tool", call: { status: "completed" } });
    expect(after[3]).toMatchObject({ type: "assistant", chunks: [{ blocks: [text("two")] }] });
  });

  it("renews an assistant run when it gains a message, keeping its earlier chunks", () => {
    const entries = [user("hi"), agent("one")];
    const [, before] = project(entries).timeline;
    const [, after] = project([...entries, thought("hmm")]).timeline;
    expect(after).not.toBe(before);
    if (before.type !== "assistant" || after.type !== "assistant") throw new Error("expected assistant runs");
    expect(after.chunks[0]).toBe(before.chunks[0]);
    expect(after.chunks).toHaveLength(2);
  });

  it("are made again when the same entry starts another turn", () => {
    const reply = agent("one");
    const [first] = project([reply]).timeline;
    const [, second] = project([user("hi"), reply]).timeline;
    expect(second).not.toBe(first);
    expect(second).toMatchObject({ id: "entry:1", historyTurnId: "turn:0" });
  });
});
