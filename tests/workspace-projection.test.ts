import { describe, expect, it } from "vitest";
import type { GlobalLive, LogEntry, Operation, ProcessUpdate, SessionSummary } from "../shared/protocol";
import { timelineTurnStarts } from "../web/src/lib/timeline-turns";
import {
  listedSessions,
  projectProcess,
  projectTimeline,
  standaloneTerminals,
} from "../web/src/lib/workspace-projection";

const entry = (prompt: string, reply: string, hash: string): LogEntry => ({
  turn: { prompt: [{ type: "text", text: prompt }], reply: [{ type: "text", text: reply }] },
  hash,
});

function operation(fields: Partial<Operation>): Operation {
  return {
    id: "op-1", kind: "prompt", status: "running", input: { prompt: [{ type: "text", text: "A" }] },
    cancelRequested: false, interactions: {}, ...fields,
  };
}

function global(sessions: Record<string, SessionSummary>): GlobalLive {
  return { sessions } as unknown as GlobalLive;
}

const summary = (title: string | null): SessionSummary => ({
  sessionId: "s", cwd: "/w", title, op: { kind: "open", status: "completed" }, pending: { interactions: 0, urlFlows: 0 },
});

describe("projected thread", () => {
  it("keeps a completed prompt's result in the turn the commit appended", () => {
    const timeline = projectTimeline("s", [entry("first", "one", "h1"), entry("second", "two", "h2")], operation({
      status: "completed", result: { stopReason: "max_tokens" },
    }));
    expect(timeline.map((item) => item.type)).toEqual(["message", "assistant", "message", "assistant", "stop"]);
    const starts = timelineTurnStarts(timeline).filter((start) => start < timeline.length);
    expect(starts).toEqual([0, 2]);
    // The committed prompt keeps the running turn's operation identity.
    expect(timeline[2]).toMatchObject({ type: "message", turnOperationId: "op-1" });
  });

  it("keeps the whole reply of a committed turn in one Agent entry", () => {
    const reply: LogEntry = {
      turn: { prompt: [{ type: "text", text: "p" }], reply: [{ type: "text", text: "Paragraph 1.\n\nParagraph 2.\n\nFinal." }] },
      hash: "h",
    };
    const [, agent] = projectTimeline("s", [reply], undefined);
    expect(agent).toMatchObject({ type: "assistant", chunks: [{ role: "agent", blocks: reply.turn.reply }] });
  });

  it("shows a running prompt's input and process, and a failed one's error without a log entry", () => {
    const process: ProcessUpdate[] = [
      { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "Thinking." } },
      { sessionUpdate: "tool_call", toolCallId: "t", title: "Read", status: "in_progress" },
    ];
    const running = projectTimeline("s", [], operation({ process }));
    expect(running.map((item) => item.type)).toEqual(["message", "assistant", "tool"]);
    const failed = projectTimeline("s", [], operation({
      status: "failed",
      error: { code: "agent_error", message: "failed", data: { code: -32603, message: "Boom", data: { owner: "Agent" } } },
    }));
    expect(failed.map((item) => item.type)).toEqual(["message", "error"]);
    expect(failed[1]).toMatchObject({ code: -32603, message: "Boom", retryBlocks: [{ type: "text", text: "A" }] });
  });

  it("drops a user message chunk that echoes the prompt and shows any other", () => {
    const echo: ProcessUpdate = { sessionUpdate: "user_message_chunk", content: { type: "text", text: "A" } };
    const other: ProcessUpdate = { sessionUpdate: "user_message_chunk", content: { type: "text", text: "B" } };
    const prompt = [{ type: "text" as const, text: "A" }];
    expect(projectProcess("s", "op-1", [echo], prompt).items).toEqual([]);
    const shown = projectProcess("s", "op-1", [other], prompt).items;
    expect(shown).toMatchObject([{ type: "message", role: "protocol-user", blocks: [{ type: "text", text: "B" }] }]);
    expect(shown[0]).toMatchObject({ raw: [{ sessionId: "s", update: other }] });
    // It belongs to the running turn: no new turn starts at it.
    const timeline = projectTimeline("s", [], operation({ input: { prompt }, process: [other] }));
    expect(timelineTurnStarts(timeline).filter((start) => start < timeline.length)).toEqual([0]);
  });

  it("shows an unfinished plan above the composer and a finished one in its turn, never an empty card", () => {
    const plan = (status: "in_progress" | "completed", entries = 1): ProcessUpdate => ({
      sessionUpdate: "plan",
      entries: Array.from({ length: entries }, () => ({ content: "Step", priority: "medium" as const, status })),
    });
    const plans = (items: readonly { type: string }[]) => items.filter((item) => item.type === "plan");
    // The bridge folds each plan into the one before it, so every snapshot
    // carries the turn's latest plan only.
    const finished = projectProcess("s", "op-1", [plan("completed")]);
    expect(finished.activePlan).toBeUndefined();
    expect(plans(finished.items)).toMatchObject([{ update: plan("completed") }]);
    const reopened = projectProcess("s", "op-1", [plan("in_progress")]);
    expect(reopened.activePlan).toMatchObject({ update: plan("in_progress") });
    expect(plans(reopened.items)).toEqual([]);
    const empty = projectProcess("s", "op-1", [plan("completed", 0)]);
    expect(empty.activePlan).toBeUndefined();
    expect(plans(empty.items)).toEqual([]);
    expect(plans(projectTimeline("s", [], operation({ process: [plan("completed", 0)] })))).toEqual([]);
  });

  it("keeps a compaction's summary when its later failure omits it", () => {
    const process: ProcessUpdate[] = [
      { sessionUpdate: "compaction_update", compactionId: "c", status: "in_progress", summary: [{ type: "text", text: "Summary" }] },
      { sessionUpdate: "compaction_update", compactionId: "c", status: "failed", error: "compaction failed" },
    ];
    const compactions = projectProcess("s", "op-1", process).items.filter((item) => item.type === "compaction");
    expect(compactions).toHaveLength(1);
    expect(compactions[0]).toMatchObject({
      status: "failed", error: "compaction failed", blocks: [{ type: "text", text: "Summary" }],
    });
  });

  it("shows only terminals no tool card shows", () => {
    const terminals = ["a", "b"].map((terminalId) => ({ sessionId: "s", terminalId, output: "", truncated: false, released: false }));
    const timeline = projectTimeline("s", [], operation({ process: [{
      sessionUpdate: "tool_call", toolCallId: "t", title: "Run", content: [{ type: "terminal", terminalId: "a" }],
    }] }));
    expect(standaloneTerminals(terminals, timeline).map(({ terminalId }) => terminalId)).toEqual(["b"]);
  });
});

describe("listed sessions", () => {
  it("takes a held session's title from its summary, including a cleared one", () => {
    const catalog = [{ sessionId: "s", cwd: "/w", title: "Old" }];
    expect(listedSessions(catalog, undefined)[0]?.title).toBe("Old");
    expect(listedSessions(catalog, global({ s: summary("New") }))[0]?.title).toBe("New");
    expect(listedSessions(catalog, global({ s: summary(null) }))[0]?.title).toBeNull();
  });
});
