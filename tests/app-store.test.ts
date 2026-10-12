import { describe, expect, it, vi } from "vitest";
import type { BridgeSessionView } from "../shared/business-api";
import { createAppStore } from "../web/src/lib/app-store";
import { initialState } from "../web/src/lib/state";

function view(viewRevision: number, patch: Partial<BridgeSessionView> = {}): BridgeSessionView {
  return {
    bridgeEpoch: "epoch", sessionId: "session", sessionIncarnation: 1, viewRevision,
    historyRevision: "revision", phase: "ready", syncError: null, timeline: [], activeTurn: null,
    workspace: { cwd: "/workspace", session: {} }, controls: {},
    interactions: { permissions: {}, elicitations: {}, urlFlows: {} }, operation: null, terminals: {},
    ...patch,
  };
}

describe("app store", () => {
  it("applies actions synchronously so a following event sees the advanced view", () => {
    const store = createAppStore();
    const listener = vi.fn();
    store.subscribe(listener);
    store.dispatch({ type: "bridge/session_hydrate", view: view(1) });
    expect(store.getState().sessionView?.viewRevision).toBe(1);
    store.dispatch({
      type: "session/turn_update",
      notification: { sessionId: "session", update: {
        sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Live" },
      } },
      view: view(2),
    });
    expect(store.getState().sessionView?.viewRevision).toBe(2);
    expect(JSON.stringify(store.getState().timeline)).toContain("Live");
    expect(listener).toHaveBeenCalledTimes(2);
  });

  it("does not notify when an action leaves the state unchanged", () => {
    const store = createAppStore(initialState);
    const listener = vi.fn();
    const unsubscribe = store.subscribe(listener);
    store.dispatch({ type: "session/view_synced", view: undefined });
    expect(listener).not.toHaveBeenCalled();
    unsubscribe();
    store.dispatch({ type: "bridge/session_hydrate", view: view(1) });
    expect(listener).not.toHaveBeenCalled();
  });
});

describe("authoritative session view", () => {
  it("keeps the view beside its projection and drops it with the session", () => {
    const store = createAppStore();
    store.dispatch({ type: "bridge/session_hydrate", view: view(1) });
    const hydrated = store.getState().sessionView;
    expect(hydrated?.viewRevision).toBe(1);

    const terminal = { sessionId: "session", terminalId: "t", output: "out", truncated: false, released: false };
    store.dispatch({ type: "session/terminal_update", terminal, view: view(2, { terminals: { t: terminal } }) });
    expect(store.getState().sessionView?.terminals.t).toEqual(terminal);
    expect(store.getState().terminalSnapshots).toEqual([terminal]);

    const synced = view(3);
    const projection = store.getState().timeline;
    store.dispatch({ type: "session/view_synced", view: synced });
    expect(store.getState().sessionView).toBe(synced);
    expect(store.getState().timeline).toBe(projection);

    store.dispatch({ type: "session/reset" });
    expect(store.getState().sessionView).toBeUndefined();

    store.dispatch({ type: "bridge/session_hydrate", view: view(4) });
    store.dispatch({ type: "bridge/connection_replaced" });
    expect(store.getState().sessionView).toBeUndefined();
  });

  it("advances the view with the turn outcome that settles it", () => {
    const store = createAppStore();
    store.dispatch({ type: "bridge/session_hydrate", view: view(1, { phase: "running", activeTurn: {
      operationId: "op", clientIntentId: "intent", prompt: [], updates: [], terminal: null,
    } }) });
    const settled = view(2, { historyRevision: "revision-2" });
    store.dispatch({ type: "bridge/turn_complete", view: settled, event: {
      type: "bridge/session_turn_complete", bridgeEpoch: "epoch", sessionId: "session", sessionIncarnation: 1,
      viewRevision: 2, historyRevision: "revision-2", phase: "ready", operationId: "op", clientIntentId: "intent",
      response: { stopReason: "end_turn" },
    } });
    expect(store.getState().sessionView).toBe(settled);
    expect(store.getState().running).toBe(false);
  });
});
