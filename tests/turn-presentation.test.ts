import type { ContentBlock } from "@agentclientprotocol/sdk";
import { describe, expect, it } from "vitest";
import type { AssistantMessageChunk, TimelineItem } from "../web/src/lib/state";
import { splitTurnPresentation } from "../web/src/lib/turn-presentation";

function chunk(id: string, role: "agent" | "thought", blocks?: ContentBlock[]): AssistantMessageChunk {
  return {
    id,
    role,
    blocks: blocks ?? [{ type: "text", text: id }],
    messageId: `${id}-message`,
    raw: [{ id }],
  };
}

function assistant(id: string, ...chunks: AssistantMessageChunk[]): TimelineItem {
  return { id, type: "assistant", chunks };
}

const prompt: TimelineItem = {
  id: "prompt",
  type: "message",
  role: "user",
  blocks: [{ type: "text", text: "Inspect the project." }],
  raw: [],
};

const tool: TimelineItem = {
  id: "tool",
  type: "tool",
  call: { toolCallId: "inspect", title: "Inspect files", kind: "read", status: "completed" },
  raw: [],
};

describe("completed turn presentation", () => {
  it("keeps the prompt and final answer visible while retaining the entire execution process", () => {
    const first = assistant("first", chunk("progress", "agent"), chunk("thinking", "thought"));
    const final = assistant("final", chunk("answer", "agent"));
    const stop: TimelineItem = { id: "stop", type: "stop", response: { stopReason: "end_turn" } };

    const presentation = splitTurnPresentation([prompt, first, tool, final, stop]);

    expect(presentation.prompts).toEqual([prompt]);
    expect(presentation.output).toBe(final);
    expect(presentation.process).toEqual([first, tool]);
    expect(presentation.outcomes).toEqual([stop]);
  });

  it("separates earlier output and thoughts in the same assistant item from its final answer", () => {
    const chunks = [
      chunk("thought-before", "thought"),
      chunk("progress", "agent"),
      chunk("answer", "agent"),
      chunk("thought-after", "thought"),
    ];
    const laterThought = assistant("later", chunk("final-thought", "thought"));
    const presentation = splitTurnPresentation([assistant("mixed", ...chunks), laterThought]);

    expect(presentation.output?.chunks).toEqual([chunks[2]]);
    expect(presentation.process).toEqual([
      { id: "mixed:process", type: "assistant", chunks: [chunks[0], chunks[1], chunks[3]] },
      laterThought,
    ]);
    const visibleIds = [presentation.output!.id, ...presentation.process.map((item) => item.id)];
    expect(new Set(visibleIds).size).toBe(visibleIds.length);
  });

  it("preserves every block and ACP metadata in a multimodal final answer", () => {
    const blocks: ContentBlock[] = [
      { type: "text", text: "See the diagram and source." },
      { type: "image", data: "aW1hZ2U=", mimeType: "image/png" },
      { type: "audio", data: "YXVkaW8=", mimeType: "audio/wav" },
      { type: "resource_link", name: "Source", uri: "file:///workspace/source.ts" },
      { type: "resource", resource: { uri: "urn:result", text: "Full result", mimeType: "text/plain" } },
    ];
    const answer = chunk("answer", "agent", blocks);
    const presentation = splitTurnPresentation([
      assistant("mixed", chunk("thinking", "thought"), answer),
    ]);

    expect(presentation.output?.chunks).toEqual([answer]);
    expect(presentation.output?.chunks[0]).toBe(answer);
    expect(presentation.output?.chunks[0].blocks).toBe(blocks);
  });

  it("does not invent an answer when the agent only emitted execution activity", () => {
    const thought = assistant("thinking", chunk("thought", "thought"));
    const plan: TimelineItem = {
      id: "plan",
      type: "plan",
      update: { sessionUpdate: "plan", entries: [{ content: "Inspect files", priority: "medium", status: "completed" }] },
      raw: [],
    };
    const compaction: TimelineItem = {
      id: "compaction",
      type: "compaction",
      compactionId: "compact-1",
      status: "completed",
      blocks: [],
      raw: [],
    };
    const protocol: TimelineItem = {
      id: "protocol",
      type: "protocol",
      notification: { sessionId: "session", update: { sessionUpdate: "current_mode_update", currentModeId: "code" } },
    };
    const process = [thought, plan, tool, compaction, protocol];
    const presentation = splitTurnPresentation([prompt, ...process]);

    expect(presentation.output).toBeUndefined();
    expect(presentation.prompts).toEqual([prompt]);
    expect(presentation.process).toEqual(process);
    expect(splitTurnPresentation([])).toEqual({ prompts: [], process: [], outcomes: [] });
  });

  it("retains cancellation and error context outside the collapsed process", () => {
    const canceled: TimelineItem = { id: "cancel", type: "stop", response: { stopReason: "cancelled" } };
    const error: TimelineItem = {
      id: "error",
      type: "error",
      message: "Agent disconnected",
      operation: "session/prompt",
      retryBlocks: [{ type: "text", text: "Try again." }],
    };
    const final = assistant("partial", chunk("partial-answer", "agent"));
    const presentation = splitTurnPresentation([prompt, tool, final, canceled, error]);

    expect(presentation.output).toBe(final);
    expect(presentation.process).toEqual([tool]);
    expect(presentation.outcomes).toEqual([canceled, error]);
    expect(splitTurnPresentation([prompt, tool, error]).outcomes).toEqual([error]);
  });

  it("keeps all user chunks from live and loaded prompts", () => {
    const historyPrompt: TimelineItem = { ...prompt, id: "history-prompt", role: "protocol-user" };
    const presentation = splitTurnPresentation([prompt, historyPrompt, tool]);

    expect(presentation.prompts).toEqual([prompt, historyPrompt]);
    expect(presentation.process).toEqual([tool]);
  });

  it("leaves the original timeline intact for expansion, export, and search", () => {
    const items = [prompt, assistant("mixed", chunk("thinking", "thought"), chunk("answer", "agent")), tool];
    const original = structuredClone(items);
    function freeze(value: unknown): void {
      if (value == null || typeof value !== "object") return;
      for (const child of Object.values(value)) freeze(child);
      Object.freeze(value);
    }
    freeze(items);

    const presentation = splitTurnPresentation(items);

    expect(items).toEqual(original);
    expect(presentation.output).not.toBe(items[1]);
    expect(presentation.process[0]).not.toBe(items[1]);
    expect(presentation.process[1]).toBe(tool);
  });
});
