import { describe, expect, it, vi } from "vitest";
import {
  mostRecentSession,
  sendClientCommand,
  shouldReconnectAfterResume,
  startupAttachMethod,
  startupHistoryStrategy,
} from "../web/src/lib/use-acp";

describe("browser ACP command transport", () => {
  const command = { type: "session/new", requestId: "request-1" } as const;

  it("reports a closed socket instead of silently dropping a command", () => {
    const errors: string[] = [];
    expect(sendClientCommand(undefined, command, (message) => errors.push(message))).toBe(false);
    expect(errors).toEqual(["Cannot send session/new: ACP WebSocket is not open"]);
  });

  it("serializes a command exactly once on an open socket", () => {
    const send = vi.fn();
    const errors: string[] = [];
    expect(sendClientCommand(
      { readyState: 1, send } as unknown as Pick<WebSocket, "readyState" | "send">,
      command,
      (message) => errors.push(message),
    )).toBe(true);
    expect(send).toHaveBeenCalledOnce();
    expect(send).toHaveBeenCalledWith(JSON.stringify(command));
    expect(errors).toEqual([]);
  });

  it("turns synchronous WebSocket failures into visible client errors", () => {
    const errors: string[] = [];
    expect(sendClientCommand(
      {
        readyState: 1,
        send: () => {
          throw new Error("send buffer rejected");
        },
      } as unknown as Pick<WebSocket, "readyState" | "send">,
      command,
      (message) => errors.push(message),
    )).toBe(false);
    expect(errors).toEqual(["Cannot send session/new: send buffer rejected"]);
  });

  it("loads completed reconnects only when history replay is negotiated", () => {
    const completedReconnectDecision = (
      capabilities: Parameters<typeof startupAttachMethod>[0],
    ) => startupAttachMethod(capabilities, false) ?? "history_unavailable";

    expect(completedReconnectDecision({ loadSession: true })).toBe("session/load");
    expect(completedReconnectDecision({
      loadSession: false,
      sessionCapabilities: { resume: {} },
    })).toBe("history_unavailable");
    expect(completedReconnectDecision({ sessionCapabilities: { list: {} } }))
      .toBe("history_unavailable");
  });

  it("does not issue a history load while an active turn can be restored locally", () => {
    expect(startupAttachMethod({ loadSession: true }, true) ?? "active_local")
      .toBe("active_local");
  });

  it("loads the stored session directly when load exists without session/list", () => {
    expect(startupHistoryStrategy(
      { loadSession: true },
      false,
      "stored-session",
    )).toEqual({
      kind: "direct_load",
      method: "session/load",
      sessionId: "stored-session",
    });
  });

  it("makes missing load history explicit and never treats resume as replay", () => {
    expect(startupHistoryStrategy(
      { sessionCapabilities: { resume: {} } },
      false,
      "stored-session",
    )).toEqual({
      kind: "history_unavailable",
      sessionId: "stored-session",
    });
    expect(startupHistoryStrategy(
      { loadSession: true, sessionCapabilities: { list: {} } },
      false,
      "stored-session",
    )).toEqual({
      kind: "list_then_load",
      method: "session/load",
    });
    expect(startupHistoryStrategy(
      { loadSession: true, sessionCapabilities: { list: {} } },
      true,
      "stored-session",
    )).toEqual({ kind: "active_local" });
  });

  it("rejects a command object that tries to upload completed browser history", () => {
    const send = vi.fn();
    const errors: string[] = [];
    const unsafeCommand = {
      type: "session/load",
      requestId: "load-saved",
      sessionId: "saved",
      history: [{ role: "agent", text: "browser-owned completed answer" }],
    } as unknown as Parameters<typeof sendClientCommand>[1];

    expect(sendClientCommand(
      { readyState: 1, send } as unknown as Pick<WebSocket, "readyState" | "send">,
      unsafeCommand,
      (message) => errors.push(message),
    )).toBe(false);
    expect(send).not.toHaveBeenCalled();
    expect(errors).toHaveLength(1);
  });

  it("restores the newest timestamped session without depending on Agent ordering", () => {
    expect(mostRecentSession([
      { sessionId: "older", cwd: "/workspace", updatedAt: "2026-08-20T00:00:00Z" },
      { sessionId: "newest", cwd: "/workspace", updatedAt: "2026-08-30T00:00:00Z" },
      { sessionId: "undated", cwd: "/workspace" },
    ])?.sessionId).toBe("newest");
    expect(mostRecentSession([
      { sessionId: "first", cwd: "/workspace" },
      { sessionId: "second", cwd: "/workspace" },
    ])?.sessionId).toBe("first");
    expect(mostRecentSession([])).toBeUndefined();
  });

  it("probes healthy resumed sockets and reconnects only closed sockets", () => {
    expect(shouldReconnectAfterResume(undefined, 10_000, 3)).toBe(true);
    expect(shouldReconnectAfterResume(1_000, 20_000, 1)).toBe(false);
    expect(shouldReconnectAfterResume(1_500, 2_000, 1)).toBe(false);
    expect(shouldReconnectAfterResume(undefined, 2_000, 1)).toBe(false);
  });
});
