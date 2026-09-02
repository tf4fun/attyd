import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { ElicitationCard, ExternalFlowCard } from "../web/src/components/acp/elicitation";
import { PromptComposer } from "../web/src/components/acp/prompt-composer";
import { PlanCard } from "../web/src/components/acp/plan";
import { CompactionCard } from "../web/src/components/acp/compaction";
import { Conversation } from "../web/src/components/acp/conversation";
import { ContentBlocks } from "../web/src/components/acp/content-block";
import { RawJson } from "../web/src/components/acp/raw-json";
import { ChangeReview } from "../web/src/components/acp/change-review";
import { collectReviewChanges } from "../web/src/lib/review-changes";

describe("ACP UI component contract", () => {
  it("renders Agent-reported ACP diffs as a read-only interaction review", () => {
    const summary = collectReviewChanges([{
      id: "edit",
      type: "tool",
      call: {
        toolCallId: "edit",
        title: "Edit workspace",
        status: "completed",
        content: [{
          type: "diff",
          path: "/workspace/app.ts",
          oldText: "const value = 1;\n",
          newText: "const value = 2;\n",
        }],
      },
      raw: [],
    }]);
    const html = renderToStaticMarkup(
      <ChangeReview summary={summary} open onToggle={() => undefined} />,
    );

    expect(html).toContain('aria-label="Agent-reported changes"');
    expect(html).toContain("Agent-reported ACP diffs");
    expect(html).toContain("Read-only");
    expect(html).toContain("does not define client rollback");
    expect(html).toContain("/workspace/app.ts");
    expect(html).toContain("const value = 1;");
    expect(html).toContain("const value = 2;");
    expect(html).not.toContain("Accept");
    expect(html).not.toContain("Reject");
  });

  it("gives the icon-only prompt action an accessible name", () => {
    const html = renderToStaticMarkup(
      <PromptComposer
        disabled={false}
        running={false}
        capabilities={{ image: true }}
        commands={[]}
        onSubmit={() => undefined}
        onCancel={() => undefined}
      />,
    );
    expect(html).toContain('aria-label="Send prompt"');
    expect(html).toContain('title="Attach ACP content"');
    expect(html).toContain('title="Add ACP resource link"');
    expect(html).toContain('aria-label="Expand message composer"');
    expect(html).toContain('aria-keyshortcuts="Alt+Shift+Escape"');
    expect(html).toContain('role="combobox"');
    expect(html).toContain('aria-autocomplete="list"');
    expect(html).toContain('aria-expanded="false"');
  });

  it("maps ACP form constraints and action labels into semantic HTML", () => {
    const html = renderToStaticMarkup(
      <ElicitationCard
        pending={{
          elicitationId: "form",
          request: {
            sessionId: "session",
            mode: "form",
            message: "Configure",
            requestedSchema: {
              type: "object",
              required: ["name", "count", "channel", "tags"],
              properties: {
                channel: {
                  type: "string",
                  title: "Channel",
                  enum: ["stable", "preview"],
                },
                count: {
                  type: "integer",
                  title: "Count",
                  minimum: 1,
                  maximum: 3,
                },
                name: {
                  type: "string",
                  title: "Name",
                  minLength: 2,
                  pattern: "^[A-Za-z ]+$",
                },
                tags: {
                  type: "array",
                  title: "Tags",
                  minItems: 1,
                  maxItems: 2,
                  items: { type: "string", enum: ["fast", "safe"] },
                },
              },
            },
          },
        }}
        onRespond={() => undefined}
      />,
    );
    expect(html).toContain("Agent needs input");
    expect(html).toContain('minLength="2"');
    expect(html).toContain('pattern="^[A-Za-z ]+$"');
    expect(html).toContain('min="1"');
    expect(html).toContain('max="3"');
    expect(html).toContain("Submit");
    expect(html).toContain("Decline");
    expect(html).toContain("Cancel");
    expect(html).toContain('role="dialog"');
    expect(html).toContain('aria-label="Agent input request"');
    expect(html.indexOf("Name *")).toBeLessThan(html.indexOf("Count *"));
    expect(html.indexOf("Count *")).toBeLessThan(html.indexOf("Channel *"));
  });

  it("renders a cancelled external flow as terminal and dismissible", () => {
    const html = renderToStaticMarkup(
      <ExternalFlowCard
        flow={{
          elicitationId: "external",
          sessionId: "session",
          message: "Connect account",
          status: "cancelled",
          abortReason: "session_closed",
        }}
        onDismiss={() => undefined}
      />,
    );
    expect(html).toContain("External flow cancelled");
    expect(html).toContain('aria-label="Dismiss external flow"');
    expect(html).not.toContain("Waiting for external flow");
  });

  it("renders prompt stop reason and semantic token usage", () => {
    const html = renderToStaticMarkup(
      <Conversation timeline={[{
        id: "stop",
        type: "stop",
        response: {
          stopReason: "max_tokens",
          usage: {
            totalTokens: 21,
            inputTokens: 13,
            outputTokens: 8,
            thoughtTokens: 3,
          },
        },
      }]} />,
    );
    expect(html).toContain("max_tokens");
    expect(html).toContain("21 tokens");
    expect(html).toContain("13 input · 8 output · 3 reasoning");
    expect(html).toContain("Turn response");
    expect(html).not.toContain("ACP payload");
  });

  it("renders structured ACP turn failures with Agent-thread retry actions", () => {
    const html = renderToStaticMarkup(
      <Conversation
        canReusePrompt
        onReusePrompt={() => undefined}
        onRetryPrompt={() => undefined}
        timeline={[{
          id: "error",
          type: "error",
          message: "ACP error -32603: Provider temporarily unavailable",
          operation: "session/prompt",
          code: -32_603,
          data: { retryAfterMs: 250 },
          dataBytes: 20,
          retryBlocks: [{ type: "text", text: "Try this again" }],
        }]}
      />,
    );

    expect(html).toContain('role="alert"');
    expect(html).toContain("Agent turn failed");
    expect(html).toContain("Retry");
    expect(html).toContain("Edit prompt");
    expect(html).toContain("ACP error details");
    expect(html).toContain("retryAfterMs");
  });

  it("renders thoughts as collapsed reasoning and message payloads as contextual metadata", () => {
    const html = renderToStaticMarkup(
      <Conversation timeline={[
        {
          id: "assistant",
          type: "assistant",
          chunks: [
            {
              id: "thought",
              role: "thought",
              blocks: [{ type: "text", text: "Inspect the workspace." }],
              messageId: "thought-1",
              raw: [{ update: { sessionUpdate: "agent_thought_chunk" } }],
            },
            {
              id: "agent",
              role: "agent",
              blocks: [{ type: "text", text: "Done." }],
              messageId: "message-1",
              raw: [{ update: { sessionUpdate: "agent_message_chunk" } }],
            },
          ],
        },
      ]} />,
    );

    expect(html).toContain('class="thinking-block"');
    expect(html).toContain('aria-label="Thinking info"');
    expect(html).toContain('aria-label="Thinking debug information"');
    expect(html).toContain('class="message message-agent assistant-chunk"');
    expect(html).toContain("Message events");
    expect(html).toContain('aria-label="Message info"');
    expect(html).toContain('aria-label="Message debug information"');
    expect(html).toContain('data-thread-search-ignore="true" hidden=""');
    expect(html).toContain('aria-label="Copy agent response"');
    const thinkingMarkup = html.slice(
      html.indexOf('<section class="thinking-block"'),
      html.indexOf('<div class="message message-agent assistant-chunk"'),
    );
    expect(thinkingMarkup).not.toContain('aria-label="Copy agent response"');
    expect(html).not.toContain("ACP payload");
  });

  it("marks only the protocol-active thought as live and initially expanded", () => {
    const html = renderToStaticMarkup(
      <Conversation
        agentActivity={{ kind: "thinking", timelineId: "thought" }}
        timeline={[{
          id: "assistant",
          type: "assistant",
          chunks: [{
            id: "thought",
            role: "thought",
            blocks: [{ type: "text", text: "Inspecting the workspace." }],
            messageId: "thought-1",
            raw: [],
          }],
        }]}
      />,
    );
    expect(html).toContain("thinking-block thinking-live");
    expect(html).toContain('data-live="true"');
    expect(html).toContain('data-open="true"');
    expect(html).toContain("Thinking…");
    expect(html).toContain("Live");
  });

  it("renders loaded ACP user messages as the same editable prompt shape", () => {
    const html = renderToStaticMarkup(
      <Conversation
        canReusePrompt
        onReusePrompt={() => undefined}
        timeline={[{
          id: "loaded-user",
          type: "message",
          role: "protocol-user",
          blocks: [{ type: "text", text: "Restored prompt" }],
          messageId: "user-1",
          raw: [],
        }]}
      />,
    );

    expect(html).toContain("message-protocol-user message-user");
    expect(html).toContain('aria-label="Edit and resend user message"');
    expect(html).not.toContain("Agent echo");
  });

  it("keeps unusual raw ACP values inspectable without crashing the thread", () => {
    const cyclic: { value: bigint; self?: unknown } = { value: 12n };
    cyclic.self = cyclic;

    const bigintHtml = renderToStaticMarkup(<RawJson value={{ value: 12n }} />);
    const cyclicHtml = renderToStaticMarkup(<RawJson value={cyclic} />);

    expect(bigintHtml).toContain("12n");
    expect(cyclicHtml).toContain("Unable to serialize payload");
  });

  it("renders an ID-addressed plan removal as a semantic terminal card", () => {
    const html = renderToStaticMarkup(
      <PlanCard entryId="plan-entry" update={{ sessionUpdate: "plan_removed", planId: "plan-1" }} />,
    );
    expect(html).toContain("Plan plan-1 removed");
    expect(html).toContain("plan-removed");
  });

  it("renders completed compaction content as a collapsed semantic disclosure", () => {
    const html = renderToStaticMarkup(
      <CompactionCard item={{
        id: "compaction:test",
        type: "compaction",
        compactionId: "test",
        status: "completed",
        blocks: [{ type: "text", text: "Compact summary." }],
        raw: [],
      }} />,
    );
    expect(html).toContain("Context compacted");
    expect(html).toContain("Compact summary.");
    expect(html).toContain("status-completed");
    expect(html).toContain("1 summary block");
    expect(html).not.toContain(" open=\"");
  });

  it("renders valid multimodal content and degrades invalid media safely", () => {
    const html = renderToStaticMarkup(<ContentBlocks blocks={[
      { type: "image", data: "iVBORw==", mimeType: "image/png" },
      { type: "audio", data: "AA==", mimeType: "audio/mpeg" },
      {
        type: "resource_link",
        name: "Workspace file",
        title: "README",
        uri: "file:///workspace/README.md",
        description: "Project overview",
        mimeType: "text/markdown",
        size: 2_048,
        annotations: {
          audience: ["user"],
          priority: 0.75,
          lastModified: "2026-08-31T12:00:00Z",
        },
      },
      {
        type: "resource",
        annotations: { audience: ["assistant"] },
        resource: {
          uri: "urn:fixture:note",
          mimeType: "text/plain",
          text: "Embedded note",
        },
      },
      { type: "image", data: "not-base64", mimeType: "image/png" },
    ]} />);

    expect(html).toContain('src="data:image/png;base64,iVBORw=="');
    expect(html).toContain('src="data:audio/mpeg;base64,AA=="');
    expect(html).toContain("README");
    expect(html).toContain("Project overview");
    expect(html).toContain("text/markdown · 2 KiB");
    expect(html).toContain("priority 0.75");
    expect(html).toContain("Intended for user");
    expect(html).toContain("ACP content annotations");
    expect(html).toContain("Intended for assistant");
    expect(html).not.toContain('href="file:///workspace/README.md"');
    expect(html).toContain("Embedded note");
    expect(html).toContain("Invalid ACP image content");
    expect(html).not.toContain("not-base64");
  });

  it("renders zero-based tool locations, diffs, and terminal references faithfully", () => {
    const html = renderToStaticMarkup(
      <Conversation
        timeline={[{
          id: "tool:fixture",
          type: "tool",
          call: {
            sessionUpdate: "tool_call",
            toolCallId: "fixture",
            title: "shell · cat /workspace/fixture.ts",
            kind: "execute",
            status: "completed",
            rawInput: { command: "cat /workspace/fixture.ts", timeoutMs: 10_000 },
            locations: [{ path: "/workspace/fixture.ts", line: 0 }],
            content: [
              {
                type: "content",
                content: {
                  type: "text",
                  text: "**literal tool output**\n\n- first result",
                },
              },
              {
                type: "diff",
                path: "/workspace/fixture.ts",
                oldText: "before",
                newText: "after",
              },
              { type: "terminal", terminalId: "terminal-1" },
            ],
          },
          raw: [],
        }, {
          id: "tool:structured",
          type: "tool",
          call: {
            toolCallId: "structured",
            title: "fetch · Load project metadata",
            kind: "fetch",
            status: "completed",
            rawInput: { uri: "https://example.test/project" },
            rawOutput: { status: 200, project: "attyd" },
          },
          raw: [{ update: { sessionUpdate: "tool_call" } }],
        }]}
        terminalSnapshots={[{
          sessionId: "session",
          terminalId: "terminal-1",
          output: "command output",
          truncated: true,
          exitStatus: { exitCode: 0 },
          released: true,
        }]}
      />,
    );

    expect(html).toContain("/workspace/fixture.ts:0");
    expect(html).toContain("− before");
    expect(html).toContain("+ after");
    expect(html).toContain("terminal-1");
    expect(html).toContain("command output");
    expect(html).toContain("exit 0");
    expect(html).toContain("Earlier output was truncated.");
    expect(html).toContain('<div class="structured-scalar structured-markdown">');
    expect(html).toContain("<strong>literal tool output</strong>");
    expect(html).toContain("<li>first result</li>");
    expect(html).toContain('data-tool-status="completed"');
    expect(html).toContain('data-live="false"');
    expect(html).toContain('aria-label="Tool status: Completed"');
    expect(html).toContain('<strong>shell</strong>');
    expect(html).toContain("Description");
    expect(html).toContain("Input");
    expect(html).toContain("command");
    expect(html).toContain("timeoutMs");
    expect(html).toContain("Output");
    expect(html).toContain("status");
    expect(html).toContain("project");
    expect(html).toContain("attyd");
    expect(html.match(/class="structured-data"/g)?.length).toBeGreaterThanOrEqual(3);
    expect(html).toContain('aria-label="Tool info"');
    expect(html).toContain('aria-label="Tool debug information"');
    expect(html).toContain("Tool call ID");
    expect(html).toContain("Message events");
    expect(html).not.toContain("open=\"\"");
  });

});
