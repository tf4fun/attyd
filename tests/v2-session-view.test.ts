import type { SessionUpdate } from "@agentclientprotocol/sdk";
import { describe, expect, it } from "vitest";
import type { SessionState } from "../shared/protocol";
import { ObservedProcesses, bridgeSessionView, turnUpdateDelta } from "../web/src/lib/v2-session-view";

function sessionState(overrides: Partial<SessionState> = {}): SessionState {
  return {
    sessionId: "S",
    incarnation: 2,
    cwd: "/work",
    controls: { modes: null, configOptions: null, commands: [], info: { title: null, updatedAt: null, meta: null }, usage: null },
    history: { revision: "r1", phase: "ready", error: null, notice: null, timeline: [], collapsedTurns: [] },
    turn: null,
    outcomes: {},
    operation: null,
    interactions: {},
    urlFlows: {},
    terminals: {},
    ...overrides,
  };
}

const user = (text: string): SessionUpdate => ({ sessionUpdate: "user_message_chunk", content: { type: "text", text } });
const agent = (text: string): SessionUpdate => ({ sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
const tool = (toolCallId: string, title: string): SessionUpdate => ({ sessionUpdate: "tool_call", toolCallId, title });

describe("bridgeSessionView", () => {
  it("maps the owner, history, interactions, flows, terminals and operation", () => {
    const view = bridgeSessionView(sessionState({
      history: { revision: "r1", phase: "running", error: "boom", notice: "note", timeline: [agent("a")], collapsedTurns: [] },
      interactions: {
        P: { interactionId: "P", kind: "permission", operationId: "op", request: { sessionId: "S", toolCall: { toolCallId: "t" }, options: [] } },
        E: { interactionId: "E", kind: "elicitation", operationId: null, request: { sessionId: "S", mode: "form", message: "m", requestedSchema: { type: "object", properties: {} } } },
      },
      urlFlows: {
        I: { interactionId: "I", elicitationId: "el", request: { sessionId: "S", mode: "url", message: "open", url: "https://x", elicitationId: "el" }, status: "completed" },
      },
      terminals: { T: { terminalId: "T", output: "out", truncated: false, exitStatus: null, released: false } },
      operation: { operationId: "o1", kind: "set_mode", stage: "running", requestId: "R" },
    }), "e1", 42);
    expect(view).toMatchObject({
      bridgeEpoch: "e1", sessionId: "S", sessionIncarnation: 2, viewRevision: 42,
      historyRevision: "r1", phase: "running", syncError: "boom", historyNotice: "note",
      timeline: [agent("a")], activeTurn: null,
      workspace: { cwd: "/work", session: {} },
      operation: { operationId: "o1", kind: "set_mode", stage: "running" },
      terminals: { T: { terminalId: "T", sessionId: "S", output: "out" } },
    });
    expect(Object.keys(view.interactions.permissions)).toEqual(["P"]);
    expect(view.interactions.permissions.P.operationId).toBe("op");
    expect(Object.keys(view.interactions.elicitations)).toEqual(["E"]);
    expect(view.interactions.urlFlows.I).toMatchObject({ interactionId: "I", elicitationId: "el", status: "completed" });
  });

  it("projects reduced controls as the updates the reducer folds", () => {
    const view = bridgeSessionView(sessionState({ controls: {
      modes: { currentModeId: "ask", availableModes: [{ id: "ask", name: "Ask" }] },
      configOptions: [],
      commands: [{ name: "init", description: "Init" }],
      info: { title: "Title", updatedAt: "2026-10-13T00:00:00Z", meta: null },
      usage: { used: 5, size: 10, cost: null },
    } }), "e1", 1);
    expect(view.workspace.session).toEqual({
      modes: { currentModeId: "ask", availableModes: [{ id: "ask", name: "Ask" }] },
      configOptions: [],
    });
    expect(view.controls.available_commands_update).toEqual({
      sessionUpdate: "available_commands_update", availableCommands: [{ name: "init", description: "Init" }],
    });
    expect(view.controls.session_info_update).toEqual({
      sessionUpdate: "session_info_update", title: "Title", updatedAt: "2026-10-13T00:00:00Z",
    });
    expect(view.controls.usage_update).toEqual({ sessionUpdate: "usage_update", used: 5, size: 10 });
  });

  it("attaches completed outcomes to their collapsed turns", () => {
    const view = bridgeSessionView(sessionState({
      history: { revision: "r2", phase: "ready", error: null, notice: null, timeline: [user("q"), agent("a")], collapsedTurns: [
        { turnId: "t1", operationId: "op1", beforeUpdate: 0, afterUpdate: 2, processCount: 3, visibleRanges: [{ start: 0, end: 2 }] },
      ] },
      outcomes: {
        op1: { operationId: "op1", clientIntentId: "k1", status: "completed", response: { stopReason: "end_turn" }, afterUpdate: 2 },
        op0: { operationId: "op0", clientIntentId: "k0", status: "failed", error: { code: "agent_error", message: "x" }, afterUpdate: 0 },
      },
    }), "e1", 3);
    expect(view.collapsedTurns).toEqual([{
      turnId: "t1", operationId: "op1", visibleRanges: [{ start: 0, end: 2 }], beforeUpdate: 0, afterUpdate: 2,
      processCount: 3, processIncluded: false, historyRevision: "r2",
      outcomes: [{ operationId: "op1", afterUpdate: 2, response: { stopReason: "end_turn" } }],
    }]);
    expect(view.turnOutcomes).toEqual(view.collapsedTurns![0].outcomes);
  });

  it("keeps a watched turn's process inline after it commits, across later commits, until released", () => {
    const processes = new ObservedProcesses();
    const running = sessionState({
      history: { revision: "r1", phase: "running", error: null, notice: null, timeline: [], collapsedTurns: [] },
      turn: { operationId: "op1", clientIntentId: "k1", prompt: [{ type: "text", text: "q" }], updates: [tool("c1", "read"), agent("answer")], cancelRequested: false },
    });
    processes.observe(undefined, running);
    const committed = sessionState({
      history: { revision: "r2", phase: "ready", error: null, notice: null, timeline: [user("q"), agent("answer")], collapsedTurns: [
        { turnId: "t1", operationId: "op1", beforeUpdate: 0, afterUpdate: 2, processCount: 1, visibleRanges: [{ start: 0, end: 1 }, { start: 1, end: 2 }] },
      ] },
    });
    processes.observe(running, committed);
    let view = bridgeSessionView(committed, "e1", 5, processes);
    expect(view.timeline).toEqual([user("q"), tool("c1", "read"), agent("answer")]);
    expect(view.collapsedTurns![0]).toMatchObject({
      beforeUpdate: 0, afterUpdate: 3, processIncluded: true, visibleRanges: [{ start: 0, end: 1 }, { start: 2, end: 3 }],
    });

    // A later turn this client did not watch stays compact, after the shifted inline one.
    const later = sessionState({
      history: { revision: "r3", phase: "ready", error: null, notice: null, timeline: [user("q"), agent("answer"), user("q2"), agent("a2")], collapsedTurns: [
        { turnId: "t1", operationId: "op1", beforeUpdate: 0, afterUpdate: 2, processCount: 1, visibleRanges: [{ start: 0, end: 1 }, { start: 1, end: 2 }] },
        { turnId: "t2", operationId: "op2", beforeUpdate: 2, afterUpdate: 4, processCount: 4, visibleRanges: [{ start: 2, end: 3 }, { start: 3, end: 4 }] },
      ] },
    });
    processes.observe(committed, later);
    view = bridgeSessionView(later, "e1", 9, processes);
    expect(view.timeline).toEqual([user("q"), tool("c1", "read"), agent("answer"), user("q2"), agent("a2")]);
    expect(view.collapsedTurns![1]).toMatchObject({
      beforeUpdate: 3, afterUpdate: 5, processIncluded: false, visibleRanges: [{ start: 3, end: 4 }, { start: 4, end: 5 }],
    });

    expect(processes.release("r3", "t1")).toBe(true);
    view = bridgeSessionView(later, "e1", 9, processes);
    expect(view.timeline).toBe(later.history.timeline);
    expect(view.collapsedTurns![0].processIncluded).toBe(false);
  });
});

describe("watched processes", () => {
  it("takes the final answer from history when the last watched overlay lacked it", () => {
    const processes = new ObservedProcesses();
    // The final chunk and the commit arrived in one entry.
    const running = sessionState({
      turn: { operationId: "op1", clientIntentId: "k1", prompt: [{ type: "text", text: "q" }], updates: [tool("c1", "read")], cancelRequested: false },
    });
    const committed = sessionState({
      history: { revision: "r2", phase: "ready", error: null, notice: null, timeline: [user("q"), agent("final")], collapsedTurns: [
        { turnId: "t1", operationId: "op1", beforeUpdate: 0, afterUpdate: 2, processCount: 1, visibleRanges: [{ start: 0, end: 1 }, { start: 1, end: 2 }] },
      ] },
    });
    processes.observe(undefined, running);
    processes.observe(running, committed);
    const view = bridgeSessionView(committed, "e1", 2, processes);
    expect(view.timeline).toEqual([user("q"), tool("c1", "read"), agent("final")]);
    expect(view.collapsedTurns![0]).toMatchObject({ processIncluded: true, visibleRanges: [{ start: 0, end: 1 }, { start: 2, end: 3 }] });
  });

  it("falls back to the page when the watched overlay misses process entries", () => {
    const processes = new ObservedProcesses();
    const running = sessionState({
      turn: { operationId: "op1", clientIntentId: "k1", prompt: [], updates: [], cancelRequested: false },
    });
    const committed = sessionState({
      history: { revision: "r2", phase: "ready", error: null, notice: null, timeline: [user("q"), agent("final")], collapsedTurns: [
        { turnId: "t1", operationId: "op1", beforeUpdate: 0, afterUpdate: 2, processCount: 2, visibleRanges: [{ start: 0, end: 2 }] },
      ] },
    });
    processes.observe(undefined, running);
    processes.observe(running, committed);
    const view = bridgeSessionView(committed, "e1", 2, processes);
    expect(view.timeline).toBe(committed.history.timeline);
    expect(view.collapsedTurns![0].processIncluded).toBe(false);
  });
});

describe("turnUpdateDelta", () => {
  it("passes appended updates through", () => {
    const first = agent("a");
    expect(turnUpdateDelta([first], [first, tool("c", "x")])).toEqual([tool("c", "x")]);
  });

  it("turns a merged text chunk into its suffix", () => {
    const fixed = tool("c", "x");
    expect(turnUpdateDelta([fixed, agent("Hel")], [fixed, agent("Hello")])).toEqual([agent("lo")]);
  });

  it("replays a folded tool call or a replaced plan as the folded entry", () => {
    const tail = agent("t");
    const folded: SessionUpdate = { sessionUpdate: "tool_call", toolCallId: "c", title: "y", status: "completed" };
    expect(turnUpdateDelta([tool("c", "x"), tail], [folded, tail])).toEqual([folded]);
    const plan: SessionUpdate = { sessionUpdate: "plan", entries: [] };
    const replaced: SessionUpdate = { sessionUpdate: "plan", entries: [{ content: "step", priority: "high", status: "pending" }] };
    expect(turnUpdateDelta([plan], [replaced])).toEqual([replaced]);
  });

  it("expresses a removed plan as plan_removed", () => {
    const plan: SessionUpdate = { sessionUpdate: "plan_update", plan: { type: "markdown", planId: "p", content: "x" } } as SessionUpdate;
    const tail = agent("t");
    expect(turnUpdateDelta([plan, tail], [tail])).toEqual([{ sessionUpdate: "plan_removed", planId: "p" }]);
  });

  it("gives up on changes the reducer cannot replay", () => {
    expect(turnUpdateDelta([agent("abc")], [agent("xyz")])).toBeUndefined();
    expect(turnUpdateDelta([agent("a"), agent("b")], [agent("ab")])).toBeUndefined();
    expect(turnUpdateDelta([agent("a"), tool("c", "x")], [agent("ab"), tool("c", "x")])).toBeUndefined();
  });
});
