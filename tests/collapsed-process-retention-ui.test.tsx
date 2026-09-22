// @vitest-environment happy-dom

import { act, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Conversation } from "../web/src/components/acp/conversation";
import type { BridgeTurnProcessPage } from "../web/src/lib/business-api";
import { COLLAPSED_PROCESS_RETENTION_MS } from "../web/src/lib/process-retention";
import type { DeferredTurnProcess, TimelineItem } from "../web/src/lib/state";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

type Props = ComponentProps<typeof Conversation>;
type Loader = NonNullable<Props["onLoadTurnProcess"]>;
const process: DeferredTurnProcess = {
  turnId: "retained-turn",
  historyRevision: "history-7",
  processCount: 23,
  owner: { bridgeEpoch: "epoch", sessionId: "session", sessionIncarnation: 2 },
};

function timeline(lazy: boolean, completed = true): TimelineItem[] {
  const entries: TimelineItem[] = [{
    id: "prompt", type: "message", role: "user", blocks: [{ type: "text", text: "My request" }], raw: [],
    ...(lazy ? { deferredProcess: process } : { retainedProcess: { ...process, owner: { ...process.owner } } }),
  }];
  if (!lazy) entries.push({
    id: "tool", type: "tool", raw: [],
    call: { toolCallId: "tool", title: "Retained tool output", kind: "read", status: "completed" },
  });
  entries.push({ id: "answer", type: "assistant", chunks: [
    { id: "final", role: "agent", blocks: [{ type: "text", text: "Final answer stays visible" }], raw: [] },
  ] });
  if (completed) entries.push({ id: "stop", type: "stop", response: { stopReason: "end_turn" } });
  return entries;
}

function page(offset: number): BridgeTurnProcessPage {
  const end = Math.min(offset + 10, process.processCount);
  return {
    ...process.owner, turnId: process.turnId, historyRevision: process.historyRevision,
    offset, total: process.processCount, nextOffset: end === process.processCount ? null : end,
    items: Array.from({ length: end - offset }, (_, index) => [{
      sessionUpdate: "tool_call", toolCallId: `tool-${offset + index}`,
      title: `Process item ${offset + index + 1}`, kind: "read", status: "completed",
    }]),
    terminals: {},
  };
}

function pendingPage() {
  let resolve!: (value: BridgeTurnProcessPage) => void;
  const promise = new Promise<BridgeTurnProcessPage>((yes) => { resolve = yes; });
  return { promise, resolve };
}

describe("collapsed process memory retention", () => {
  let container: HTMLDivElement;
  let root: Root;
  let load: ReturnType<typeof vi.fn<Loader>>;
  let release: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    vi.useFakeTimers();
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    load = vi.fn<Loader>().mockImplementation(async (_descriptor, offset) => page(offset));
    release = vi.fn();
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  function trigger() { return container.querySelector<HTMLButtonElement>(".turn-process-trigger")!; }
  function more() { return container.querySelector<HTMLButtonElement>(".turn-process-load-more")!; }
  function tools() { return container.querySelectorAll(".tool-card"); }
  async function render(props: Partial<Props> = {}) {
    await act(async () => root.render(<Conversation
      timeline={timeline(true)} onLoadTurnProcess={load} onReleaseTurnProcess={release} {...props}
    />));
  }
  async function advance(milliseconds: number) {
    await act(async () => vi.advanceTimersByTimeAsync(milliseconds));
  }
  async function toggle() { await act(async () => trigger().click()); }

  it("keeps recently folded pages, releases them after five minutes, and reloads from offset zero", async () => {
    await render();
    await toggle();
    await act(async () => more().click());
    expect(tools()).toHaveLength(20);
    await toggle();
    await advance(COLLAPSED_PROCESS_RETENTION_MS - 1);
    expect(tools()).toHaveLength(20);
    await advance(1);
    expect(tools()).toHaveLength(0);
    expect(container.textContent).toContain("23 items");
    expect(container.textContent).toContain("Final answer stays visible");
    expect(container.textContent).not.toContain("20 of 23 loaded");
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 10]);
    await toggle();
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 10, 0]);
    expect(tools()).toHaveLength(10);
  });

  it("protects expanded content and restarts the retention period after each collapse", async () => {
    await render();
    await toggle();
    await advance(COLLAPSED_PROCESS_RETENTION_MS * 2);
    expect(tools()).toHaveLength(10);
    await toggle();
    await advance(COLLAPSED_PROCESS_RETENTION_MS - 1);
    await toggle();
    expect(load).toHaveBeenCalledTimes(1);
    await toggle();
    await advance(1);
    expect(tools()).toHaveLength(10);
    await advance(COLLAPSED_PROCESS_RETENTION_MS - 1);
    expect(tools()).toHaveLength(0);
  });

  it("aborts a hidden in-flight page and ignores its late response after the cache is released", async () => {
    await render();
    await toggle();
    const old = pendingPage();
    load.mockReturnValueOnce(old.promise);
    await act(async () => more().click());
    const oldSignal = load.mock.calls[1][2];
    await toggle();
    await advance(COLLAPSED_PROCESS_RETENTION_MS);
    expect(oldSignal.aborted).toBe(true);
    expect(tools()).toHaveLength(0);
    expect(container.querySelector('[role="status"]')).toBeNull();

    const fresh = pendingPage();
    load.mockReturnValueOnce(fresh.promise);
    await toggle();
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 10, 0]);
    await act(async () => old.resolve(page(10)));
    expect(tools()).toHaveLength(0);
    expect(container.querySelector('[role="status"]')).not.toBeNull();
    await act(async () => fresh.resolve(page(0)));
    expect(tools()).toHaveLength(10);
    expect(container.textContent).not.toContain("Process item 11");
  });

  it("does not extend retention when a request finishes while the process is folded", async () => {
    const pending = pendingPage();
    load.mockReturnValueOnce(pending.promise);
    await render();
    expect(vi.getTimerCount()).toBe(0);
    await toggle();
    await toggle();
    await advance(COLLAPSED_PROCESS_RETENTION_MS - 1);
    await act(async () => pending.resolve(page(0)));
    expect(tools()).toHaveLength(10);
    await advance(1);
    expect(tools()).toHaveLength(0);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("clears a hidden failure so reopening retries the first page automatically", async () => {
    load.mockRejectedValueOnce(new Error("offline"));
    await render();
    await toggle();
    expect(container.querySelector('[role="alert"]')).not.toBeNull();
    await toggle();
    await advance(COLLAPSED_PROCESS_RETENTION_MS);
    expect(container.querySelector('[role="alert"]')).toBeNull();
    await toggle();
    expect(load.mock.calls.map((call) => call[1])).toEqual([0, 0]);
    expect(tools()).toHaveLength(10);
  });

  it("does not postpone completed timeline release when the same scope is rerendered", async () => {
    await render({ timeline: timeline(false) });
    await advance(COLLAPSED_PROCESS_RETENTION_MS - 1);
    await render({ timeline: timeline(false) });
    await advance(1);
    expect(release).toHaveBeenCalledExactlyOnceWith(process);
    await advance(COLLAPSED_PROCESS_RETENTION_MS);
    expect(release).toHaveBeenCalledTimes(1);
  });

  it.each([false, true])("keeps the original deadline across history refreshes (operation identity: %s)", async (hasOperationId) => {
    const retained = { ...process, ...(hasOperationId ? { operationId: "operation-1" } : {}) };
    const initial = timeline(false);
    initial[0] = { ...initial[0], retainedProcess: retained };
    await render({ timeline: initial });
    await advance(COLLAPSED_PROCESS_RETENTION_MS - 1);
    const latest = {
      ...retained,
      historyRevision: "history-8",
      ...(hasOperationId ? { turnId: "rebased-history-turn" } : {}),
    };
    const refreshed = timeline(false);
    refreshed[0] = { ...refreshed[0], retainedProcess: latest };
    await render({ timeline: refreshed });
    await advance(1);
    expect(release).toHaveBeenCalledExactlyOnceWith(latest);
  });

  it("does not release a completed timeline while its process is explicitly expanded", async () => {
    await render({ timeline: timeline(false) });
    await toggle();
    await advance(COLLAPSED_PROCESS_RETENTION_MS * 2);
    expect(release).not.toHaveBeenCalled();
    await toggle();
    await advance(COLLAPSED_PROCESS_RETENTION_MS - 1);
    expect(release).not.toHaveBeenCalled();
    await advance(1);
    expect(release).toHaveBeenCalledExactlyOnceWith(process);
  });

  it("protects live turns and completed content being read until automatic folding is allowed", async () => {
    let following = false;
    const canAutoCollapse = () => following;
    await render({ timeline: timeline(false, false), atBottom: false, canAutoCollapse });
    await advance(COLLAPSED_PROCESS_RETENTION_MS * 2);
    expect(release).not.toHaveBeenCalled();
    expect(trigger()).toBeNull();
    await render({ timeline: timeline(false), atBottom: false, canAutoCollapse });
    await advance(COLLAPSED_PROCESS_RETENTION_MS * 2);
    expect(release).not.toHaveBeenCalled();
    expect(tools()[0].closest("[hidden]")).toBeNull();
    following = true;
    await render({ timeline: timeline(false), atBottom: true, canAutoCollapse });
    expect(trigger()).not.toBeNull();
    await advance(COLLAPSED_PROCESS_RETENTION_MS);
    expect(release).toHaveBeenCalledExactlyOnceWith(process);
  });

  it("clears the retention timer on unmount", async () => {
    await render({ timeline: timeline(false) });
    expect(vi.getTimerCount()).toBeGreaterThan(0);
    await act(async () => root.render(null));
    expect(vi.getTimerCount()).toBe(0);
    await advance(COLLAPSED_PROCESS_RETENTION_MS);
    expect(release).not.toHaveBeenCalled();
  });
});
