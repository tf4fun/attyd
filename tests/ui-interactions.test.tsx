// @vitest-environment happy-dom

import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ContentBlock } from "@agentclientprotocol/sdk";
import { ElicitationCard } from "../src/components/acp/elicitation";
import { Conversation } from "../src/components/acp/conversation";
import { PermissionCard } from "../src/components/acp/permission";
import { PromptComposer } from "../src/components/acp/prompt-composer";
import { QueuedPrompts, type QueuedPrompt } from "../src/components/acp/queued-prompts";
import { SessionControls } from "../src/components/acp/session-controls";
import { ChangeReview } from "../src/components/acp/change-review";
import { collectReviewChanges } from "../src/lib/review-changes";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

describe("ACP interactive UI contract", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
  });

  it("exposes the Agent change review as a controlled disclosure", async () => {
    const onToggle = vi.fn();
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
          oldText: "old\n",
          newText: "new\n",
        }],
      },
      raw: [],
    }]);
    await render(root, <ChangeReview summary={summary} open={false} onToggle={onToggle} />);

    const trigger = requireElement<HTMLButtonElement>(container.querySelector(".change-review-trigger"));
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(container.querySelector(".change-review-panel")).toBeNull();
    await click(trigger);
    expect(onToggle).toHaveBeenCalledOnce();

    await render(root, <ChangeReview summary={summary} open onToggle={onToggle} />);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    expect(container.querySelector(".change-review-panel")?.textContent).toContain("old");
    expect(container.querySelector(".change-review-panel")?.textContent).toContain("new");
  });

  it("auto-expands live thinking, collapses on the next ACP phase, and remains inspectable", async () => {
    const timeline = [{
      id: "assistant",
      type: "assistant" as const,
      chunks: [{
        id: "thought",
        role: "thought" as const,
        blocks: [{ type: "text" as const, text: "Inspecting the workspace." }],
        messageId: "thought-1",
        raw: [],
      }],
    }];
    await render(root, (
      <Conversation
        timeline={timeline}
        agentActivity={{ kind: "thinking", timelineId: "thought" }}
      />
    ));
    const thinking = requireElement<HTMLElement>(container.querySelector(".thinking-block"));
    expect(thinking.dataset.open).toBe("true");
    expect(thinking.dataset.live).toBe("true");

    await render(root, (
      <Conversation timeline={timeline} agentActivity={{ kind: "responding" }} />
    ));
    expect(thinking.dataset.open).toBe("false");
    expect(thinking.dataset.live).toBe("false");

    await click(requireElement(thinking.querySelector(".thinking-disclosure")));
    await render(root, (
      <Conversation timeline={timeline} agentActivity={{ kind: "responding" }} />
    ));
    expect(thinking.dataset.open).toBe("true");
  });

  it("keeps ACP tools collapsed by default and preserves manual disclosure", async () => {
    const pendingTool = {
      id: "tool:inspect",
      type: "tool" as const,
      call: {
        toolCallId: "inspect",
        title: "Inspect workspace",
        kind: "read" as const,
        status: "pending" as const,
        rawInput: { path: "/workspace" },
      },
      raw: [{ update: { sessionUpdate: "tool_call" } }],
    };
    await render(root, <Conversation timeline={[pendingTool]} />);
    const disclosure = requireElement<HTMLElement>(container.querySelector(".tool-card"));
    expect(disclosure.dataset.open).toBe("false");
    expect(disclosure.dataset.live).toBe("true");
    expect(disclosure.querySelector(".tool-status")?.getAttribute("aria-label"))
      .toBe("Tool status: Pending");
    const body = requireElement<HTMLElement>(disclosure.querySelector(".tool-body"));
    expect(body.hasAttribute("hidden")).toBe(true);
    expect(disclosure.querySelector(".tool-card-header")?.querySelector('[aria-label="Tool info"]'))
      .toBeNull();
    await click(requireElement(disclosure.querySelector(".tool-disclosure")));
    expect(disclosure.dataset.open).toBe("true");
    expect(body.hasAttribute("hidden")).toBe(false);
    const toolInfo = requireElement<HTMLButtonElement>(
      body.querySelector('button[aria-label="Tool info"]'),
    );
    expect(disclosure.querySelector(".debug-info-panel")?.hasAttribute("hidden")).toBe(true);
    await click(toolInfo);
    expect(disclosure.dataset.open).toBe("true");
    expect(toolInfo.getAttribute("aria-expanded")).toBe("true");
    expect(disclosure.querySelector(".tool-input")?.textContent).toContain("/workspace");
    expect(disclosure.querySelector(".debug-info-panel")?.textContent).toContain("Tool call ID");
    expect(disclosure.querySelector(".debug-info-panel")?.textContent).toContain("tool_call");
    expect(disclosure.querySelector(".debug-info-panel")?.querySelector(".raw-json")).toBeNull();
    await click(toolInfo);
    expect(disclosure.dataset.open).toBe("true");
    expect(disclosure.querySelector(".debug-info-panel")?.hasAttribute("hidden")).toBe(true);

    await render(root, (
      <Conversation timeline={[{
        ...pendingTool,
        call: { ...pendingTool.call, status: "in_progress", title: "Inspecting workspace" },
      }]} />
    ));
    expect(disclosure.dataset.open).toBe("true");
    expect(disclosure.querySelector(".tool-status")?.textContent).toContain("Running");

    const completedWithDiff = {
      ...pendingTool,
      call: {
        ...pendingTool.call,
        status: "completed" as const,
        content: [{
          type: "diff" as const,
          path: "/workspace/app.ts",
          oldText: "before",
          newText: "after",
        }],
      },
    };
    await render(root, <Conversation timeline={[completedWithDiff]} />);
    expect(disclosure.dataset.open).toBe("true");
    expect(disclosure.dataset.live).toBe("false");
    expect(disclosure.querySelector(".tool-status")?.textContent).toContain("Completed");

    await click(requireElement(disclosure.querySelector(".tool-disclosure")));
    await render(root, (
      <Conversation timeline={[
        completedWithDiff,
        {
          id: "answer",
          type: "assistant",
          chunks: [{
            id: "answer-chunk",
            role: "agent",
            blocks: [{ type: "text", text: "Done." }],
            raw: [],
          }],
        },
      ]} />
    ));
    expect(disclosure.dataset.open).toBe("false");

    await render(root, (
      <Conversation timeline={[{
        ...pendingTool,
        id: "tool:failed",
        call: { ...pendingTool.call, toolCallId: "failed", status: "failed" },
      }]} />
    ));
    const failed = requireElement<HTMLElement>(container.querySelector(".tool-card"));
    expect(failed.dataset.open).toBe("false");
    expect(failed.querySelector(".tool-status")?.getAttribute("aria-label"))
      .toBe("Tool status: Failed");
  });

  it("follows ACP compaction lifecycle, then preserves the user's disclosure choice", async () => {
    const compaction = {
      id: "compaction:test",
      type: "compaction" as const,
      compactionId: "test",
      status: "in_progress" as const,
      blocks: [{ type: "text" as const, text: "Streaming summary." }],
      raw: [],
    };
    await render(root, <Conversation timeline={[compaction]} />);
    const disclosure = requireElement<HTMLDetailsElement>(
      container.querySelector(".compaction-card"),
    );
    expect(disclosure.open).toBe(true);
    expect(disclosure.dataset.live).toBe("true");
    expect(disclosure.querySelector("summary")?.textContent).toContain("Compacting context");

    await render(root, (
      <Conversation timeline={[{ ...compaction, status: "completed" }]} />
    ));
    expect(disclosure.open).toBe(false);
    expect(disclosure.dataset.live).toBe("false");
    expect(disclosure.querySelector("summary")?.textContent).toContain("Context compacted");

    await act(async () => {
      disclosure.open = true;
      disclosure.dispatchEvent(new Event("toggle"));
    });
    await render(root, (
      <Conversation timeline={[{
        ...compaction,
        status: "completed",
        blocks: [{ type: "text", text: "Final summary." }],
      }]} />
    ));
    expect(disclosure.open).toBe(true);
    expect(disclosure.textContent).toContain("Final summary.");

    await render(root, (
      <Conversation timeline={[{
        ...compaction,
        status: "failed",
        error: "Compaction provider failed.",
      }]} />
    ));
    expect(disclosure.open).toBe(true);
    expect(disclosure.textContent).toContain("Compaction provider failed.");

    await render(root, (
      <Conversation timeline={[{ ...compaction, status: "cancelled" }]} />
    ));
    expect(disclosure.open).toBe(false);
    expect(disclosure.querySelector("summary")?.textContent)
      .toContain("Context compaction cancelled");
  });


  it("supports keyboard command selection and Enter submission", async () => {
    const onSubmit = vi.fn();
    const onCancel = vi.fn();
    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        commands={[
          {
            name: "bootstrap",
            description: "Initialize the workspace",
            input: { hint: "path" },
          },
          { name: "status", description: "Show status" },
        ]}
        onSubmit={onSubmit}
        onCancel={onCancel}
      />
    ));

    const composer = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );
    await press(composer, "Enter");
    expect(onSubmit).not.toHaveBeenCalled();
    await replaceText(composer, "/");
    expect(composer.getAttribute("aria-expanded")).toBe("true");
    expect(container.querySelectorAll('[role="option"]')).toHaveLength(2);
    expect(container.querySelector('[role="option"][aria-selected="true"]')?.textContent)
      .toContain("/bootstrap");

    await press(composer, "ArrowDown");
    expect(container.querySelector('[role="option"][aria-selected="true"]')?.textContent)
      .toContain("/status");
    await press(composer, "Enter");
    expect(composer.value).toBe("/status");
    expect(composer.getAttribute("aria-expanded")).toBe("false");

    await replaceText(composer, "usage-flow");
    await press(composer, "Enter");
    expect(onSubmit).toHaveBeenCalledWith("usage-flow", []);
    expect(composer.value).toBe("");

    await render(root, (
      <PromptComposer
        disabled={false}
        running
        commands={[]}
        onSubmit={onSubmit}
        onCancel={onCancel}
      />
    ));
    const runningComposer = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );
    expect(runningComposer.placeholder).toBe("Queue a follow-up…");
    await press(runningComposer, "Escape");
    expect(onCancel).toHaveBeenCalledOnce();
    await replaceText(runningComposer, "queued follow-up");
    await press(runningComposer, "Enter");
    expect(onSubmit).toHaveBeenLastCalledWith("queued follow-up", []);
    expect(runningComposer.value).toBe("");
    await click(requireElement(container.querySelector('button[aria-label="Stop current turn"]')));
    expect(onCancel).toHaveBeenCalledTimes(2);
  });

  it("navigates Zed-style prompt history and resends exact ACP blocks", async () => {
    const onSubmit = vi.fn();
    const older = [
      { type: "text", text: "inspect the image" },
      {
        type: "image",
        mimeType: "image/png",
        data: "AQID",
        uri: "file:///workspace/pixel.png",
      },
    ] satisfies ContentBlock[];
    const latest = [
      {
        type: "resource_link",
        uri: "file:///workspace/app.ts",
        name: "app.ts",
        title: "Application",
      },
      { type: "text", text: "review this file" },
      {
        type: "resource",
        resource: {
          uri: "file:///workspace/note.md",
          mimeType: "text/markdown",
          text: "# Note",
        },
      },
    ] satisfies ContentBlock[];
    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        capabilities={{ image: true, embeddedContext: true }}
        commands={[]}
        history={[older, latest]}
        onSubmit={onSubmit}
        onCancel={vi.fn()}
      />
    ));
    const composer = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );

    await press(composer, "ArrowUp");
    expect(composer.value).toBe("review this file");
    expect(container.querySelector(".attachment-list")?.textContent)
      .toContain("Applicationlink");
    expect(container.querySelector(".attachment-list")?.textContent)
      .toContain("file:///workspace/note.mdcontext");
    expect(container.querySelector(".composer-bar")?.textContent)
      .toContain("2 of 2 previous prompts");

    await press(composer, "ArrowUp");
    expect(composer.value).toBe("inspect the image");
    expect(container.querySelector(".attachment-list")?.textContent)
      .toContain("file:///workspace/pixel.pngimage · 3 B");
    await press(composer, "ArrowDown");
    expect(composer.value).toBe("review this file");

    await press(composer, "Enter");
    expect(onSubmit).toHaveBeenCalledWith(
      "review this file",
      [latest[0], latest[2]],
      latest,
    );
    expect(composer.value).toBe("");
    expect(container.querySelector(".attachment-list")).toBeNull();
  });

  it("keeps ordinary draft cursor movement separate from prompt history", async () => {
    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        commands={[]}
        history={[[{ type: "text", text: "previous" }]]}
        onSubmit={vi.fn()}
        onCancel={vi.fn()}
      />
    ));
    const composer = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );
    await replaceText(composer, "current\nmultiline draft");
    await press(composer, "ArrowUp");
    expect(composer.value).toBe("current\nmultiline draft");
    expect(container.querySelector(".composer-bar")?.textContent)
      .not.toContain("previous prompts");

    await replaceText(composer, "");
    await press(composer, "ArrowUp");
    expect(composer.value).toBe("previous");
    await press(composer, "ArrowDown");
    expect(composer.value).toBe("");
  });

  it("toggles Zed-style full message-editor mode without replacing the ACP draft", async () => {
    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        capabilities={{ embeddedContext: true }}
        commands={[{ name: "status", description: "Show status" }]}
        sessionControls={<button type="button">Agent mode</button>}
        onSubmit={vi.fn()}
        onCancel={vi.fn()}
      />
    ));
    const shell = requireElement<HTMLDivElement>(container.querySelector(".composer"));
    const editor = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );
    await replaceText(editor, "/");
    expect(container.querySelector('[role="listbox"][aria-label="Agent commands"]')).not.toBeNull();

    await press(editor, "Escape", { altKey: true, shiftKey: true });
    expect(shell.classList.contains("composer-expanded")).toBe(true);
    expect(shell.dataset.expanded).toBe("true");
    expect(editor.value).toBe("/");
    expect(document.activeElement).toBe(editor);
    expect(container.textContent).toContain("Agent mode");
    expect(container.querySelector('[role="listbox"][aria-label="Agent commands"]')).not.toBeNull();
    const collapse = requireElement<HTMLButtonElement>(
      container.querySelector('button[aria-label="Collapse message composer"]'),
    );
    expect(collapse.getAttribute("aria-pressed")).toBe("true");

    await press(editor, "Escape", { altKey: true, shiftKey: true });
    expect(shell.classList.contains("composer-expanded")).toBe(false);
    expect(editor.value).toBe("/");
    const expand = requireElement<HTMLButtonElement>(
      container.querySelector('button[aria-label="Expand message composer"]'),
    );
    await click(expand);
    await act(async () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve())));
    expect(shell.classList.contains("composer-expanded")).toBe(true);
    expect(document.activeElement).toBe(editor);

    await render(root, (
      <PromptComposer
        disabled={false}
        running
        capabilities={{ embeddedContext: true }}
        commands={[{ name: "status", description: "Show status" }]}
        sessionControls={<button type="button">Agent mode</button>}
        interactionPending
        onSubmit={vi.fn()}
        onCancel={vi.fn()}
      />
    ));
    expect(shell.classList.contains("composer-expanded")).toBe(false);
    expect(editor.value).toBe("/");
    expect(requireElement<HTMLButtonElement>(
      container.querySelector('button[aria-label="Expand message composer"]'),
    ).disabled).toBe(true);
  });

  it("surfaces Agent-reported ACP context usage beside the composer controls", async () => {
    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        commands={[]}
        usage={{
          used: 82_000,
          size: 100_000,
          cost: { amount: 1.2345, currency: "USD" },
        }}
        onSubmit={vi.fn()}
        onCancel={vi.fn()}
      />
    ));

    const trigger = requireElement<HTMLButtonElement>(
      container.querySelector('button[aria-label^="Context usage: 82%"]'),
    );
    expect(trigger.classList.contains("warning")).toBe(true);
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    await click(trigger);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    const details = requireElement(container.querySelector('[role="region"][aria-label="ACP context usage"]'));
    expect(details.textContent).toContain("82,000 tokens");
    expect(details.textContent).toContain("18,000 tokens");
    expect(details.textContent).toContain("1.2345 USD");
    expect(details.textContent).toContain("usage_update");

    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, key: "Escape" }));
    });
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(trigger);

    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        commands={[]}
        usage={{ used: 100_000, size: 100_000 }}
        onSubmit={vi.fn()}
        onCancel={vi.fn()}
      />
    ));
    expect(trigger.classList.contains("critical")).toBe(true);
    expect(trigger.getAttribute("aria-label")).toContain("Context usage: 100%");
  });

  it("retains a composer draft when the queue rejects a submission", async () => {
    await render(root, (
      <PromptComposer
        disabled={false}
        running
        commands={[]}
        onSubmit={() => false}
        onCancel={vi.fn()}
      />
    ));
    const composer = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );
    await replaceText(composer, "keep this draft");
    await press(composer, "Enter");
    expect(composer.value).toBe("keep this draft");
  });

  it("turns pasted and dropped files into negotiated ACP prompt attachments", async () => {
    const onSubmit = vi.fn();
    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        capabilities={{ image: true, embeddedContext: true }}
        commands={[]}
        onSubmit={onSubmit}
        onCancel={vi.fn()}
      />
    ));
    const composer = requireElement<HTMLDivElement>(container.querySelector(".composer"));
    const editor = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );

    await dispatchFileEvent(editor, "paste", [
      new File([new Uint8Array([1, 2, 3])], "clipboard.png", { type: "image/png" }),
    ]);
    expect(container.querySelector(".attachment-list")?.textContent)
      .toContain("clipboard.pngimage · 3 B");

    await dispatchFileEvent(composer, "dragenter", [
      new File(["project context"], "context.md", { type: "text/markdown" }),
    ]);
    expect(container.querySelector('[role="status"]')?.textContent)
      .toContain("Drop files to add context");
    await dispatchFileEvent(composer, "drop", [
      new File(["project context"], "context.md", { type: "text/markdown" }),
    ]);
    expect(container.querySelector('[role="status"]')).toBeNull();
    expect(container.querySelector(".attachment-list")?.textContent)
      .toContain("context.mdcontext · 15 B");

    await press(editor, "Enter");
    expect(onSubmit).toHaveBeenCalledWith("", [
      expect.objectContaining({
        type: "image",
        mimeType: "image/png",
        data: "AQID",
      }),
      expect.objectContaining({
        type: "resource",
        resource: expect.objectContaining({
          uri: "attyd://attachment/context.md",
          text: "project context",
        }),
      }),
    ]);
  });

  it("reports pasted files when the Agent did not negotiate attachment input", async () => {
    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        commands={[]}
        onSubmit={vi.fn()}
        onCancel={vi.fn()}
      />
    ));
    const editor = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );
    await dispatchFileEvent(editor, "paste", [
      new File([new Uint8Array([1])], "clipboard.png", { type: "image/png" }),
    ]);
    expect(container.querySelector(".attachment-error")?.textContent)
      .toContain("did not advertise");
    expect(container.querySelector(".attachment-list")).toBeNull();
  });

  it("waits for pasted files to become ACP blocks before sending", async () => {
    const onSubmit = vi.fn();
    let resolveBytes!: (value: ArrayBuffer) => void;
    const bytes = new Promise<ArrayBuffer>((resolve) => {
      resolveBytes = resolve;
    });
    const slowImage = new File([], "slow.png", { type: "image/png" });
    Object.defineProperty(slowImage, "arrayBuffer", { value: () => bytes });

    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        capabilities={{ image: true }}
        commands={[]}
        onSubmit={onSubmit}
        onCancel={vi.fn()}
      />
    ));
    const editor = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );

    await dispatchFileEvent(editor, "paste", [slowImage]);
    expect(container.querySelector('[role="status"]')?.textContent)
      .toContain("Preparing 1 file");
    expect(requireElement<HTMLButtonElement>(
      container.querySelector('button[aria-label="Send prompt"]'),
    ).disabled).toBe(true);

    await replaceText(editor, "inspect this");
    await press(editor, "Enter");
    expect(onSubmit).not.toHaveBeenCalled();
    expect(container.querySelector(".attachment-error")?.textContent)
      .toContain("finish preparing");

    await act(async () => {
      resolveBytes(new Uint8Array([1, 2, 3]).buffer);
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    expect(container.querySelector('[role="status"]')).toBeNull();
    expect(container.querySelector(".attachment-list")?.textContent).toContain("slow.png");

    await press(editor, "Enter");
    expect(onSubmit).toHaveBeenCalledWith("inspect this", [
      expect.objectContaining({ type: "image", data: "AQID" }),
    ]);
  });

  it("turns a Zed-style @ file mention into ACP embedded context", async () => {
    const onSubmit = vi.fn();
    const onSearchWorkspaceContext = vi.fn(async (query: string) => [{
      path: "/workspace/src/app.tsx",
      name: "app.tsx",
      relativePath: "src/app.tsx",
      rootName: "workspace",
      size: 18,
    }]);
    const onReadWorkspaceContext = vi.fn(async () => ({
      name: "src/app.tsx",
      size: 18,
      block: {
        type: "resource" as const,
        resource: {
          uri: "file:///workspace/src/app.tsx",
          mimeType: "text/typescript",
          text: "export default App",
        },
      },
    }));
    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        capabilities={{ embeddedContext: true }}
        commands={[]}
        onSearchWorkspaceContext={onSearchWorkspaceContext}
        onReadWorkspaceContext={onReadWorkspaceContext}
        onSubmit={onSubmit}
        onCancel={vi.fn()}
      />
    ));
    const editor = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );

    await replaceText(editor, "Review @app");
    await act(async () => new Promise((resolve) => setTimeout(resolve, 150)));
    expect(onSearchWorkspaceContext).toHaveBeenCalledWith("app");
    expect(container.querySelector('[role="listbox"][aria-label="Workspace context"]')?.textContent)
      .toContain("app.tsxsrc/app.tsx18 B");
    expect(editor.getAttribute("aria-activedescendant")).toBe("workspace-context-0");

    await press(editor, "Enter");
    await act(async () => new Promise((resolve) => setTimeout(resolve, 0)));
    expect(onReadWorkspaceContext).toHaveBeenCalledWith("/workspace/src/app.tsx");
    expect(editor.value).toBe("Review ");
    expect(container.querySelector(".attachment-list")?.textContent)
      .toContain("src/app.tsxcontext · 18 B");

    await replaceText(editor, "Review this file");
    await press(editor, "Enter");
    expect(onSubmit).toHaveBeenCalledWith("Review this file", [
      expect.objectContaining({
        type: "resource",
        resource: expect.objectContaining({
          uri: "file:///workspace/src/app.tsx",
          text: "export default App",
        }),
      }),
    ]);
  });

  it("offers bounded queued-message editing, removal, clearing, and Send now actions", async () => {
    const prompts: QueuedPrompt[] = [
      {
        id: "queued-1",
        sessionId: "session",
        blocks: [{ type: "text", text: "First follow-up" }],
      },
      {
        id: "queued-2",
        sessionId: "session",
        blocks: [
          { type: "text", text: "Second follow-up" },
          { type: "resource_link", uri: "file:///workspace/a.ts", name: "a.ts" },
        ],
      },
    ];
    const onEdit = vi.fn();
    const onRemove = vi.fn();
    const onClear = vi.fn();
    const onSendNow = vi.fn();
    await render(root, (
      <QueuedPrompts
        prompts={prompts}
        canSendNow
        onEdit={onEdit}
        onRemove={onRemove}
        onClear={onClear}
        onSendNow={onSendNow}
      />
    ));

    expect(container.querySelector('[aria-label="Queued messages"]')?.textContent)
      .toContain("2 queued");
    expect(container.querySelector('[aria-label="Queued message 2"]')?.textContent)
      .toContain("a.ts");
    await click(requireElement(container.querySelector('[aria-label="Edit queued message 1"]')));
    await click(requireElement(container.querySelector('[aria-label="Send queued message 2 now"]')));
    await click(requireElement(container.querySelector('[aria-label="Remove queued message 1"]')));
    await click(requireElement(container.querySelector('[aria-label="Clear queued messages"]')));
    expect(onEdit).toHaveBeenCalledWith(prompts[0]);
    expect(onSendNow).toHaveBeenCalledWith("queued-2");
    expect(onRemove).toHaveBeenCalledWith("queued-1");
    expect(onClear).toHaveBeenCalledOnce();
  });

  it("routes Zed-style thread navigation shortcuts from the composer", async () => {
    const onNavigateThread = vi.fn();
    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        commands={[]}
        onSubmit={vi.fn()}
        onCancel={vi.fn()}
        onNavigateThread={onNavigateThread}
      />
    ));
    const composer = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );

    await press(composer, "Home", { ctrlKey: true });
    await press(composer, "PageDown", { ctrlKey: true });
    await press(composer, "ArrowUp", { ctrlKey: true, altKey: true });
    await press(composer, "PageDown", { ctrlKey: true, altKey: true });

    expect(onNavigateThread.mock.calls.map(([target]) => target)).toEqual([
      "top",
      "page-down",
      "previous-message",
      "next-prompt",
    ]);
  });

  it("makes a stopped ACP connection recoverable from the disabled composer", async () => {
    const onReconnect = vi.fn();
    const onSubmit = vi.fn();
    await render(root, (
      <PromptComposer
        disabled
        running={false}
        commands={[]}
        connectionRecovery={{
          message: "The ACP connection stopped. Reconnect before continuing.",
          onReconnect,
        }}
        onSubmit={onSubmit}
        onCancel={vi.fn()}
      />
    ));

    const composer = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );
    expect(composer.disabled).toBe(true);
    expect(container.querySelector(".composer-reconnect")?.textContent)
      .toContain("Agent connection unavailable");
    await click(requireElement(container.querySelector(".composer-reconnect button")));
    expect(onReconnect).toHaveBeenCalledOnce();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("restores a prior ACP prompt into the composer for Zed-style editing", async () => {
    const onSubmit = vi.fn();
    await render(root, (
      <PromptComposer
        disabled={false}
        running={false}
        commands={[]}
        draft={{
          id: "draft-1",
          blocks: [
            { type: "text", text: "Revise this task" },
            { type: "resource_link", uri: "file:///workspace/a.ts", name: "a.ts" },
          ],
        }}
        onSubmit={onSubmit}
        onCancel={vi.fn()}
      />
    ));

    const composer = requireElement<HTMLTextAreaElement>(
      container.querySelector('textarea[role="combobox"]'),
    );
    expect(composer.value).toBe("Revise this task");
    expect(document.activeElement).toBe(composer);
    expect(container.querySelector(".attachment-list")?.textContent).toContain("a.ts");
    await press(composer, "Enter");
    expect(onSubmit).toHaveBeenCalledWith(
      "Revise this task",
      [{ type: "resource_link", uri: "file:///workspace/a.ts", name: "a.ts" }],
      [
        { type: "text", text: "Revise this task" },
        { type: "resource_link", uri: "file:///workspace/a.ts", name: "a.ts" },
      ],
    );
  });

  it("offers message actions without conflating them with ACP metadata", async () => {
    const onReusePrompt = vi.fn();
    const userBlocks: ContentBlock[] = [{ type: "text", text: "Original task" }];
    await render(root, (
      <Conversation
        canReusePrompt
        onReusePrompt={onReusePrompt}
        timeline={[
          {
            id: "user",
            type: "message",
            role: "protocol-user",
            blocks: userBlocks,
            messageId: "user-message-id",
            raw: [{ update: { sessionUpdate: "user_message_chunk" } }],
          },
          {
            id: "agent",
            type: "assistant",
            chunks: [
              {
                id: "agent-chunk",
                role: "agent",
                blocks: [{ type: "text", text: "Answer" }],
                messageId: "router-message-id",
                raw: [{ update: { sessionUpdate: "agent_message_chunk" } }],
              },
              { id: "thought-chunk", role: "thought", blocks: [{ type: "text", text: "Follow-up thought" }], raw: [] },
            ],
          },
        ]}
      />
    ));

    const edit = requireElement<HTMLButtonElement>(
      container.querySelector('button[aria-label="Edit and resend user message"]'),
    );
    await click(edit);
    expect(onReusePrompt).toHaveBeenCalledWith(userBlocks);
    const copy = requireElement<HTMLButtonElement>(
      container.querySelector('button[aria-label="Copy agent response"]'),
    );
    expect(copy.closest(".assistant-chunk")).not.toBeNull();
    expect(copy.closest(".thinking-block")).toBeNull();
    expect(edit.textContent?.trim()).toBe("");
    expect(copy.textContent?.trim()).toBe("");

    const messageInfos = [...container.querySelectorAll<HTMLButtonElement>(
      'button[aria-label="Message info"]',
    )];
    expect(messageInfos).toHaveLength(2);
    expect(messageInfos.every((info) => info.textContent?.trim() === "")).toBe(true);
    const userMessage = requireElement<HTMLElement>(container.querySelector(".message-user"));
    const answer = requireElement<HTMLElement>(container.querySelector(".assistant-chunk"));
    expect(userMessage.querySelector(".debug-info-panel")?.textContent)
      .toContain("user-message-id");
    expect(answer.querySelector(".debug-info-panel")?.textContent)
      .toContain("router-message-id");
    expect(userMessage.querySelector(".debug-info-panel")?.textContent)
      .toContain("Message events");
    expect([...requireElement(userMessage.querySelector(".message-meta-actions")).children].map(
      (element) => element.getAttribute("aria-label"),
    )).toEqual(["Message info", "Edit and resend user message"]);
    expect([...requireElement(answer.querySelector(".message-meta-actions")).children].map(
      (element) => element.getAttribute("aria-label"),
    )).toEqual(["Message info", "Copy agent response"]);
    expect(userMessage.querySelector(".debug-info-panel")?.hasAttribute("hidden")).toBe(true);
    await click(messageInfos[0]!);
    expect(messageInfos[0]?.getAttribute("aria-expanded")).toBe("true");
    expect(userMessage.querySelector(".debug-info-panel")?.hasAttribute("hidden")).toBe(false);
    expect(userMessage.querySelector(".debug-info-panel .raw-json")).toBeNull();
    expect(userMessage.querySelector(".debug-info-panel pre")?.textContent)
      .toContain("user_message_chunk");

    const thinking = requireElement<HTMLElement>(container.querySelector(".thinking-header"));
    await act(async () => {
      thinking.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
    });
    expect(document.querySelector('[role="menu"][aria-label="Agent response actions"]')).toBeNull();
  });

  it("provides the Zed-style Agent response context menu with keyboard access", async () => {
    const onNavigateThread = vi.fn();
    const onOpenThreadMarkdown = vi.fn();
    const writeText = vi.fn(async () => undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });
    await render(root, (
      <Conversation
        timeline={[{
          id: "agent",
          type: "assistant",
          chunks: [{
            id: "agent-chunk",
            role: "agent",
            blocks: [{ type: "text", text: "**Answer**" }],
            raw: [],
          }],
        }]}
        onNavigateThread={onNavigateThread}
        onOpenThreadMarkdown={onOpenThreadMarkdown}
      />
    ));

    const response = requireElement<HTMLElement>(container.querySelector(".assistant-entry"));
    const content = requireElement<HTMLElement>(response.querySelector(".message-content"));
    const selection = window.getSelection();
    const range = document.createRange();
    range.selectNodeContents(content);
    selection?.removeAllRanges();
    selection?.addRange(range);

    await act(async () => {
      content.dispatchEvent(new MouseEvent("contextmenu", {
        bubbles: true,
        cancelable: true,
        clientX: 40,
        clientY: 50,
      }));
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
    });
    let menu = requireElement<HTMLElement>(document.querySelector('[role="menu"][aria-label="Agent response actions"]'));
    expect(menu.textContent).toContain("Copy Selection");
    expect(document.activeElement?.getAttribute("role")).toBe("menuitem");
    await click(buttonWithText(menu, "Copy Selection"));
    expect(writeText).toHaveBeenCalledWith("Answer");

    selection?.removeAllRanges();
    await openResponseMenu(response);
    menu = requireElement(document.querySelector('[role="menu"][aria-label="Agent response actions"]'));
    expect(menu.textContent).not.toContain("Copy Selection");
    await click(buttonWithText(menu, "Copy This Agent Response"));
    expect(writeText).toHaveBeenLastCalledWith("**Answer**");

    await openResponseMenu(response);
    menu = requireElement(document.querySelector('[role="menu"][aria-label="Agent response actions"]'));
    await click(buttonWithText(menu, "Scroll to Top"));
    expect(onNavigateThread).toHaveBeenCalledWith("top");

    await openResponseMenu(response);
    menu = requireElement(document.querySelector('[role="menu"][aria-label="Agent response actions"]'));
    await click(buttonWithText(menu, "Open Thread as Markdown"));
    expect(onOpenThreadMarkdown).toHaveBeenCalledOnce();

    response.focus();
    await press(response, "F10", { shiftKey: true });
    await act(async () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve())));
    expect(document.querySelector('[role="menu"][aria-label="Agent response actions"]')).not.toBeNull();
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, key: "Escape" }));
    });
    expect(document.querySelector('[role="menu"][aria-label="Agent response actions"]')).toBeNull();
    expect(document.activeElement).toBe(response);
  });

  it("uses searchable ACP config pickers and suppresses the legacy mode duplicate", async () => {
    const onConfig = vi.fn();
    await render(root, (
      <SessionControls
        modes={{
          currentModeId: "auto",
          availableModes: [
            { id: "auto", name: "Auto (legacy)" },
            { id: "chat", name: "Chat (legacy)" },
          ],
        }}
        currentMode="auto"
        options={[
          {
            type: "select",
            id: "provider",
            name: "Provider",
            currentValue: "chatgpt_codex",
            options: [
              { value: "goose", name: "Goose" },
              { value: "anthropic", name: "Anthropic" },
              { value: "chatgpt_codex", name: "ChatGPT Codex" },
              { value: "codex_cli", name: "Codex CLI" },
              { value: "gemini", name: "Gemini" },
              { value: "openai", name: "OpenAI" },
              { value: "openrouter", name: "OpenRouter" },
              { value: "ollama", name: "Ollama" },
            ],
          },
          {
            type: "select",
            id: "mode",
            name: "Mode",
            category: "mode",
            currentValue: "auto",
            options: [
              { value: "auto", name: "Auto" },
              { value: "chat", name: "Chat" },
            ],
          },
        ]}
        disabled={false}
        onMode={vi.fn()}
        onConfig={onConfig}
      />
    ));

    const controls = requireElement(container.querySelector('[aria-label="Session controls"]'));
    expect([...controls.querySelectorAll("button")].filter((button) => button.textContent?.includes("Mode")))
      .toHaveLength(1);

    const provider = requireElement<HTMLButtonElement>(
      controls.querySelector('button[aria-controls="config-options-provider"]'),
    );
    await click(provider);
    const search = requireElement<HTMLInputElement>(
      controls.querySelector('input[aria-label="Search Provider"]'),
    );
    expect(document.activeElement).toBe(search);
    await replaceInput(search, "codex");
    expect([...controls.querySelectorAll('[role="option"]')].map((option) => option.textContent))
      .toEqual(["ChatGPT Codex", "Codex CLI"]);

    await click(requireElement([...controls.querySelectorAll('[role="option"]')]
      .find((option) => option.textContent === "Codex CLI")));
    expect(onConfig).toHaveBeenCalledWith("provider", "codex_cli");
    expect(document.activeElement).toBe(provider);
  });

  it("focuses transient Agent interactions and restores the previous control", async () => {
    const previous = document.createElement("button");
    previous.textContent = "Previous control";
    document.body.append(previous);
    previous.focus();
    const onPermission = vi.fn();

    await render(root, (
      <PermissionCard
        pending={{
          permissionId: "permission",
          request: {
            sessionId: "session",
            toolCall: { toolCallId: "tool", title: "Inspect workspace" },
            options: [
              { optionId: "allow", name: "Allow once", kind: "allow_once" },
              { optionId: "reject", name: "Reject", kind: "reject_once" },
            ],
          },
        }}
        onRespond={onPermission}
      />
    ));
    expect(document.activeElement?.textContent).toBe("Allow once");
    await click(buttonWithText(container, "Allow once"));
    expect(onPermission).toHaveBeenCalledWith({
      outcome: "selected",
      optionId: "allow",
    });
    await render(root, <></>);
    expect(document.activeElement).toBe(previous);

    const onElicitation = vi.fn();
    await render(root, (
      <ElicitationCard
        pending={{
          elicitationId: "elicitation",
          request: {
            requestId: null,
            mode: "form",
            message: "Configure request",
            requestedSchema: {
              type: "object",
              properties: {
                name: { type: "string", title: "Name" },
              },
            },
          },
        }}
        onRespond={onElicitation}
      />
    ));
    const name = requireElement<HTMLInputElement>(
      container.querySelector('input[type="text"]'),
    );
    expect(document.activeElement).toBe(name);
    await replaceInput(name, "Ada");
    await click(buttonWithText(container, "Submit"));
    expect(onElicitation).toHaveBeenCalledWith({
      action: "accept",
      content: { name: "Ada" },
    });
    await render(root, <></>);
    expect(document.activeElement).toBe(previous);
    previous.remove();
  });

  it("shows the permission subject and locks transient responses while awaiting acknowledgement", async () => {
    const onPermission = vi.fn();
    await render(root, (
      <PermissionCard
        pending={{
          permissionId: "permission",
          responseRequestId: "response",
          responseError: "Previous response failed",
          request: {
            sessionId: "session",
            toolCall: { toolCallId: "tool", title: "Write settings" },
            options: [{ optionId: "allow", name: "Allow once", kind: "allow_once" }],
          },
        }}
        toolCall={{
          toolCallId: "tool",
          title: "Write settings",
          name: "write_file",
          kind: "edit",
          locations: [{ path: "/workspace/settings.json", line: 4 }],
          rawInput: { path: "/workspace/settings.json", value: "safe" },
        }}
        onRespond={onPermission}
      />
    ));
    expect(container.querySelector('[data-testid="permission-subject"]')?.textContent)
      .toContain("/workspace/settings.json:4");
    expect(container.textContent).toContain("write_file");
    expect(container.textContent).toContain('"value": "safe"');
    expect(container.querySelector(".permission-subject .raw-json")?.hasAttribute("open"))
      .toBe(true);
    expect(container.textContent).toContain("Previous response failed");
    expect([...container.querySelectorAll("button")].every((button) => button.disabled)).toBe(true);
    await press(requireElement(container.querySelector('[role="alertdialog"]')), "Escape");
    expect(onPermission).not.toHaveBeenCalled();

    const onElicitation = vi.fn();
    await render(root, (
      <ElicitationCard
        pending={{
          elicitationId: "form",
          responseRequestId: "form-response",
          request: {
            requestId: "agent-request",
            mode: "form",
            message: "Configure tool",
            requestedSchema: {
              type: "object",
              properties: { value: { type: "string", title: "Value" } },
            },
          },
        }}
        onRespond={onElicitation}
      />
    ));
    expect(requireElement<HTMLInputElement>(container.querySelector('input[type="text"]')).disabled)
      .toBe(true);
    expect([...container.querySelectorAll("button")].every((button) => button.disabled)).toBe(true);
    await press(requireElement(container.querySelector('[role="dialog"]')), "Escape");
    expect(onElicitation).not.toHaveBeenCalled();
  });

  it("maps Escape to the protocol cancellation outcomes", async () => {
    const onPermission = vi.fn();
    await render(root, (
      <PermissionCard
        pending={{
          permissionId: "permission",
          request: {
            sessionId: "session",
            toolCall: { toolCallId: "tool", title: "Run command" },
            options: [{ optionId: "allow", name: "Allow once", kind: "allow_once" }],
          },
        }}
        onRespond={onPermission}
      />
    ));
    await press(requireElement(container.querySelector('[role="alertdialog"]')), "Escape");
    expect(onPermission).toHaveBeenCalledWith({ outcome: "cancelled" });

    const onElicitation = vi.fn();
    await render(root, (
      <ElicitationCard
        pending={{
          elicitationId: "form",
          request: {
            requestId: "agent-request",
            mode: "form",
            message: "Configure tool",
            requestedSchema: { type: "object", properties: {} },
          },
        }}
        onRespond={onElicitation}
      />
    ));
    await press(requireElement(container.querySelector('[role="dialog"]')), "Escape");
    expect(onElicitation).toHaveBeenCalledWith({ action: "cancel" });
  });

  it("unwinds focus correctly when Agent interactions are stacked", async () => {
    const previous = document.createElement("button");
    previous.textContent = "Composer stand-in";
    document.body.append(previous);
    previous.focus();
    const permission = (
      <PermissionCard
        key="permission"
        pending={{
          permissionId: "permission",
          request: {
            sessionId: "session",
            toolCall: { toolCallId: "tool", title: "Run tool" },
            options: [{ optionId: "allow", name: "Allow once", kind: "allow_once" }],
          },
        }}
        onRespond={() => undefined}
      />
    );
    const elicitation = (
      <ElicitationCard
        key="elicitation"
        pending={{
          elicitationId: "elicitation",
          request: {
            requestId: "request",
            mode: "form",
            message: "Configure tool",
            requestedSchema: {
              type: "object",
              properties: { value: { type: "string", title: "Value" } },
            },
          },
        }}
        onRespond={() => undefined}
      />
    );

    await render(root, <>{permission}{elicitation}</>);
    expect(document.activeElement).toBe(
      requireElement(container.querySelector('input[type="text"]')),
    );
    await render(root, permission);
    expect(document.activeElement?.textContent).toBe("Allow once");
    await render(root, <></>);
    expect(document.activeElement).toBe(previous);
    previous.remove();
  });
});

async function render(root: Root, node: ReactNode): Promise<void> {
  await act(async () => root.render(node));
}

async function replaceText(element: HTMLTextAreaElement, value: string): Promise<void> {
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(
      HTMLTextAreaElement.prototype,
      "value",
    )?.set;
    setter?.call(element, value);
    element.dispatchEvent(new InputEvent("input", { bubbles: true, data: value }));
  });
}

async function replaceInput(element: HTMLInputElement, value: string): Promise<void> {
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    )?.set;
    setter?.call(element, value);
    element.dispatchEvent(new InputEvent("input", { bubbles: true, data: value }));
  });
}

async function press(
  element: HTMLElement,
  key: string,
  init: Pick<KeyboardEventInit, "ctrlKey" | "altKey" | "shiftKey" | "metaKey"> = {},
): Promise<void> {
  await act(async () => {
    element.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, key, ...init }));
  });
}

async function click(element: Element): Promise<void> {
  await act(async () => {
    element.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  });
}

async function focus(element: Element): Promise<void> {
  await act(async () => {
    element.dispatchEvent(new FocusEvent("focusin", { bubbles: true }));
  });
}

async function openResponseMenu(response: HTMLElement): Promise<void> {
  const target = response.querySelector<HTMLElement>(".assistant-chunk") ?? response;
  await act(async () => {
    target.dispatchEvent(new MouseEvent("contextmenu", {
      bubbles: true,
      cancelable: true,
      clientX: 40,
      clientY: 50,
    }));
    await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
  });
}

async function dispatchFileEvent(
  element: Element,
  type: "paste" | "dragenter" | "drop",
  files: File[],
): Promise<void> {
  await act(async () => {
    const event = new Event(type, { bubbles: true, cancelable: true });
    const transfer = { files, types: ["Files"], dropEffect: "none" };
    Object.defineProperty(event, type === "paste" ? "clipboardData" : "dataTransfer", {
      value: transfer,
    });
    element.dispatchEvent(event);
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

function buttonWithText(container: ParentNode, text: string): HTMLButtonElement {
  const button = [...container.querySelectorAll<HTMLButtonElement>("button")]
    .find((candidate) => candidate.textContent?.trim() === text);
  if (!button) throw new Error(`Missing button: ${text}`);
  return button;
}

function requireElement<T extends Element>(element: T | null): T {
  if (!element) throw new Error("Expected UI element was not rendered");
  return element;
}
