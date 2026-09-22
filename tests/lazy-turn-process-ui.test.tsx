// @vitest-environment happy-dom

import { act, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Conversation } from "../web/src/components/acp/conversation";
import { RequestTimeoutError, type BridgeTurnProcessPage } from "../web/src/lib/business-api";
import { COLLAPSED_PROCESS_RETENTION_MS } from "../web/src/lib/process-retention";
import i18n from "../web/src/i18n";
import type { DeferredTurnProcess, TimelineItem } from "../web/src/lib/state";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

type Loader = NonNullable<ComponentProps<typeof Conversation>["onLoadTurnProcess"]>;
const process: DeferredTurnProcess = {
  turnId: "history-turn-1",
  historyRevision: "history-7",
  processCount: 23,
  owner: { bridgeEpoch: "epoch", sessionId: "session", sessionIncarnation: 2 },
};

function timeline(descriptor = process): TimelineItem[] {
  return [
    { id: "prompt", type: "message", role: "user", blocks: [{ type: "text", text: "My request" }], raw: [], deferredProcess: descriptor },
    { id: "answer", type: "assistant", chunks: [
      { id: "final", role: "agent", blocks: [{ type: "text", text: "Final answer stays visible" }], raw: [] },
    ] },
    { id: "stop", type: "stop", response: { stopReason: "end_turn" } },
  ];
}

function page(offset: number, descriptor = process): BridgeTurnProcessPage {
  const end = Math.min(offset + 10, descriptor.processCount);
  return {
    ...descriptor.owner,
    turnId: descriptor.turnId,
    historyRevision: descriptor.historyRevision,
    offset,
    total: descriptor.processCount,
    nextOffset: end === descriptor.processCount ? null : end,
    items: Array.from({ length: end - offset }, (_, index) => [{
      sessionUpdate: "tool_call",
      toolCallId: `tool-${offset + index}`,
      title: `Process item ${offset + index + 1}`,
      kind: "read",
      status: "completed",
    }]),
    terminals: {},
  };
}

function pendingPage() {
  let resolve!: (page: BridgeTurnProcessPage) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<BridgeTurnProcessPage>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

describe("deferred completed turn process", () => {
  let container: HTMLDivElement;
  let root: Root;
  let load: ReturnType<typeof vi.fn<Loader>>;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    load = vi.fn<Loader>().mockImplementation(async (descriptor, offset) => page(offset, descriptor));
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    await i18n.changeLanguage("en");
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  function trigger() { return container.querySelector<HTMLButtonElement>(".turn-process-trigger")!; }
  function more() { return container.querySelector<HTMLButtonElement>(".turn-process-load-more")!; }
  function tools() { return container.querySelectorAll(".tool-card"); }
  async function render(descriptor = process) {
    await act(async () => root.render(<Conversation timeline={timeline(descriptor)} settled onLoadTurnProcess={load} />));
  }

  it("sends no request while collapsed and loads exactly 10, 10, then the remainder on explicit clicks", async () => {
    await render();
    expect(load).not.toHaveBeenCalled();
    expect(tools()).toHaveLength(0);
    expect(container.querySelector(".turn-process-count")?.textContent).toBe("23 items");
    expect(container.querySelector(".assistant-entry")?.closest("[hidden]")).toBeNull();

    await act(async () => trigger().click());
    expect(load).toHaveBeenCalledTimes(1);
    expect(load.mock.calls[0][1]).toBe(0);
    expect(tools()).toHaveLength(10);
    expect(container.textContent).toContain("10 of 23 loaded");
    expect(more().textContent).toBe("Load 10 more");

    await act(async () => more().click());
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 10]);
    expect(tools()).toHaveLength(20);
    await act(async () => more().click());
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 10, 20]);
    expect(tools()).toHaveLength(23);
    expect(more()).toBeNull();
    expect(container.textContent).toContain("23 of 23 loaded");
    expect(container.textContent).toContain("Final answer stays visible");
  });

  it("keeps loaded pages when collapsed and reopened without refetching or draining remaining pages", async () => {
    await render();
    await act(async () => trigger().click());
    await act(async () => trigger().click());
    expect(tools()[0].closest("[hidden]")).not.toBeNull();
    await render();
    await act(async () => trigger().click());
    expect(load).toHaveBeenCalledTimes(1);
    expect(tools()).toHaveLength(10);
    expect(tools()[0].closest("[hidden]")).toBeNull();
  });

  it("retries a failed page at the same offset and retains already loaded details and the final answer", async () => {
    load.mockRejectedValueOnce(new Error("Temporary disconnect"));
    await render();
    await act(async () => trigger().click());
    expect(container.querySelector('[role="alert"]')?.textContent).toBe("Unable to load execution details.");
    expect(container.textContent).toContain("Final answer stays visible");
    expect(load).toHaveBeenCalledTimes(1);
    await act(async () => more().click());
    expect(tools()).toHaveLength(10);
    load.mockRejectedValueOnce(new Error("Second page failed"));
    await act(async () => more().click());
    expect(tools()).toHaveLength(10);
    expect(more().textContent).toBe("Retry loading");
    await act(async () => more().click());
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 0, 10, 10]);
    expect(tools()).toHaveLength(20);
    expect(container.querySelector('[role="alert"]')).toBeNull();
  });

  it("explains a first-page timeout and waits for an explicit retry at the same offset", async () => {
    vi.useFakeTimers();
    load.mockRejectedValueOnce(new RequestTimeoutError(30_000));
    await render();
    await act(async () => trigger().click());
    expect(container.querySelector('[role="alert"]')?.textContent).toBe("Loading execution details timed out. Retry when ready.");
    expect(container.querySelector('[role="status"]')).toBeNull();
    expect(container.textContent).toContain("Final answer stays visible");
    expect(more().textContent).toBe("Retry loading");
    await act(async () => vi.advanceTimersByTimeAsync(60_000));
    await render();
    await act(async () => trigger().click());
    await act(async () => trigger().click());
    expect(load).toHaveBeenCalledTimes(1);
    await act(async () => more().click());
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 0]);
    expect(tools()).toHaveLength(10);
    expect(container.querySelector('[role="alert"]')).toBeNull();
  });

  it("keeps successful pages and the final reply when the next page times out, then retries that page", async () => {
    await render();
    await act(async () => trigger().click());
    load.mockRejectedValueOnce(new RequestTimeoutError(30_000));
    await act(async () => more().click());
    expect(tools()).toHaveLength(10);
    expect(container.textContent).toContain("10 of 23 loaded");
    expect(container.textContent).toContain("Final answer stays visible");
    expect(container.querySelector('[role="alert"]')?.textContent).toContain("timed out");
    expect(more().disabled).toBe(false);
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 10]);
    await act(async () => more().click());
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 10, 10]);
    expect(tools()).toHaveLength(20);
    expect(container.querySelector('[role="alert"]')).toBeNull();
  });

  it("localizes the timeout explanation and retry action in Chinese", async () => {
    await i18n.changeLanguage("zh-CN");
    load.mockRejectedValueOnce(new RequestTimeoutError(30_000));
    await render();
    await act(async () => trigger().click());
    expect(container.querySelector('[role="alert"]')?.textContent).toBe("执行过程加载超时，请点击重试。");
    expect(more().textContent).toBe("重试加载");
  });

  it("clears a timeout after five minutes folded and starts a fresh first page on reopening", async () => {
    vi.useFakeTimers();
    await render();
    await act(async () => trigger().click());
    load.mockRejectedValueOnce(new RequestTimeoutError(30_000));
    await act(async () => more().click());
    expect(container.querySelector('[role="alert"]')?.textContent).toContain("timed out");
    await act(async () => trigger().click());
    await act(async () => vi.advanceTimersByTimeAsync(COLLAPSED_PROCESS_RETENTION_MS));
    expect(tools()).toHaveLength(0);
    expect(container.querySelector('[role="alert"]')).toBeNull();
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 10]);
    await act(async () => trigger().click());
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 10, 0]);
    expect(tools()).toHaveLength(10);
    expect(container.querySelector('[role="alert"]')).toBeNull();
  });

  it("allows only one request in flight even across rapid clicks and disclosure changes", async () => {
    const first = pendingPage();
    load.mockReturnValueOnce(first.promise);
    await render();
    await act(async () => trigger().click());
    await act(async () => trigger().click());
    await act(async () => trigger().click());
    expect(load).toHaveBeenCalledTimes(1);
    expect(container.querySelector('[role="status"]')?.textContent).toContain("Loading");
    await act(async () => first.resolve(page(0)));
    const second = pendingPage();
    load.mockReturnValueOnce(second.promise);
    await act(async () => { more().click(); more().click(); });
    expect(load).toHaveBeenCalledTimes(2);
    expect(more().disabled).toBe(true);
    await act(async () => second.resolve(page(10)));
    expect(tools()).toHaveLength(20);
  });

  it.each([
    ["session", { ...process, owner: { ...process.owner, sessionId: "other-session" } }],
    ["owner incarnation", { ...process, owner: { ...process.owner, sessionIncarnation: 3 } }],
    ["bridge epoch", { ...process, owner: { ...process.owner, bridgeEpoch: "new-epoch" } }],
    ["history revision", { ...process, historyRevision: "history-8" }],
    ["turn", { ...process, turnId: "history-turn-2" }],
  ] as const)("aborts and ignores stale pages when the %s changes", async (_name, replacement) => {
    const old = pendingPage();
    load.mockReturnValueOnce(old.promise);
    await render();
    await act(async () => trigger().click());
    const signal = load.mock.calls[0][2];
    await render(replacement);
    expect(signal.aborted).toBe(true);
    expect(trigger().getAttribute("aria-expanded")).toBe("false");
    await act(async () => old.resolve(page(0)));
    expect(tools()).toHaveLength(0);
    expect(load).toHaveBeenCalledTimes(1);
    await act(async () => trigger().click());
    expect(load.mock.calls[1][0]).toEqual(replacement);
    expect(tools()).toHaveLength(10);
  });

  it("aborts a pending request when the conversation is unmounted", async () => {
    const pending = pendingPage();
    load.mockReturnValueOnce(pending.promise);
    await render();
    await act(async () => trigger().click());
    const signal = load.mock.calls[0][2];
    await act(async () => root.render(null));
    expect(signal.aborted).toBe(true);
    await act(async () => pending.resolve(page(0)));
    expect(container.childElementCount).toBe(0);
  });

  it("rejects a mismatched response without hiding the final answer or poisoning the page cache", async () => {
    load.mockResolvedValueOnce({ ...page(0), historyRevision: "stale" });
    await render();
    await act(async () => trigger().click());
    expect(tools()).toHaveLength(0);
    expect(container.querySelector('[role="alert"]')).not.toBeNull();
    expect(container.textContent).toContain("Final answer stays visible");
    await act(async () => more().click());
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 0]);
    expect(tools()).toHaveLength(10);
  });

  it.each([
    ["missing entries", { ...page(0), items: page(0).items.slice(0, 9) }],
    ["premature end", { ...page(0), nextOffset: null }],
    ["skipped entry", { ...page(0), nextOffset: 11 }],
  ] as const)("keeps pagination retryable for a response with %s", async (_name, response) => {
    load.mockResolvedValueOnce(response);
    await render();
    await act(async () => trigger().click());
    expect(tools()).toHaveLength(0);
    expect(more().textContent).toBe("Retry loading");
    await act(async () => more().click());
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 0]);
    expect(tools()).toHaveLength(10);
  });

  it("computes change review only from pages the user has loaded", async () => {
    load.mockImplementation(async (descriptor, offset) => {
      const result = page(offset, descriptor);
      if (offset === 10) result.items[0] = [{
        sessionUpdate: "tool_call", toolCallId: "edit-file", title: "Edit file", kind: "edit", status: "completed",
        content: [{ type: "diff", path: "/work/only-after-next-page.ts", oldText: "before", newText: "after" }],
      }];
      return result;
    });
    await render();
    expect(container.textContent).not.toContain("only-after-next-page.ts");
    await act(async () => trigger().click());
    expect(container.textContent).not.toContain("only-after-next-page.ts");
    await act(async () => more().click());
    expect(container.textContent).toContain("only-after-next-page.ts");
    expect(container.querySelector(".change-review")).not.toBeNull();
  });
});
