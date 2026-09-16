import type { PlanEntry, SessionUpdate } from "@agentclientprotocol/sdk";
import { describe, expect, it } from "vitest";
import type { BridgeSessionView } from "../web/src/lib/business-api";
import { appReducer, initialState, type AppState } from "../web/src/lib/state";

const prompt = [{ type: "text" as const, text: "Do the work" }];
const owner = {
  bridgeEpoch: "epoch", sessionId: "s1", sessionIncarnation: 1,
  viewRevision: 3, historyRevision: "revision", phase: "ready" as const,
};

function plan(status: PlanEntry["status"], content = "Inspect"): SessionUpdate {
  return { sessionUpdate: "plan", entries: [{ content, priority: "high", status }] };
}

function update(state: AppState, update: SessionUpdate): AppState {
  return appReducer(state, {
    type: "server/event",
    event: { type: "acp/session_update", notification: { sessionId: "s1", update } },
  });
}

function start(): AppState {
  return appReducer({ ...initialState, session: { sessionId: "s1" } }, {
    type: "user/prompt", requestId: "intent", sessionId: "s1", blocks: prompt,
  });
}

function view(overrides: Partial<BridgeSessionView> = {}): BridgeSessionView {
  return {
    ...owner, syncError: null, timeline: [], activeTurn: null,
    workspace: { cwd: "/workspace", session: {} }, controls: {},
    interactions: { permissions: {}, elicitations: {}, urlFlows: {} },
    operation: null, terminals: {}, ...overrides,
  };
}

function hydrate(snapshot: BridgeSessionView): AppState {
  return appReducer(initialState, { type: "bridge/session_hydrate", view: snapshot });
}

describe("plan lifecycle", () => {
  it.each(["end_turn", "cancelled"] as const)("archives unfinished plans on %s without changing their status", (stopReason) => {
    const active = update(start(), plan("in_progress"));
    expect(active.activePlan).toBeDefined();
    const completion = {
      type: "bridge/turn_complete" as const,
      event: {
        ...owner, type: "bridge/session_turn_complete" as const,
        operationId: "op", clientIntentId: "intent", response: { stopReason },
      },
    };
    const settled = appReducer(active, completion);
    expect(settled.activePlan).toBeUndefined();
    expect(settled.timeline.map(({ type }) => type)).toEqual(["message", "plan", "stop"]);
    expect(settled.timeline[1]).toMatchObject({ update: plan("in_progress") });
    expect(appReducer(settled, completion)).toBe(settled);
    const next = appReducer(settled, {
      type: "user/prompt", requestId: "next", sessionId: "s1", blocks: prompt,
    });
    expect(next.activePlan).toBeUndefined();
  });

  it("archives the plan when a turn fails", () => {
    const settled = appReducer(update(start(), plan("pending")), {
      type: "bridge/turn_failed",
      event: {
        ...owner, type: "bridge/session_turn_failed", operationId: "op",
        clientIntentId: "intent", prompt, error: { code: -32603, message: "Failed" },
      },
    });
    expect(settled.activePlan).toBeUndefined();
    expect(settled.timeline.map(({ type }) => type)).toEqual(["message", "plan", "error"]);
    expect(settled.timeline[1]).toMatchObject({ update: plan("pending") });
  });

  it("retires completed plans immediately and replaces their snapshot if the agent revises them", () => {
    let state = update(start(), plan("in_progress"));
    const id = state.activePlan!.id;
    state = update(state, plan("completed"));
    expect(state.running).toBe(true);
    expect(state.activePlan).toBeUndefined();
    expect(state.agentActivity).toEqual({ kind: "waiting" });
    expect(state.timeline[1]).toMatchObject({ id, type: "plan", update: plan("completed") });
    state = update(state, plan("completed"));
    expect(state.timeline.filter(({ type }) => type === "plan")).toHaveLength(1);
    state = update(state, plan("in_progress", "Follow up"));
    expect(state.activePlan).toMatchObject({ id, update: plan("in_progress", "Follow up") });
    expect(state.timeline.filter(({ type }) => type === "plan")).toHaveLength(0);
  });

  it("clears empty plans without displaying a 0/0 card", () => {
    const empty: SessionUpdate = { sessionUpdate: "plan", entries: [] };
    for (const status of ["in_progress", "completed"] as const) {
      const state = update(update(start(), plan(status)), empty);
      expect(state.activePlan).toBeUndefined();
      expect(state.timeline.filter(({ type }) => type === "plan")).toHaveLength(0);
    }
    expect(hydrate(view({ timeline: [empty] })).timeline).toHaveLength(0);
  });

  it.each([true, false])("keeps every historical plan in its own turn (outcomes: %s)", (withOutcomes) => {
    const state = hydrate(view({
      timeline: [
        { sessionUpdate: "user_message_chunk", content: { type: "text", text: "First" } },
        plan("completed", "First plan"),
        { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Done" } },
        { sessionUpdate: "user_message_chunk", content: { type: "text", text: "Second" } },
        plan("in_progress", "Second plan"),
      ],
      turnOutcomes: withOutcomes ? [
        { operationId: "first", afterUpdate: 3, response: { stopReason: "end_turn" } },
        { operationId: "second", afterUpdate: 5, response: { stopReason: "cancelled" } },
      ] : [],
    }));
    expect(state.activePlan).toBeUndefined();
    expect(state.timeline.filter(({ type }) => type === "plan").map((item) => item.update))
      .toEqual([plan("completed", "First plan"), plan("in_progress", "Second plan")]);
  });

  it("restores only the running turn's plan beside the composer", () => {
    const baseline = [plan("in_progress", "Old plan")];
    const activeTurn = { operationId: "op", clientIntentId: "intent", prompt, updates: [plan("pending", "New plan")] };
    const state = hydrate(view({ phase: "running", timeline: baseline, activeTurn }));
    expect(state.activePlan?.update).toEqual(plan("pending", "New plan"));
    expect(state.timeline.filter(({ type }) => type === "plan").map((item) => item.update)).toEqual(baseline);
    const noPlan = hydrate(view({ phase: "running", timeline: baseline, activeTurn: { ...activeTurn, updates: [] } }));
    expect(noPlan.activePlan).toBeUndefined();
  });

  it.each(["reconciling", "blocked"] as const)("archives the terminal overlay's plan while %s", (phase) => {
    const state = hydrate(view({
      phase,
      activeTurn: {
        operationId: "op", clientIntentId: "intent", prompt,
        updates: [plan("in_progress")], terminal: { stopReason: "end_turn" },
      },
    }));
    expect(state.activePlan).toBeUndefined();
    expect(state.timeline.find(({ type }) => type === "plan")).toMatchObject({ update: plan("in_progress") });
  });

  it("does not retire the current plan for a stale turn completion", () => {
    const active = update(start(), plan("in_progress"));
    const state = appReducer(active, {
      type: "bridge/turn_complete",
      event: {
        ...owner, type: "bridge/session_turn_complete", operationId: "old",
        clientIntentId: "old", response: { stopReason: "end_turn" },
      },
    });
    expect(state.activePlan).toBe(active.activePlan);
    expect(state.running).toBe(true);
  });

  it("also archives plans for legacy prompt cancellation and errors", () => {
    const active = update(start(), plan("in_progress"));
    for (const event of [
      { type: "acp/prompt_complete", requestId: "intent", sessionId: "s1", response: { stopReason: "cancelled" } },
      { type: "bridge/error", requestId: "intent", operation: "session/prompt", message: "Failed" },
    ] as const) {
      const state = appReducer(active, { type: "server/event", event });
      expect(state.activePlan).toBeUndefined();
      expect(state.timeline[1]).toMatchObject({ update: plan("in_progress") });
      expect(state.running).toBe(false);
    }
  });

  it("archives the active plan when the bridge stops", () => {
    const state = appReducer(update(start(), plan("in_progress")), {
      type: "server/event", event: { type: "bridge/phase", phase: "stopped" },
    });
    expect(state.activePlan).toBeUndefined();
    expect(state.timeline.find(({ type }) => type === "plan")).toMatchObject({ update: plan("in_progress") });
  });
});
