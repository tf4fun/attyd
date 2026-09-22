import { describe, expect, it } from "vitest";
import type { BridgeSessionView } from "../web/src/lib/business-api";
import { appReducer, initialState } from "../web/src/lib/state";
import { releaseSessionViewProcess } from "../web/src/lib/process-retention";

const owner = { bridgeEpoch: "epoch", sessionId: "session", sessionIncarnation: 1 };
function includedView(): BridgeSessionView {
  return {
    ...owner, historyRevision: "history", viewRevision: 3, phase: "ready", syncError: null,
    workspace: { cwd: "/work", session: {} }, controls: {}, operation: null, activeTurn: null,
    interactions: { permissions: {}, elicitations: {}, urlFlows: {} },
    timeline: [
      { sessionUpdate: "user_message_chunk", content: { type: "text", text: "Question" } },
      { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "PRIVATE_THOUGHT" } },
      { sessionUpdate: "tool_call", toolCallId: "tool", title: "Private tool", status: "completed",
        rawOutput: "PRIVATE_OUTPUT", content: [{ type: "terminal", terminalId: "hidden" }] },
      { sessionUpdate: "agent_message_chunk", messageId: "final", content: { type: "text", text: "Final answer" } },
      { sessionUpdate: "agent_message_chunk", messageId: "final", content: { type: "image", data: "aW1n", mimeType: "image/png" } },
    ],
    collapsedTurns: [{ turnId: "turn", operationId: "op", beforeUpdate: 0, afterUpdate: 5,
      visibleRanges: [{ start: 0, end: 1 }, { start: 3, end: 5 }],
      processCount: 2, processIncluded: true, historyRevision: "history",
      outcomes: [{ operationId: "op", afterUpdate: 5, response: { stopReason: "end_turn" } }],
    }],
    turnOutcomes: [{ operationId: "op", afterUpdate: 5, response: { stopReason: "end_turn" } }],
    terminals: {
      hidden: { sessionId: "session", terminalId: "hidden", output: "PRIVATE_TERMINAL", outputBytes: "YWJj", truncated: false, released: true },
      live: { sessionId: "session", terminalId: "live", output: "LIVE", truncated: false, released: false },
    },
  };
}

describe("completed process memory release", () => {
  it("drops the canonical process and finished terminal bodies while preserving prompt, multimodal final, outcome and page identity", () => {
    const view = includedView();
    const state = appReducer(initialState, { type: "bridge/session_hydrate", view });
    const process = state.timeline.find((item) => item.retainedProcess)?.retainedProcess;
    expect(process).toMatchObject({ turnId: "turn", operationId: "op", processCount: 2, owner });
    const next = appReducer(state, { type: "session/release_turn_process", process: process! });
    expect(JSON.stringify(next)).not.toContain("PRIVATE_");
    expect(next.timeline.find((item) => item.deferredProcess)?.deferredProcess).toEqual(process);
    expect(next.timeline.find((item) => item.type === "assistant")).toMatchObject({ chunks: [{ blocks: [
      { type: "text", text: "Final answer" }, { type: "image", data: "aW1n" },
    ] }] });
    expect(next.timeline.at(-1)).toMatchObject({ type: "stop", response: { stopReason: "end_turn" } });
    expect(next.terminalSnapshots.map((terminal) => terminal.terminalId)).toEqual(["live"]);
    expect(JSON.stringify(state)).toContain("PRIVATE_OUTPUT");
  });

  it("compacts the raw session mirror using authoritative ranges and remaps every affected boundary", () => {
    const view = includedView();
    const next = releaseSessionViewProcess(view, () => true);
    expect(next.timeline).toEqual([view.timeline[0], view.timeline[3], view.timeline[4]]);
    expect(next.collapsedTurns?.[0]).toMatchObject({ beforeUpdate: 0, afterUpdate: 3, processIncluded: false,
      visibleRanges: [{ start: 0, end: 1 }, { start: 1, end: 3 }], outcomes: [{ afterUpdate: 3 }] });
    expect(next.turnOutcomes?.[0].afterUpdate).toBe(3);
    expect(JSON.stringify(next)).not.toContain("PRIVATE_");
    expect(next.terminals.live).toBe(view.terminals.live);
  });

  it("rejects stale owner/history release and leaves the active turn and shared terminal intact", () => {
    const view = includedView();
    view.phase = "running";
    view.activeTurn = { operationId: "active", clientIntentId: "active", prompt: [], terminal: null, updates: [
      { sessionUpdate: "tool_call", toolCallId: "active-tool", title: "Running", content: [{ type: "terminal", terminalId: "hidden" }] },
    ] };
    const state = appReducer(initialState, { type: "bridge/session_hydrate", view });
    const process = state.timeline.find((item) => item.retainedProcess)!.retainedProcess!;
    expect(appReducer(state, { type: "session/release_turn_process", process: { ...process, historyRevision: "old" } })).toBe(state);
    expect(appReducer(state, { type: "session/release_turn_process", process: { ...process, owner: { ...owner, sessionIncarnation: 2 } } })).toBe(state);
    const next = appReducer(state, { type: "session/release_turn_process", process });
    expect(next.running).toBe(true);
    expect(next.timeline.some((item) => item.type === "tool" && item.call.toolCallId === "active-tool")).toBe(true);
    expect(next.terminalSnapshots).toHaveLength(2);
    expect(releaseSessionViewProcess(view, () => true).activeTurn).toBe(view.activeTurn);
    expect(releaseSessionViewProcess(view, () => true).terminals.hidden).toBe(view.terminals.hidden);
  });

  it("releases turns independently and rebases the remaining ranges for later eviction", () => {
    const first = includedView();
    const second = first.collapsedTurns![0];
    const view: BridgeSessionView = { ...first, timeline: [...first.timeline, ...first.timeline], collapsedTurns: [second, {
      ...second, turnId: "second", operationId: "op-second", beforeUpdate: 5, afterUpdate: 10,
      visibleRanges: [{ start: 5, end: 6 }, { start: 8, end: 10 }],
      outcomes: [{ operationId: "op-second", afterUpdate: 10, response: { stopReason: "end_turn" } }],
    }] };
    const releasedFirst = releaseSessionViewProcess(view, (turn) => turn.operationId === "op");
    expect(releasedFirst.timeline).toHaveLength(8);
    expect(releasedFirst.collapsedTurns?.[1]).toMatchObject({ beforeUpdate: 3, afterUpdate: 8, processIncluded: true,
      visibleRanges: [{ start: 3, end: 4 }, { start: 6, end: 8 }] });
    expect(releasedFirst.terminals.hidden).toBe(first.terminals.hidden);
    const releasedBoth = releaseSessionViewProcess(releasedFirst, () => true);
    expect(releasedBoth.timeline).toHaveLength(6);
    expect(releasedBoth.turnOutcomes?.map((outcome) => outcome.afterUpdate)).toEqual([3, 6]);
    expect(JSON.stringify(releasedBoth)).not.toContain("PRIVATE_");
  });

  it("keeps full raw history when no authoritative visible ranges are available", () => {
    const view = includedView();
    view.collapsedTurns![0].visibleRanges = undefined;
    expect(releaseSessionViewProcess(view, () => true)).toBe(view);
  });

  it("releases reconstructible completed contents when caching a departed session", () => {
    const state = appReducer(initialState, { type: "bridge/session_hydrate", view: includedView() });
    const next = appReducer(state, { type: "session/deselect" });
    const cached = next.cachedSessions.get("session");
    expect(cached?.timeline.some((item) => item.deferredProcess)).toBe(true);
    expect(JSON.stringify(cached)).not.toContain("PRIVATE_");
  });
});
