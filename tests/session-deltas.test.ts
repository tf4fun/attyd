import { describe, expect, it } from "vitest";
import type { BridgeSessionView, SessionDeltaChange } from "../shared/business-api";
import { applyLocalSessionDelta } from "../web/src/lib/session-deltas";

function view(overrides: Partial<BridgeSessionView> = {}): BridgeSessionView {
  return {
    bridgeEpoch: "epoch", sessionId: "session", sessionIncarnation: 1, viewRevision: 4,
    historyRevision: "epoch:1:4", phase: "running", syncError: null,
    timeline: [], activeTurn: null,
    workspace: { cwd: "/work", session: { modes: { currentModeId: "ask", availableModes: [] } } },
    controls: {}, interactions: { permissions: {}, elicitations: {}, urlFlows: {} },
    operation: null, terminals: {},
    ...overrides,
  };
}

const permission = {
  sessionId: "session",
  toolCall: { toolCallId: "tool", title: "Run" },
  options: [{ optionId: "allow", name: "Allow", kind: "allow_once" as const }],
};

describe("local session deltas", () => {
  it("upserts and removes interactions and advances the view revision", () => {
    const upserted = applyLocalSessionDelta(view(), {
      kind: "interaction_upsert",
      interaction: { interactionId: "p1", type: "permission", request: permission },
    }, 5);
    expect(upserted?.view.viewRevision).toBe(5);
    expect(upserted?.view.interactions.permissions.p1).toEqual({
      interactionId: "p1", operationId: null, request: permission,
    });
    expect(upserted?.settledControl).toBeUndefined();

    const removed = applyLocalSessionDelta(upserted!.view, {
      kind: "interaction_remove", interactionId: "p1",
    }, 6);
    expect(removed?.view.interactions.permissions).toEqual({});
    expect(removed?.view.viewRevision).toBe(6);
  });

  it("settles mode and configuration controls and their running operation", () => {
    const configOptions = [{
      id: "model", name: "Model", type: "select", currentValue: "fast",
      options: [{ value: "fast", name: "Fast" }],
    }] as unknown as Extract<SessionDeltaChange, { control: "configuration" }>["configOptions"];
    const config = applyLocalSessionDelta(
      view({ operation: { operationId: "op", kind: "set_config", stage: "running" } }),
      { kind: "control_update", control: "configuration", configId: "model", value: "fast", configOptions },
      5,
    );
    expect(config?.settledControl).toBe("config");
    expect(config?.view.workspace.session?.configOptions).toBe(configOptions);
    expect(config?.view.operation).toBeNull();

    const mode = applyLocalSessionDelta(
      view({ operation: { operationId: "fork", kind: "fork", stage: "running" } }),
      { kind: "control_update", control: "mode", modeId: "code" },
      5,
    );
    expect(mode?.settledControl).toBe("mode");
    expect(mode?.view.workspace.session?.modes?.currentModeId).toBe("code");
    expect(mode?.view.operation?.kind).toBe("fork");
  });

  it("asks for a refetch instead of guessing", () => {
    expect(applyLocalSessionDelta(view(), { kind: "sync_state", phase: "ready" }, 5)).toBeUndefined();
    expect(applyLocalSessionDelta(
      view({ workspace: { cwd: "/work", session: null } }),
      { kind: "control_update", control: "mode", modeId: "code" },
      5,
    )).toBeUndefined();
    expect(applyLocalSessionDelta(view(), {
      kind: "interaction_upsert",
      interaction: { interactionId: 7, type: "permission" },
    } as unknown as SessionDeltaChange, 5)).toBeUndefined();
    const url = applyLocalSessionDelta(view(), {
      kind: "interaction_upsert",
      interaction: {
        interactionId: "e1", type: "elicitation",
        request: { sessionId: "session", mode: "url", elicitationId: "flow", url: "https://example.com", message: "Sign in" },
      },
    }, 5);
    expect(applyLocalSessionDelta(url!.view, { kind: "interaction_remove", interactionId: "e1" }, 6))
      .toBeUndefined();
  });
});
