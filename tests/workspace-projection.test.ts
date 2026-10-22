import { describe, expect, it } from "vitest";
import type { GlobalLive, SessionSummary } from "../shared/protocol";
import type { TimelineItem } from "../web/src/lib/state";
import { listedSessions, standaloneTerminals } from "../web/src/lib/workspace-projection";

function global(sessions: Record<string, SessionSummary>): GlobalLive {
  return { sessions } as unknown as GlobalLive;
}

const summary = (title: string | null): SessionSummary => ({
  sessionId: "s", cwd: "/w", title, op: { kind: "open", status: "completed" }, pending: { interactions: 0, urlFlows: 0 },
});

describe("terminals of the thread", () => {
  it("shows only terminals no tool card shows", () => {
    const terminals = ["a", "b"].map((terminalId) => ({ sessionId: "s", terminalId, output: "", truncated: false, released: false }));
    const timeline: TimelineItem[] = [{
      id: "entry:0", type: "tool", raw: [],
      call: { toolCallId: "t", title: "Run", content: [{ type: "terminal", terminalId: "a" }] },
    }];
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
