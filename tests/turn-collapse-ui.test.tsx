// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Conversation } from "../web/src/components/acp/conversation";
import type { TimelineItem } from "../web/src/lib/state";
import { scanThreadSearchDom } from "../web/src/components/acp/thread-search";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const live: TimelineItem[] = [
  { id: "prompt", type: "message", role: "user", blocks: [{ type: "text", text: "My request" }], raw: [] },
  { id: "assistant", type: "assistant", chunks: [
    { id: "progress", role: "agent", blocks: [{ type: "text", text: "Intermediate explanation" }], raw: [] },
    { id: "thought", role: "thought", blocks: [{ type: "text", text: "Private reasoning" }], raw: [] },
    { id: "answer", role: "agent", blocks: [{ type: "text", text: "Final answer" }], raw: [] },
  ] },
];
const stop: TimelineItem = { id: "stop", type: "stop", response: { stopReason: "end_turn" } };
const searchOptions = { caseSensitive: false, wholeWord: false, regex: false };

describe("completed turn presentation", () => {
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
    vi.restoreAllMocks();
  });

  it("keeps live messages visible and folds the process after completion without changing the timeline", async () => {
    const before = JSON.stringify(live);
    await act(async () => root.render(<Conversation timeline={live} />));
    expect(container.querySelector(".turn-process-trigger")).toBeNull();
    expect(scanThreadSearchDom(container, "Intermediate", searchOptions).matches).toHaveLength(1);
    await act(async () => root.render(<Conversation timeline={[...live, stop]} />));
    const button = container.querySelector<HTMLButtonElement>(".turn-process-trigger")!;
    expect(button.getAttribute("aria-expanded")).toBe("false");
    expect(container.querySelector(".turn-process-content")?.id).toBe(button.getAttribute("aria-controls"));
    expect(scanThreadSearchDom(container, "Intermediate", searchOptions).matches).toHaveLength(0);
    expect(scanThreadSearchDom(container, "Final answer", searchOptions).matches).toHaveLength(1);
    expect(container.querySelector(".message-user")?.closest("[hidden]")).toBeNull();
    expect(container.querySelector(".turn-stop")?.closest("[hidden]")).toBeNull();
    expect(JSON.stringify(live)).toBe(before);
  });

  it("preserves explicit expansion across later turns and copies only the final answer", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.spyOn(navigator.clipboard, "writeText").mockImplementation(writeText);
    const pauseFollowing = vi.fn();
    await act(async () => root.render(<Conversation timeline={[...live, stop]} onProcessToggle={pauseFollowing} />));
    const toggles = vi.fn();
    container.addEventListener("toggle", toggles, true);
    const button = container.querySelector<HTMLButtonElement>(".turn-process-trigger")!;
    await act(async () => button.click());
    expect(button.getAttribute("aria-expanded")).toBe("true");
    expect(pauseFollowing).toHaveBeenCalledOnce();
    expect(toggles).toHaveBeenCalledOnce();
    expect(scanThreadSearchDom(container, "Intermediate", searchOptions).matches).toHaveLength(1);
    await act(async () => root.render(<Conversation timeline={[
      ...live, stop,
      { id: "next", type: "message", role: "user", blocks: [{ type: "text", text: "Next request" }], raw: [] },
    ]} />));
    expect(container.querySelector(".turn-process-trigger")).toBe(button);
    expect(button.getAttribute("aria-expanded")).toBe("true");
    const output = container.querySelector('.conversation-turn > .assistant-entry')!;
    await act(async () => output.querySelector<HTMLButtonElement>('[aria-label="Copy agent response"]')!.click());
    expect(writeText).toHaveBeenCalledWith("Final answer");
  });

  it("defers a live turn's collapse while reading and folds upon returning to the bottom", async () => {
    let following = false;
    const canAutoCollapse = () => following;
    await act(async () => root.render(<Conversation timeline={live} atBottom={false} canAutoCollapse={canAutoCollapse} />));
    await act(async () => root.render(<Conversation timeline={[...live, stop]} atBottom={false} canAutoCollapse={canAutoCollapse} />));
    expect(container.querySelector(".turn-process-trigger")).toBeNull();
    expect(scanThreadSearchDom(container, "Intermediate", searchOptions).matches).toHaveLength(1);
    following = true;
    await act(async () => root.render(<Conversation timeline={[...live, stop]} atBottom canAutoCollapse={canAutoCollapse} />));
    expect(container.querySelector(".turn-process-trigger")?.getAttribute("aria-expanded")).toBe("false");
    expect(scanThreadSearchDom(container, "Intermediate", searchOptions).matches).toHaveLength(0);
  });

  it("folds loaded histories without stop events only when settled or followed by another prompt", async () => {
    await act(async () => root.render(<Conversation timeline={live} />));
    expect(container.querySelector(".turn-process-trigger")).toBeNull();
    await act(async () => root.render(<Conversation timeline={live} settled />));
    expect(container.querySelector(".turn-process-trigger")?.getAttribute("aria-expanded")).toBe("false");
    await act(async () => root.render(<Conversation key="history" timeline={[
      ...live,
      { id: "next", type: "message", role: "protocol-user", blocks: [], raw: [] },
    ]} />));
    expect(container.querySelectorAll(".turn-process-trigger")).toHaveLength(1);
  });

  it("keeps failures visible and execution details accessible when there is no agent answer", async () => {
    await act(async () => root.render(<Conversation timeline={[
      live[0],
      { id: "tool", type: "tool", call: { toolCallId: "tool", title: "Read files", status: "failed" }, raw: [] },
      { id: "failed", type: "error", operation: "session/prompt", message: "Agent disconnected", retryBlocks: [{ type: "text", text: "My request" }] },
    ]} onRetryPrompt={() => {}} />));
    const alert = container.querySelector('[role="alert"]')!;
    expect(alert.closest("[hidden]")).toBeNull();
    expect(alert.textContent).toContain("Agent disconnected");
    expect(container.querySelector(".assistant-entry")).toBeNull();
    expect(container.querySelector(".tool-card")?.closest("[hidden]")).not.toBeNull();
    await act(async () => container.querySelector<HTMLButtonElement>(".turn-process-trigger")!.click());
    expect(container.querySelector(".tool-card")?.closest("[hidden]")).toBeNull();
  });

  it("preserves an expanded message-only history when hydration regenerates entry IDs", async () => {
    const history = live.map((item): TimelineItem => item.type === "assistant"
      ? { ...item, chunks: item.chunks.map((chunk) => ({ ...chunk, messageId: `acp:${chunk.id}` })) }
      : item);
    await act(async () => root.render(<Conversation timeline={history} settled />));
    const button = container.querySelector<HTMLButtonElement>(".turn-process-trigger")!;
    await act(async () => button.click());
    await act(async () => root.render(<Conversation timeline={history.map((item) => ({
      ...item, id: `hydrated:${item.id}`,
    }))} settled />));
    expect(container.querySelector(".turn-process-trigger")).toBe(button);
    expect(button.getAttribute("aria-expanded")).toBe("true");
  });

  it("notifies visible thread search when a deferred answer-only turn finishes folding", async () => {
    const timeline: TimelineItem[] = [{
      id: "answer", type: "assistant", chunks: [{ id: "chunk", role: "agent", blocks: [{ type: "text", text: "Answer only" }], raw: [] }],
    }];
    await act(async () => root.render(<Conversation timeline={timeline} atBottom={false} />));
    await act(async () => root.render(<Conversation timeline={[...timeline, stop]} atBottom={false} />));
    const toggles = vi.fn();
    container.addEventListener("toggle", toggles, true);
    await act(async () => root.render(<Conversation timeline={[...timeline, stop]} atBottom />));
    expect(toggles).toHaveBeenCalledOnce();
    expect(scanThreadSearchDom(container, "Answer only", searchOptions).matches).toHaveLength(1);
  });
});
