import { describe, expect, it } from "vitest";
import { contentBlocksToMarkdown, timelineToMarkdown } from "../src/lib/thread-markdown";

describe("Zed-style thread Markdown export", () => {
  it("preserves ACP message content, annotations, tools, terminals, and turn usage", () => {
    const markdown = timelineToMarkdown([
      {
        id: "user",
        type: "message",
        role: "protocol-user",
        messageId: "user-1",
        blocks: [{ type: "text", text: "Inspect the project." }],
        raw: [],
      },
      {
        id: "agent",
        type: "assistant",
        chunks: [{
          id: "agent-chunk",
          role: "agent",
          blocks: [{
            type: "resource_link",
            name: "README",
            uri: "file:///workspace/README.md",
            description: "Project overview",
            annotations: { audience: ["user"], priority: 0.8 },
          }],
          raw: [],
        }],
      },
      {
        id: "tool:run",
        type: "tool",
        call: {
          toolCallId: "run",
          title: "Run tests",
          kind: "execute",
          status: "completed",
          locations: [{ path: "/workspace/package.json", line: 0 }],
          content: [
            { type: "terminal", terminalId: "terminal-1" },
            { type: "diff", path: "/workspace/a.ts", oldText: "old", newText: "new" },
          ],
          rawInput: { command: "npm test" },
        },
        raw: [],
      },
      {
        id: "stop",
        type: "stop",
        response: {
          stopReason: "end_turn",
          usage: { totalTokens: 21, inputTokens: 13, outputTokens: 8 },
        },
      },
    ], {
      title: "Saved # thread",
      agentName: "Goose",
      sessionId: "session-1",
      cwd: "/workspace",
      terminalSnapshots: [{
        sessionId: "session-1",
        terminalId: "terminal-1",
        output: "all tests passed",
        truncated: false,
        exitStatus: { exitCode: 0 },
        released: true,
      }],
    });

    expect(markdown).toContain("# Saved \\# thread");
    expect(markdown).toContain("- Agent: Goose");
    expect(markdown).toContain("## You\n\nInspect the project.");
    expect(markdown).toContain("[README](file:///workspace/README.md)");
    expect(markdown).toContain("ACP annotations · audience: user · priority: 0.8");
    expect(markdown).toContain("## Tool · Run tests");
    expect(markdown).toContain("`/workspace/package.json:0`");
    expect(markdown).toContain("all tests passed");
    expect(markdown).toContain("-old\n+new");
    expect(markdown).toContain("Turn complete · end_turn · 21 tokens");
  });

  it("uses a safe longer fence for embedded text that already contains backticks", () => {
    const markdown = contentBlocksToMarkdown([{
      type: "resource",
      resource: {
        uri: "urn:fixture:code",
        mimeType: "text/markdown",
        text: "```ts\nconst ok = true;\n```",
      },
    }]);
    expect(markdown).toContain("````text/markdown");
    expect(markdown).toContain("```ts");
  });
});
