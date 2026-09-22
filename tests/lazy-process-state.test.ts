import type { SessionUpdate } from "@agentclientprotocol/sdk";
import { describe, expect, it } from "vitest";
import type { BridgeSessionView, BridgeTurnProcessPage } from "../web/src/lib/business-api";
import { appReducer, initialState, processPageTimeline } from "../web/src/lib/state";
import { collectTurnReviewChanges } from "../web/src/lib/review-changes";
import { splitTurnPresentation } from "../web/src/lib/turn-presentation";

const owner = { bridgeEpoch: "epoch", sessionId: "session", sessionIncarnation: 4 };
const user = (text: string): SessionUpdate => ({ sessionUpdate: "user_message_chunk", content: { type: "text", text } });
const answer = (text: string): SessionUpdate => ({ sessionUpdate: "agent_message_chunk", messageId: "reused-answer", content: { type: "text", text } });
function view(): BridgeSessionView {
  return {
    ...owner, viewRevision: 7, historyRevision: "history", phase: "ready", syncError: null,
    timeline: [user("first"), answer("first answer"), user("second"), answer("second answer")],
    collapsedTurns: [
      { turnId: "0", beforeUpdate: 0, afterUpdate: 2, processCount: 25, historyRevision: "history", outcomes: [] },
      { turnId: "30", beforeUpdate: 2, afterUpdate: 4, processCount: 2, historyRevision: "history", outcomes: [
        { operationId: "done", afterUpdate: 4, response: { stopReason: "cancelled" } },
      ] },
    ],
    activeTurn: null, workspace: { cwd: "/work", session: {} }, controls: {},
    interactions: { permissions: {}, elicitations: {}, urlFlows: {} }, operation: null, terminals: {},
  };
}

describe("compact completed-turn hydration", () => {
  it.each([false, true])("preserves a fully observed turn's identities while applying included authoritative content (outcome arrived: %s)", (completed) => {
    const running = view();
    running.timeline = [];
    running.collapsedTurns = [];
    running.phase = "running";
    running.activeTurn = {
      operationId: "observed", clientIntentId: "intent", prompt: [{ type: "text", text: "observed prompt" }], terminal: null,
      updates: [
        { sessionUpdate: "agent_message_chunk", messageId: "progress", content: { type: "text", text: "visible progress" } },
        { sessionUpdate: "tool_call", toolCallId: "observed-tool", title: "Observed tool", status: "completed", rawOutput: "before reconciliation" },
        answer("visible final"),
      ],
    };
    let observed = appReducer(initialState, { type: "bridge/session_hydrate", view: running });
    if (completed) observed = appReducer(observed, { type: "bridge/turn_complete", event: {
      type: "bridge/session_turn_complete", ...owner, viewRevision: 8, historyRevision: "next", phase: "ready",
      operationId: "observed", clientIntentId: "intent", response: { stopReason: "end_turn" },
    } });
    const authoritative = view();
    authoritative.timeline = [user("observed prompt"), ...running.activeTurn.updates];
    authoritative.timeline[2] = { sessionUpdate: "tool_call", toolCallId: "observed-tool", title: "Observed tool", status: "completed", rawOutput: "authoritative output" };
    authoritative.collapsedTurns = [{ turnId: "observed-history", beforeUpdate: 0, afterUpdate: 4, processCount: 2,
      processIncluded: true, historyRevision: "next", outcomes: [{ operationId: "observed", afterUpdate: 4, response: { stopReason: "end_turn" } }] }];
    const hydrated = appReducer(observed, { type: "bridge/session_hydrate", view: authoritative });
    expect(hydrated.timeline.filter((item) => item.type !== "stop").map((item) => item.id))
      .toEqual(observed.timeline.filter((item) => item.type !== "stop").map((item) => item.id));
    expect(hydrated.timeline.filter((item) => item.type === "assistant").map((item) => item.chunks.map((chunk) => chunk.id)))
      .toEqual(observed.timeline.filter((item) => item.type === "assistant").map((item) => item.chunks.map((chunk) => chunk.id)));
    expect(hydrated.timeline.every((item) => item.historyTurnId === "observed-history" && item.deferredProcess == null)).toBe(true);
    expect(hydrated.timeline.find((item) => item.type === "tool")).toMatchObject({ call: { rawOutput: "authoritative output" } });
    const replacementOwner = appReducer(hydrated, { type: "bridge/session_hydrate", view: {
      ...authoritative, sessionIncarnation: owner.sessionIncarnation + 1,
    } });
    expect(replacementOwner.timeline[0].id).not.toBe(hydrated.timeline[0].id);

    const reset = view();
    reset.timeline = [user("replacement prompt"), answer("replacement answer")];
    reset.collapsedTurns = [{ turnId: "observed-history", beforeUpdate: 0, afterUpdate: 2, processCount: 4, historyRevision: "replaced", outcomes: [] }];
    const replaced = appReducer(hydrated, { type: "bridge/session_hydrate", view: reset });
    expect(replaced.timeline.some((item) => item.type === "tool")).toBe(false);
    expect(replaced.timeline[0].deferredProcess).toMatchObject({ historyRevision: "replaced", processCount: 4 });
  });

  it("uses included authoritative bodies when the previously loaded turn was sparse", () => {
    const sparse = view();
    const before = appReducer(initialState, { type: "bridge/session_hydrate", view: sparse });
    sparse.timeline = [user("first"), { sessionUpdate: "tool_call", toolCallId: "loaded", title: "New authoritative details", status: "completed" }, answer("first answer")];
    sparse.collapsedTurns = [{ ...sparse.collapsedTurns![0], afterUpdate: 3, processCount: 1, processIncluded: true }];
    const after = appReducer(before, { type: "bridge/session_hydrate", view: sparse });
    expect(after.timeline.some((item) => item.deferredProcess)).toBe(false);
    expect(after.timeline.find((item) => item.type === "tool")).toMatchObject({ call: { title: "New authoritative details" } });
  });

  it("isolates reused tool, identified plan, and compaction IDs from included history", () => {
    const updates: SessionUpdate[] = [
      { sessionUpdate: "tool_call", toolCallId: "shared", title: "Historical tool", status: "completed", rawInput: "historical input" },
      { sessionUpdate: "plan_update", plan: { planId: "shared", type: "markdown", content: "Historical plan" } },
      { sessionUpdate: "compaction_update", compactionId: "shared", status: "completed", summary: [{ type: "text", text: "Historical summary" }] },
      answer("Historical answer"),
    ];
    const running = { ...view(), timeline: [], collapsedTurns: [], phase: "running" as const,
      activeTurn: { operationId: "observed", clientIntentId: "observed", prompt: [{ type: "text" as const, text: "Old prompt" }], updates, terminal: null } };
    let state = appReducer(initialState, { type: "bridge/session_hydrate", view: running });
    state = appReducer(state, { type: "bridge/session_hydrate", view: {
      ...view(), timeline: [user("Old prompt"), ...updates],
      collapsedTurns: [{ turnId: "included", beforeUpdate: 0, afterUpdate: 5, processCount: 3, processIncluded: true, historyRevision: "history",
        outcomes: [{ operationId: "observed", afterUpdate: 5, response: { stopReason: "end_turn" } }] }],
    } });
    const historical = state.timeline;
    state = appReducer(state, { type: "user/prompt", sessionId: owner.sessionId, requestId: "next", blocks: [{ type: "text", text: "New prompt" }] });
    const next: SessionUpdate[] = [
      { sessionUpdate: "tool_call_update", toolCallId: "shared", status: "completed", rawOutput: "unintroduced live output" },
      { sessionUpdate: "plan_update", plan: { planId: "shared", type: "markdown", content: "Live plan" } },
      { sessionUpdate: "plan_removed", planId: "shared" },
      { sessionUpdate: "compaction_summary_chunk", compactionId: "shared", content: { type: "text", text: "Live summary" } },
      { sessionUpdate: "compaction_update", compactionId: "shared", status: "completed" },
    ];
    for (const update of next) state = appReducer(state, { type: "server/event", event: {
      type: "acp/session_update", notification: { sessionId: owner.sessionId, update },
    } });
    expect(state.timeline.filter((item) => item.historyTurnId != null)).toEqual(historical);
    const current = state.timeline.filter((item) => item.historyTurnId == null);
    expect(current.find((item) => item.type === "tool")).toMatchObject({ call: { title: "Tool call not found", status: "failed" } });
    expect(current.find((item) => item.type === "plan")).toMatchObject({ update: { sessionUpdate: "plan_removed", planId: "shared" } });
    expect(current.find((item) => item.type === "compaction")).toMatchObject({ status: "completed", blocks: [{ type: "text", text: "Live summary" }] });
    expect(new Set(state.timeline.map((item) => item.id)).size).toBe(state.timeline.length);
    expect(collectTurnReviewChanges(state.timeline)).toHaveLength(2);
  });

  it("keeps boundaries and outcomes without merging reused message IDs or retaining hidden process", () => {
    const state = appReducer(initialState, { type: "bridge/session_hydrate", view: view() });
    const turns = collectTurnReviewChanges(state.timeline);
    expect(turns).toHaveLength(2);
    expect(turns.map(({ items }) => splitTurnPresentation(items).output?.chunks[0].blocks))
      .toEqual([[{ type: "text", text: "first answer" }], [{ type: "text", text: "second answer" }]]);
    expect(turns.map(({ items }) => items[0].deferredProcess)).toEqual([
      { owner, turnId: "0", historyRevision: "history", processCount: 25 },
      { owner, turnId: "30", operationId: "done", historyRevision: "history", processCount: 2 },
    ]);
    expect(turns[1].items.at(-1)).toMatchObject({ id: "bridge-turn-outcome:done", type: "stop", response: { stopReason: "cancelled" } });
    expect(state.timeline.some((item) => item.type === "tool")).toBe(false);
  });

  it("represents process-only and empty-output turns at identical compact offsets", () => {
    const compact = view();
    compact.timeline = [answer("answer without a user prompt")];
    compact.collapsedTurns = [
      { turnId: "0", beforeUpdate: 0, afterUpdate: 0, processCount: 12, historyRevision: "history", outcomes: [] },
      { turnId: "12", beforeUpdate: 0, afterUpdate: 1, processCount: 0, historyRevision: "history", outcomes: [] },
    ];
    const state = appReducer(initialState, { type: "bridge/session_hydrate", view: compact });
    const turns = collectTurnReviewChanges(state.timeline);
    expect(turns).toHaveLength(2);
    expect(turns[0].items[0].deferredProcess?.processCount).toBe(12);
    expect(splitTurnPresentation(turns[0].items).output).toBeUndefined();
    expect(splitTurnPresentation(turns[1].items).output?.chunks[0].blocks).toEqual([{ type: "text", text: "answer without a user prompt" }]);
  });

  it("keeps multimodal final output complete and a running turn fully reducible", () => {
    const compact = view();
    compact.timeline.splice(2, 0, {
      sessionUpdate: "agent_message_chunk", messageId: "reused-answer",
      content: { type: "image", data: "aW1hZ2U=", mimeType: "image/png" },
    });
    compact.collapsedTurns![0].afterUpdate = 3;
    compact.collapsedTurns![1].beforeUpdate = 3;
    compact.collapsedTurns![1].afterUpdate = 5;
    compact.phase = "running";
    compact.activeTurn = {
      operationId: "running", clientIntentId: "running", prompt: [{ type: "text", text: "new turn" }], terminal: null,
      updates: [{ sessionUpdate: "tool_call", toolCallId: "live", title: "Live tool", status: "in_progress" }],
    };
    let state = appReducer(initialState, { type: "bridge/session_hydrate", view: compact });
    state = appReducer(state, { type: "server/event", event: { type: "acp/session_update", notification: {
      sessionId: owner.sessionId, update: { sessionUpdate: "tool_call_update", toolCallId: "live", status: "completed", rawOutput: "complete output" },
    } } });
    const turns = collectTurnReviewChanges(state.timeline);
    expect(turns).toHaveLength(3);
    expect(splitTurnPresentation(turns[0].items).output?.chunks[0].blocks).toHaveLength(2);
    expect(turns[2].items.find((item) => item.type === "tool")).toMatchObject({ call: { title: "Live tool", rawOutput: "complete output", status: "completed" } });
    expect(turns[2].items.every((item) => item.deferredProcess == null)).toBe(true);
    expect(state.running).toBe(true);
  });

  it.each(["snapshot", "stream"] as const)("keeps reused user and assistant message IDs inside the live turn during %s updates", (source) => {
    const compact = view();
    compact.timeline = compact.timeline.map((update): SessionUpdate => update.sessionUpdate === "user_message_chunk"
      ? { ...update, messageId: "reused-user" } : update);
    const historical = appReducer(initialState, { type: "bridge/session_hydrate", view: compact }).timeline;
    const updates: SessionUpdate[] = [
      { sessionUpdate: "user_message_chunk", messageId: "reused-user", content: { type: "text", text: "new prompt" } },
      answer("live answer"),
      answer(" continues"),
    ];
    compact.phase = "running";
    compact.activeTurn = { operationId: "running", clientIntentId: "running", prompt: [{ type: "text", text: "new prompt" }],
      updates: source === "snapshot" ? updates : [], terminal: null };
    let state = appReducer(initialState, { type: "bridge/session_hydrate", view: compact });
    if (source === "stream") for (const update of updates) state = appReducer(state, {
      type: "server/event", event: { type: "acp/session_update", notification: { sessionId: owner.sessionId, update } },
    });
    expect(state.timeline.filter((item) => item.historyTurnId != null).map((item) =>
      item.type === "assistant" ? item.chunks.map((chunk) => chunk.blocks) : item.type === "message" ? item.blocks : item.type
    )).toEqual(historical.map((item) =>
      item.type === "assistant" ? item.chunks.map((chunk) => chunk.blocks) : item.type === "message" ? item.blocks : item.type
    ));
    const turns = collectTurnReviewChanges(state.timeline);
    expect(turns).toHaveLength(3);
    const live = splitTurnPresentation(turns[2].items);
    expect(live.prompts).toHaveLength(1);
    expect(live.prompts[0]).toMatchObject({ role: "user", messageId: "reused-user", blocks: [{ type: "text", text: "new prompt" }] });
    expect(live.output?.chunks).toHaveLength(1);
    expect(live.output?.chunks[0].blocks).toEqual([{ type: "text", text: "live answer continues" }]);
    expect(state.running).toBe(true);
  });

  it.each(["user", "assistant"] as const)("does not merge an anonymous %s update into the last compact history entry", (kind) => {
    const compact = view();
    compact.timeline = [kind === "user" ? user("historical prompt") : { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "historical answer" } }];
    compact.collapsedTurns = [{ turnId: "0", beforeUpdate: 0, afterUpdate: 1, processCount: 0, historyRevision: "history", outcomes: [] }];
    let state = appReducer(initialState, { type: "bridge/session_hydrate", view: compact });
    const historical = state.timeline[0];
    state = appReducer(state, { type: "server/event", event: { type: "acp/session_update", notification: {
      sessionId: owner.sessionId,
      update: kind === "user" ? user("new prompt") : { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "new answer" } },
    } } });
    expect(state.timeline).toHaveLength(2);
    expect(state.timeline[0]).toBe(historical);
    expect(state.timeline[1].historyTurnId).toBeUndefined();
    expect(collectTurnReviewChanges(state.timeline)).toHaveLength(2);
  });

  it("decodes page groups independently with stable indices and cancelled tool presentation", () => {
    const page: BridgeTurnProcessPage = {
      ...owner, turnId: "0", historyRevision: "history", offset: 10, total: 12, nextOffset: null, terminals: {},
      response: { stopReason: "cancelled" }, items: [
        [{ sessionUpdate: "tool_call", toolCallId: "tool", title: "Read", status: "in_progress", rawInput: { path: "/work" } },
          { sessionUpdate: "tool_call_update", toolCallId: "tool", rawOutput: "current output" }],
        [answer("part one"), answer("part two")],
      ],
    };
    const timeline = processPageTimeline(page);
    expect(timeline).toHaveLength(2);
    expect(timeline[0]).toMatchObject({ id: "process:0:10:0", type: "tool", cancelled: true, call: { rawInput: { path: "/work" }, rawOutput: "current output" } });
    expect(timeline[1]).toMatchObject({ id: "process:0:11:0", type: "assistant", chunks: [{ blocks: [{ type: "text", text: "part onepart two" }] }] });
    expect(processPageTimeline(page).map(({ id }) => id)).toEqual(timeline.map(({ id }) => id));
  });
});
