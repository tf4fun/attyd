// @vitest-environment happy-dom

import type { SessionUpdate } from "@agentclientprotocol/sdk";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { REQUEST_TIMEOUT_MS, type BridgeSessionView, type BridgeTurnProcessPage } from "../web/src/lib/business-api";
import type { DeferredTurnProcess } from "../web/src/lib/state";
import { sessionPath } from "../web/src/lib/session-route";
import { useAcp } from "../web/src/lib/use-acp";

const owner = { bridgeEpoch: "epoch", sessionId: "history-session", sessionIncarnation: 3 };
const sessionUrl = `/api/v1/sessions/${owner.sessionId}`;
const cwd = "/work/history";
const historyRevision = "epoch:3:7";
const turnId = "turn/one";
const user: SessionUpdate = { sessionUpdate: "user_message_chunk", content: { type: "text", text: "Question" } };
const answer: SessionUpdate = { sessionUpdate: "agent_message_chunk", messageId: "answer", content: { type: "text", text: "Final answer" } };
const tool = (index: number): SessionUpdate => ({
  sessionUpdate: "tool_call", toolCallId: `tool-${index}`, title: `Intermediate work ${index}`,
  kind: "read", status: "completed", rawOutput: `Hidden process ${index}`,
});

function view(overrides: Partial<BridgeSessionView> = {}): BridgeSessionView {
  return {
    ...owner, viewRevision: 7, historyRevision, phase: "ready", syncError: null,
    timeline: [user, answer],
    collapsedTurns: [{ turnId, beforeUpdate: 0, afterUpdate: 2, processCount: 12, historyRevision, outcomes: [
      { operationId: turnId, afterUpdate: 2, response: { stopReason: "end_turn" } },
    ] }],
    activeTurn: null, workspace: { cwd, session: {} }, controls: {},
    interactions: { permissions: {}, elicitations: {}, urlFlows: {} }, operation: null, terminals: {},
    ...overrides,
  };
}

function fullView(): BridgeSessionView {
  return {
    ...view(), collapsedTurns: undefined,
    timeline: [user, ...Array.from({ length: 12 }, (_, index) => tool(index)), answer],
    turnOutcomes: [{ operationId: turnId, afterUpdate: 14, response: { stopReason: "end_turn" } }],
  };
}

function processPage(offset = 0): BridgeTurnProcessPage {
  const end = Math.min(offset + 10, 12);
  return { ...owner, turnId, historyRevision, offset, total: 12,
    nextOffset: end === 12 ? null : end,
    items: Array.from({ length: end - offset }, (_, index) => [tool(offset + index)]), terminals: {} };
}

function deferredResponse() {
  let finish!: (body: unknown, status?: number) => void;
  const promise = new Promise<Response>((resolve) => {
    finish = (body, status = 200) => resolve(response(body, status));
  });
  return { promise, finish };
}

function response(body: unknown, status = 200) { return new Response(JSON.stringify(body), { status }); }

describe("lazy process REST hook", () => {
  let root: Root;
  let container: HTMLDivElement;
  let acp: ReturnType<typeof useAcp>;
  let fetchMock: ReturnType<typeof vi.fn<typeof fetch>>;
  let currentView: BridgeSessionView;
  let readProcess: (url: URL) => Promise<Response>;
  let readFull: () => Promise<Response>;
  let readCompact: () => Promise<Response>;
  let startTurn: () => Promise<Response>;

  class TestEventSource {
    static CLOSED = 2;
    static instances: TestEventSource[] = [];
    readyState = 1;
    onopen: (() => void) | null = null;
    onmessage: ((event: { data: string }) => void) | null = null;
    onerror: (() => void) | null = null;
    constructor(readonly url: string) { TestEventSource.instances.push(this); }
    close() { this.readyState = TestEventSource.CLOSED; }
  }

  function Harness() { acp = useAcp(); return null; }
  function descriptor(): DeferredTurnProcess { return acp.state.timeline.find((item) => item.deferredProcess)!.deferredProcess!; }
  function source() { return TestEventSource.instances.find(({ url }) => url.split("?")[0] === `${sessionUrl}/events`)!; }
  function emitReset() { source().onmessage?.({ data: JSON.stringify({ type: "bridge/session_reset", ...owner, viewRevision: currentView.viewRevision }) }); }
  function sessionReads() { return fetchMock.mock.calls.filter(([path]) => String(path).split("?")[0] === sessionUrl); }
  async function mount(expectDeferred = true) {
    window.history.replaceState(null, "", sessionPath(owner.sessionId, cwd));
    await act(async () => root.render(createElement(Harness)));
    expect(acp.state.session?.sessionId).toBe(owner.sessionId);
    if (expectDeferred) expect(descriptor().processCount).toBe(12);
  }

  beforeEach(() => {
    (globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    currentView = view();
    readProcess = async (url) => response(processPage(Number(url.searchParams.get("offset"))));
    readFull = async () => response(fullView());
    readCompact = async () => response(currentView);
    startTurn = async () => response({ operationId: "accepted-turn", disposition: "accepted", status: "running" });
    TestEventSource.instances = [];
    vi.stubGlobal("EventSource", TestEventSource);
    fetchMock = vi.fn(async (input) => {
      const url = new URL(String(input), "http://attyd.test");
      if (url.pathname === "/api/v1/runtime") return response({
        connected: true, generation: 1, bridgeEpoch: owner.bridgeEpoch, hello: null,
        initialized: { type: "acp/initialized", response: { protocolVersion: 1,
          agentCapabilities: { loadSession: true, sessionCapabilities: { list: {} } } } },
        error: null, phase: { type: "bridge/phase", phase: "ready" },
      });
      if (url.pathname === "/api/v1/sessions") return response({ sessions: [{ sessionId: owner.sessionId, cwd }] });
      if (url.pathname.endsWith("/process")) return readProcess(url);
      if (url.pathname === `${sessionUrl}/turns`) return startTurn();
      if (url.pathname === sessionUrl) return url.searchParams.get("presentation") === "compact"
        ? readCompact() : readFull();
      throw new Error(`Unexpected request: ${String(input)}`);
    });
    vi.stubGlobal("fetch", fetchMock);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    window.history.replaceState(null, "", "/");
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("uses compact presentation for initial and reset reads while keeping the owner-fenced SSE URL unchanged", async () => {
    await mount();
    expect(acp.state.timeline.some((item) => item.type === "tool")).toBe(false);
    expect(fetchMock.mock.calls.some(([path]) => new URL(String(path), "http://attyd.test").pathname.endsWith("/process"))).toBe(false);
    currentView = view({ viewRevision: 8 });
    await act(async () => emitReset());
    expect(sessionReads()).toHaveLength(2);
    const queries = sessionReads().map(([path]) => new URL(String(path), "http://attyd.test").searchParams);
    expect(queries.map((query) => query.get("presentation"))).toEqual(["compact", "compact"]);
    expect(queries[1].get("expectedEpoch")).toBe(owner.bridgeEpoch);
    expect(queries[1].get("expectedIncarnation")).toBe(String(owner.sessionIncarnation));
    const stream = new URL(source().url, "http://attyd.test");
    expect(stream.searchParams.has("presentation")).toBe(false);
    expect(stream.searchParams.get("expectedEpoch")).toBe(owner.bridgeEpoch);
    expect(stream.searchParams.get("expectedIncarnation")).toBe(String(owner.sessionIncarnation));
  });

  it("requests one owner/history/offset page with cancellation support without mutating canonical state", async () => {
    await mount();
    const before = acp.state;
    const controller = new AbortController();
    const result = await acp.loadTurnProcess(descriptor(), 10, controller.signal);
    expect(result.items).toHaveLength(2);
    const [path, init] = fetchMock.mock.calls.at(-1)!;
    const url = new URL(String(path), "http://attyd.test");
    expect(url.pathname).toBe(`${sessionUrl}/turns/turn%2Fone/process`);
    expect(Object.fromEntries(url.searchParams)).toEqual({
      expectedEpoch: owner.bridgeEpoch, expectedIncarnation: "3", historyRevision, offset: "10",
    });
    expect(init?.signal?.aborted).toBe(false);
    expect(controller.signal.aborted).toBe(false);
    expect(acp.state).toBe(before);
  });

  function includedView(): BridgeSessionView {
    return { ...fullView(), viewRevision: 8, collapsedTurns: [{
      ...view().collapsedTurns![0], operationId: turnId, processIncluded: true,
      afterUpdate: 14, visibleRanges: [{ start: 0, end: 1 }, { start: 13, end: 14 }],
      outcomes: [{ operationId: turnId, afterUpdate: 14, response: { stopReason: "end_turn" } }],
    }] };
  }

  it("releases an observed canonical turn locally, excludes it on refresh, and reloads only its requested page", async () => {
    currentView = view({ phase: "running", timeline: [], collapsedTurns: [], activeTurn: {
      operationId: turnId, clientIntentId: "intent", prompt: [user.content], updates: [tool(0)], terminal: null,
    } });
    await mount(false);
    currentView = includedView();
    await act(async () => emitReset());
    const process = acp.state.timeline.find((item) => item.retainedProcess)!.retainedProcess!;
    expect(acp.state.timeline.filter((item) => item.type === "tool")).toHaveLength(12);
    const reads = sessionReads().length;
    await act(async () => acp.releaseTurnProcess(process));
    expect(sessionReads()).toHaveLength(reads);
    expect(acp.state.timeline.some((item) => item.type === "tool")).toBe(false);
    expect(descriptor().processCount).toBe(12);
    currentView = { ...includedView(), viewRevision: 9 };
    await act(async () => emitReset());
    const query = new URL(String(sessionReads().at(-1)![0]), "http://attyd.test").searchParams;
    expect(JSON.parse(query.get("excludeProcessFor")!)).toContain(turnId);
    // The mock deliberately returns the old full body despite the exclusion.
    expect(acp.state.timeline.some((item) => item.type === "tool")).toBe(false);
    expect(descriptor().processCount).toBe(12);
    await expect(acp.loadTurnProcess(descriptor(), 0, new AbortController().signal)).resolves.toMatchObject({ offset: 0, nextOffset: 10 });
    const exported = await acp.readThreadForExport();
    expect(exported?.timeline.filter((item) => item.type === "tool")).toHaveLength(12);
    expect(acp.state.timeline.some((item) => item.type === "tool")).toBe(false);
  });

  it("does not let a refresh sent before release resurrect process bodies or carry exclusions to a new owner", async () => {
    currentView = includedView();
    await mount(false);
    const process = acp.state.timeline.find((item) => item.retainedProcess)!.retainedProcess!;
    const pending = deferredResponse();
    readCompact = async () => pending.promise;
    currentView = { ...includedView(), viewRevision: 9 };
    await act(async () => emitReset());
    await act(async () => acp.releaseTurnProcess(process));
    await act(async () => pending.finish(currentView));
    expect(acp.state.timeline.some((item) => item.type === "tool")).toBe(false);
    readCompact = async () => response(currentView);
    currentView = { ...includedView(), viewRevision: 10, sessionIncarnation: 4 };
    await act(async () => emitReset());
    expect(acp.state.timeline.filter((item) => item.type === "tool")).toHaveLength(12);
    currentView = { ...currentView, viewRevision: 11 };
    await act(async () => emitReset());
    const query = new URL(String(sessionReads().at(-1)![0]), "http://attyd.test").searchParams;
    expect(query.has("excludeProcessFor")).toBe(false);
  });

  it("releases the same completed operation when prompt preflight has advanced only the raw view's history", async () => {
    currentView = includedView();
    await mount(false);
    const process = acp.state.timeline.find((item) => item.retainedProcess)!.retainedProcess!;
    currentView = { ...includedView(), viewRevision: 9, historyRevision: "new-history",
      collapsedTurns: includedView().collapsedTurns!.map((turn) => ({ ...turn, turnId: "relocated", historyRevision: "new-history" })),
    };
    const pending = deferredResponse();
    startTurn = async () => pending.promise;
    await act(async () => { expect(acp.prompt([{ type: "text", text: "Next request" }])).toBe(true); });
    expect(acp.state.timeline.find((item) => item.retainedProcess)?.retainedProcess).toEqual(process);
    await act(async () => acp.releaseTurnProcess(process));
    expect(acp.state.timeline.some((item) => item.type === "tool")).toBe(false);
    expect(acp.state.running).toBe(true);
    await act(async () => pending.finish({ operationId: "next-operation", disposition: "accepted", status: "running" }));
  });

  it("requests authoritative complete process from the first observed live turn while leaving initial reads compact", async () => {
    currentView = view({ phase: "running", activeTurn: {
      operationId: "observed-first", clientIntentId: "intent", prompt: [{ type: "text", text: "Live question" }],
      updates: [tool(99)], terminal: null,
    } });
    await mount();
    const inclusion = () => new URL(String(sessionReads().at(-1)![0]), "http://attyd.test").searchParams.get("includeProcessFrom");
    expect(inclusion()).toBeNull();
    currentView = view({ viewRevision: 8 });
    await act(async () => emitReset());
    expect(inclusion()).toBe("observed-first");
    currentView = view({ viewRevision: 9, phase: "running", activeTurn: {
      operationId: "observed-second", clientIntentId: "next", prompt: [], updates: [], terminal: null,
    } });
    await act(async () => emitReset());
    expect(inclusion()).toBe("observed-first");
    currentView = view({ viewRevision: 10 });
    await act(async () => emitReset());
    expect(inclusion()).toBe("observed-first");
    await act(async () => acp.goHome());
    await act(async () => acp.attachSession({ sessionId: owner.sessionId, cwd }));
    expect(inclusion()).toBeNull();
  });

  it("clears process inclusion when the session owner changes", async () => {
    currentView = view({ phase: "running", activeTurn: {
      operationId: "old-owner-operation", clientIntentId: "intent", prompt: [], updates: [], terminal: null,
    } });
    await mount();
    currentView = view({ viewRevision: 8, sessionIncarnation: 4 });
    await act(async () => emitReset());
    currentView = view({ viewRevision: 9, sessionIncarnation: 4 });
    await act(async () => emitReset());
    const query = new URL(String(sessionReads().at(-1)![0]), "http://attyd.test").searchParams;
    expect(query.get("includeProcessFrom")).toBeNull();
    expect(query.get("expectedIncarnation")).toBe("4");
  });

  it("keeps a fast accepted turn included even if no running snapshot was received", async () => {
    await mount();
    startTurn = async () => {
      currentView = view({ viewRevision: 8, historyRevision: "epoch:3:8" });
      return response({ operationId: "accepted-fast", disposition: "accepted", status: "ready" });
    };
    await act(async () => { expect(acp.prompt([{ type: "text", text: "Fast prompt" }])).toBe(true); });
    const query = new URL(String(sessionReads().at(-1)![0]), "http://attyd.test").searchParams;
    expect(query.get("presentation")).toBe("compact");
    expect(query.get("includeProcessFrom")).toBe("accepted-fast");
  });

  it.each(["running", "ready", "completed", "replacement-owner"] as const)("keeps an uncertain timed-out turn busy until a %s snapshot reconciles it", async (outcome) => {
    await mount();
    vi.useFakeTimers();
    const post = deferredResponse();
    const refresh = deferredResponse();
    startTurn = async () => post.promise;
    const prompt = [{ type: "text" as const, text: "Possibly admitted prompt" }];
    await act(async () => { expect(acp.prompt(prompt)).toBe(true); });
    const pending = acp.state.pendingPrompt;
    expect(pending).toBeDefined();
    readCompact = async () => refresh.promise;
    await act(async () => vi.advanceTimersByTimeAsync(REQUEST_TIMEOUT_MS));
    expect(acp.state.running).toBe(true);
    expect(acp.state.pendingPrompt).toEqual(pending);
    expect(acp.state.timeline.some((item) => item.type === "error" && item.message.includes("timed out"))).toBe(true);
    expect(acp.state.timeline.some((item) => item.type === "error" && item.retryBlocks != null)).toBe(false);
    await act(async () => { expect(acp.prompt([{ type: "text", text: "Queued next prompt" }])).toBe(false); });
    const writes = () => fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");
    expect(writes()).toHaveLength(1);
    expect(writes()[0][1]?.signal?.aborted).toBe(true);
    expect(fetchMock.mock.calls.some(([path]) => String(path).endsWith("/cancel"))).toBe(false);
    const phase = outcome === "running" ? "running" : "ready";
    currentView = view({ viewRevision: 8, phase,
      historyRevision: outcome === "completed" ? "epoch:3:8" : historyRevision,
      sessionIncarnation: outcome === "replacement-owner" ? 4 : owner.sessionIncarnation,
      activeTurn: phase === "running" ? {
      operationId: "accepted-despite-timeout", clientIntentId: pending!.requestId,
      prompt, updates: [], terminal: null,
    } : null });
    await act(async () => refresh.finish(currentView));
    expect(acp.state.running).toBe(phase === "running");
    expect(acp.state.pendingPrompt?.requestId).toBe(phase === "running" ? "accepted-despite-timeout" : undefined);
    const retry = acp.state.timeline.find((item) => item.type === "error" && item.retryBlocks != null);
    if (outcome === "ready") {
      expect(retry).toMatchObject({ type: "error", retryBlocks: prompt });
      expect(acp.state.timeline.some((item) => item.type === "message" && item.blocks === pending!.blocks)).toBe(true);
    } else {
      expect(retry).toBeUndefined();
    }
    await act(async () => post.finish({ operationId: "accepted-despite-timeout", disposition: "accepted", status: phase }));
    expect(writes()).toHaveLength(1);
    expect(acp.state.running).toBe(phase === "running");
    if (outcome === "ready" && retry?.type === "error") {
      readCompact = async () => response(currentView);
      currentView = { ...currentView, viewRevision: 9 };
      await act(async () => emitReset());
      expect(acp.state.timeline.find((item) => item.type === "error" && item.retryBlocks != null)).toMatchObject({ retryBlocks: prompt });
      expect(acp.state.timeline.filter((item) => item.type === "message" && item.blocks === pending!.blocks)).toHaveLength(1);
      startTurn = async () => response({ operationId: "explicit-retry", disposition: "accepted", status: "running" });
      await act(async () => { expect(acp.prompt(retry.retryBlocks!)).toBe(true); });
      expect(writes()).toHaveLength(2);
      expect(JSON.parse(String(writes()[1][1]?.body))).toEqual({ prompt });
    }
  });

  it("fails an expired prompt preflight normally without ever posting a turn", async () => {
    await mount();
    vi.useFakeTimers();
    const preflight = deferredResponse();
    readCompact = async () => preflight.promise;
    const prompt = [{ type: "text" as const, text: "Not submitted" }];
    await act(async () => { expect(acp.prompt(prompt)).toBe(true); });
    await act(async () => vi.advanceTimersByTimeAsync(REQUEST_TIMEOUT_MS));
    expect(acp.state.running).toBe(false);
    expect(acp.state.pendingPrompt).toBeUndefined();
    expect(acp.state.timeline.some((item) => item.type === "error" && item.retryBlocks?.[0] === prompt[0])).toBe(true);
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === "POST")).toBe(false);
    await act(async () => preflight.finish(currentView));
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === "POST")).toBe(false);
  });

  it("does not let a pre-timeout ready refresh settle an uncertain submitted turn", async () => {
    await mount();
    vi.useFakeTimers();
    const stale = deferredResponse();
    const post = deferredResponse();
    const reconciled = deferredResponse();
    startTurn = async () => post.promise;
    await act(async () => { expect(acp.prompt([{ type: "text", text: "Uncertain turn" }])).toBe(true); });
    await act(async () => vi.advanceTimersByTimeAsync(REQUEST_TIMEOUT_MS / 2));
    let reads = 0;
    readCompact = async () => {
      reads += 1;
      if (reads === 1) return stale.promise;
      return reconciled.promise;
    };
    currentView = view({ viewRevision: 8 });
    await act(async () => emitReset());
    await act(async () => vi.advanceTimersByTimeAsync(REQUEST_TIMEOUT_MS / 2));
    await act(async () => stale.finish(currentView));
    expect(acp.state.running).toBe(true);
    expect(acp.state.pendingPrompt).toBeDefined();
    await act(async () => reconciled.finish(view({ viewRevision: 9 })));
    expect(acp.state.running).toBe(false);
    await act(async () => post.finish({ operationId: "late", disposition: "accepted", status: "ready" }));
  });

  it("rejects a pre-aborted request without fetching and rejects an aborted late response", async () => {
    await mount();
    const controller = new AbortController();
    controller.abort();
    const requests = fetchMock.mock.calls.length;
    await expect(acp.loadTurnProcess(descriptor(), 0, controller.signal)).rejects.toMatchObject({ name: "AbortError" });
    expect(fetchMock.mock.calls).toHaveLength(requests);
    const pending = deferredResponse();
    readProcess = async () => pending.promise;
    const active = new AbortController();
    const result = acp.loadTurnProcess(descriptor(), 0, active.signal);
    const assertion = expect(result).rejects.toMatchObject({ name: "AbortError" });
    const transportSignal = fetchMock.mock.calls.at(-1)![1]?.signal;
    active.abort();
    expect(transportSignal?.aborted).toBe(true);
    await assertion;
    pending.finish(processPage());
  });

  it.each(["navigation", "incarnation", "epoch"] as const)("rejects a late process page after %s changes", async (change) => {
    await mount();
    const pending = deferredResponse();
    readProcess = async () => pending.promise;
    const result = acp.loadTurnProcess(descriptor(), 0, new AbortController().signal);
    const assertion = expect(result).rejects.toMatchObject({ name: "AbortError" });
    if (change === "navigation") await act(async () => acp.goHome());
    else {
      currentView = view({ viewRevision: 8,
        ...(change === "incarnation" ? { sessionIncarnation: 4 } : {}),
        ...(change === "epoch" ? { bridgeEpoch: "replacement-epoch" } : {}),
      });
      await act(async () => emitReset());
    }
    const current = acp.state;
    pending.finish(processPage());
    await assertion;
    expect(acp.state).toBe(current);
    expect(acp.state.timeline.some((item) => item.type === "tool")).toBe(false);
  });

  it.each(["before requesting", "while requesting"] as const)("refreshes an old process descriptor when a turn advances history %s", async (timing) => {
    await mount();
    const original = descriptor();
    const pending = deferredResponse();
    readProcess = async () => pending.promise;
    const result = timing === "while requesting"
      ? acp.loadTurnProcess(original, 0, new AbortController().signal) : undefined;
    const assertion = result ? expect(result).rejects.toMatchObject({ name: "AbortError" }) : undefined;
    currentView = view({ viewRevision: 8, historyRevision: "epoch:3:8",
      collapsedTurns: view().collapsedTurns!.map((turn) => ({ ...turn, historyRevision: "epoch:3:8" })) });
    await act(async () => source().onmessage?.({ data: JSON.stringify({
      type: "bridge/session_turn_complete", ...owner, viewRevision: 8, historyRevision: "epoch:3:8", phase: "ready",
      operationId: "new-completed-turn", clientIntentId: "new-completed-turn", response: { stopReason: "end_turn" },
    }) }));
    expect(sessionReads()).toHaveLength(1);
    expect(descriptor().historyRevision).toBe(historyRevision);
    await act(async () => {
      if (assertion) {
        pending.finish(processPage());
        await assertion;
      } else {
        await expect(acp.loadTurnProcess(original, 0, new AbortController().signal)).rejects.toMatchObject({ name: "AbortError" });
      }
    });
    expect(sessionReads()).toHaveLength(2);
    expect(descriptor().historyRevision).toBe("epoch:3:8");
    expect(acp.state.timeline.some((item) => item.type === "tool")).toBe(false);
    readProcess = async () => response({ ...processPage(), historyRevision: "epoch:3:8" });
    await expect(acp.loadTurnProcess(descriptor(), 0, new AbortController().signal)).resolves.toMatchObject({ historyRevision: "epoch:3:8" });
  });

  it("refreshes compact history after a 409 without retrying or applying the obsolete page", async () => {
    await mount();
    const original = descriptor();
    const updatedRevision = "epoch:3:8";
    currentView = view({ viewRevision: 8, historyRevision: updatedRevision,
      collapsedTurns: view().collapsedTurns!.map((turn) => ({ ...turn, historyRevision: updatedRevision })) });
    readProcess = async () => response({ error: "History changed", code: "history_changed" }, 409);
    await act(async () => {
      await expect(acp.loadTurnProcess(original, 0, new AbortController().signal)).rejects.toMatchObject({ status: 409 });
    });
    expect(descriptor().historyRevision).toBe(updatedRevision);
    expect(sessionReads()).toHaveLength(2);
    expect(new URL(String(sessionReads()[1][0]), "http://attyd.test").searchParams.get("presentation")).toBe("compact");
    expect(acp.state.timeline.some((item) => item.type === "tool")).toBe(false);
  });

  it.each([
    ["wrong owner", { ...processPage(), sessionIncarnation: 99 }],
    ["wrong history", { ...processPage(), historyRevision: "stale" }],
    ["wrong turn", { ...processPage(), turnId: "other" }],
    ["wrong offset", { ...processPage(), offset: 10 }],
    ["empty item", { ...processPage(), items: [[]] }],
    ["premature end", { ...processPage(), nextOffset: null }],
    ["missing update", { ...processPage(), items: processPage().items.map(() => [null]) }],
    ["missing terminals", { ...processPage(), terminals: null }],
  ] as const)("rejects malformed pages with %s", async (_name, malformed) => {
    await mount();
    const before = acp.state;
    readProcess = async () => response(malformed);
    await expect(acp.loadTurnProcess(descriptor(), 0, new AbortController().signal)).rejects.toThrow();
    expect(acp.state).toBe(before);
  });

  it("exports a complete owner-fenced snapshot without expanding or replacing the sparse visible state", async () => {
    await mount();
    const before = acp.state;
    const exported = await acp.readThreadForExport();
    expect(exported?.timeline.filter((item) => item.type === "tool")).toHaveLength(12);
    expect(exported?.timeline.some((item) => item.deferredProcess)).toBe(false);
    expect(JSON.stringify(exported?.timeline)).toContain("Final answer");
    const url = new URL(String(sessionReads().at(-1)![0]), "http://attyd.test");
    expect(url.searchParams.has("presentation")).toBe(false);
    expect(url.searchParams.get("expectedEpoch")).toBe(owner.bridgeEpoch);
    expect(url.searchParams.get("expectedIncarnation")).toBe("3");
    expect(acp.state).toBe(before);
    expect(acp.state.timeline.some((item) => item.type === "tool")).toBe(false);
  });

  it("ignores a full export response arriving after navigation", async () => {
    await mount();
    const pending = deferredResponse();
    readFull = async () => pending.promise;
    const exported = acp.readThreadForExport();
    await act(async () => acp.goHome());
    const current = acp.state;
    pending.finish(fullView());
    expect(await exported).toBeUndefined();
    expect(acp.state).toBe(current);
    expect(acp.state.session).toBeUndefined();
  });
});
