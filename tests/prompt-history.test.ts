import { describe, expect, it } from "vitest";
import type { ContentBlock } from "@agentclientprotocol/sdk";
import { collectPromptHistory } from "../web/src/lib/prompt-history";
import type { TimelineItem } from "../web/src/lib/state";

describe("ACP prompt history", () => {
  it("collects local and Agent-replayed user messages in thread order", () => {
    const local = [{ type: "text", text: "local" }] satisfies ContentBlock[];
    const replayed = [
      { type: "resource_link", uri: "file:///workspace/app.ts", name: "app.ts" },
      { type: "text", text: "replayed" },
    ] satisfies ContentBlock[];

    expect(collectPromptHistory([
      message("local", "user", local),
      message("answer", "agent", [{ type: "text", text: "answer" }]),
      message("replayed", "protocol-user", replayed),
    ])).toEqual([local, replayed]);
  });

  it("collapses only an adjacent ACP echo of the optimistic user prompt", () => {
    const repeated = [{ type: "resource_link", uri: "urn:same", name: "same" }] satisfies ContentBlock[];
    const reorderedEcho = [{ name: "same", uri: "urn:same", type: "resource_link" }] satisfies ContentBlock[];
    expect(collectPromptHistory([
      message("optimistic", "user", repeated),
      message("echo", "protocol-user", reorderedEcho),
      message("answer", "agent", [{ type: "text", text: "done" }]),
      message("intentional-repeat", "user", repeated),
    ])).toEqual([repeated, repeated]);
  });

  it("preserves complete prompt history and ContentBlock arrays", () => {
    const timeline = Array.from({ length: 100 + 3 }, (_, index) =>
      message(String(index), "user", [{ type: "text", text: String(index) }])
    );
    const history = collectPromptHistory(timeline);
    expect(history).toHaveLength(103);
    expect(history[0]).toEqual([{ type: "text", text: "0" }]);
    expect(history.at(-1)).toEqual([{ type: "text", text: "102" }]);
  });
});

function message(
  id: string,
  role: Extract<TimelineItem, { type: "message" }>["role"],
  blocks: ContentBlock[],
): TimelineItem {
  return { id, type: "message", role, blocks, raw: [] };
}
