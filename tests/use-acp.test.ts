import { describe, expect, it, vi } from "vitest";
import {
  mostRecentSession,
  sendClientCommand,
  shouldReconnectAfterResume,
  startupAttachMethod,
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

  it("chooses only negotiated session restore methods", () => {
    expect(startupAttachMethod({ loadSession: true })).toBe("session/load");
    expect(startupAttachMethod({
      loadSession: false,
      sessionCapabilities: { resume: {} },
    })).toBe("session/resume");
    expect(startupAttachMethod({ sessionCapabilities: { list: {} } })).toBeUndefined();
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
