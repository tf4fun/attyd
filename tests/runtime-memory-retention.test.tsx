// @vitest-environment happy-dom

import type { ContentBlock, SessionUpdate } from "@agentclientprotocol/sdk";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { BridgeSessionView } from "../web/src/lib/business-api";
import { sessionPath } from "../web/src/lib/session-route";
import { useAcp } from "../web/src/lib/use-acp";

// M07-M09: exercise the real REST/SSE hook. The server may replace obsolete
// presentation deltas with a reset, but the resulting view and turn outcome
// must remain complete. Promises provide the GET/SSE interleaving barriers.
describe("runtime memory retention: browser recovery acceptance", () => {
  const sessionId = "memory-session";
  const cwd = "/work/memory";
  const sessionUrl = `/api/v1/sessions/${sessionId}`;
  const prompt: ContentBlock[] = [{ type: "text", text: "Inspect the build" }];
  let root: Root;
  let container: HTMLDivElement;
  let acp: ReturnType<typeof useAcp>;
  let fetchMock: ReturnType<typeof vi.fn<typeof fetch>>;
  let currentView: BridgeSessionView;
  let readView: () => Promise<Response>;

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

  function Harness() {
    acp = useAcp();
    return null;
  }

  function tool(text: string): SessionUpdate {
    return {
      sessionUpdate: "tool_call", toolCallId: "build-tool", title: "Build",
      kind: "execute", status: "in_progress",
      content: [{ type: "content", content: { type: "text", text } }],
    };
  }

  function view(revision = 1, output = "initial output"): BridgeSessionView {
    return {
      bridgeEpoch: "memory-epoch", sessionId, sessionIncarnation: 7,
      viewRevision: revision, historyRevision: "memory-epoch:7:1",
      phase: "running", syncError: null,
      timeline: [
        { sessionUpdate: "user_message_chunk", content: { type: "text", text: "Earlier question" } },
        { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Earlier answer" } },
      ],
      activeTurn: { operationId: "turn-1", clientIntentId: "intent-1", prompt,
        updates: [tool(output)], terminal: null },
      workspace: { cwd, session: {} }, controls: {},
      interactions: { permissions: {}, elicitations: {}, urlFlows: {} },
      operation: null, terminals: {},
    };
  }

  function readyView(revision: number, stopReason?: "end_turn" | "cancelled"): BridgeSessionView {
    const ready = view(revision, "final output");
    ready.phase = "ready";
    ready.historyRevision = `memory-epoch:7:${revision}`;
    ready.timeline.push(
      { sessionUpdate: "user_message_chunk", content: prompt[0] },
      ...ready.activeTurn!.updates,
    );
    ready.activeTurn = null;
    ready.turnOutcomes = stopReason == null ? [] : [{
      operationId: "turn-1", afterUpdate: ready.timeline.length, response: { stopReason },
    }];
    return ready;
  }

  function response(body: unknown) { return new Response(JSON.stringify(body)); }

  function deferredView() {
    let finish!: (view: BridgeSessionView) => void;
    const promise = new Promise<Response>((resolve) => { finish = (value) => resolve(response(value)); });
    return { promise, finish };
  }

  function source() {
    return TestEventSource.instances.find(({ url }) => url.split("?")[0] === `${sessionUrl}/events`)!;
  }

  function emit(event: unknown) { source().onmessage?.({ data: JSON.stringify(event) }); }

  function reset(viewRevision: number) {
    return { type: "bridge/session_reset", bridgeEpoch: "memory-epoch", sessionId,
      sessionIncarnation: 7, viewRevision };
  }

  function replacement(fromRevision: number, text: string) {
    return { type: "bridge/session_delta", bridgeEpoch: "memory-epoch", sessionId,
      sessionIncarnation: 7, fromRevision, viewRevision: fromRevision + 1,
      change: { kind: "turn_update", update: {
        sessionUpdate: "tool_call_update", toolCallId: "build-tool",
        content: [{ type: "content", content: { type: "text", text } }],
      } } };
  }

  function failure(viewRevision = 2) {
    return { type: "bridge/session_turn_failed", bridgeEpoch: "memory-epoch", sessionId,
      sessionIncarnation: 7, viewRevision, historyRevision: `memory-epoch:7:${viewRevision}`,
      phase: "ready", operationId: "turn-1", clientIntentId: "intent-1", prompt,
      error: { code: -32603, message: "Build agent failed", data: { retry: true } } };
  }

  function toolOutput() {
    const tools = acp.state.timeline.filter((item) => item.type === "tool");
    expect(tools).toHaveLength(1);
    return tools[0].call.content;
  }

  function expectToolOutput(text: string) {
    expect(toolOutput()).toEqual([{ type: "content", content: { type: "text", text } }]);
  }

  function sessionReads() {
    return fetchMock.mock.calls.filter(([path, init]) =>
      String(path).split("?")[0] === sessionUrl && (init?.method == null || init.method === "GET"));
  }

  async function mount() {
    window.history.replaceState(null, "", sessionPath(sessionId, cwd));
    await act(async () => root.render(createElement(Harness)));
    expect(acp.state.session?.sessionId).toBe(sessionId);
    expect(acp.state.running).toBe(true);
    expect(source()).toBeDefined();
  }

  beforeEach(() => {
    (globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    currentView = view();
    readView = async () => response(currentView);
    TestEventSource.instances = [];
    vi.stubGlobal("EventSource", TestEventSource);
    fetchMock = vi.fn(async (input, init) => {
      const path = String(input);
      if (path === "/api/v1/runtime") return response({
        connected: true, generation: 1, bridgeEpoch: "memory-epoch", hello: null,
        initialized: { type: "acp/initialized", response: {
          protocolVersion: 1, agentCapabilities: { loadSession: true, sessionCapabilities: { list: {} } },
        } }, error: null, phase: { type: "bridge/phase", phase: "ready" },
      });
      if (path === "/api/v1/sessions") return response({ sessions: [{ sessionId, cwd }] });
      if (path.split("?")[0] === sessionUrl) return readView();
      if (path === `${sessionUrl}/turns` && init?.method === "POST") {
        return response({ operationId: "turn-2", disposition: "accepted", status: "running" });
      }
      throw new Error(`Unexpected request: ${path}`);
    });
    vi.stubGlobal("fetch", fetchMock);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    window.history.replaceState(null, "", "/");
    vi.unstubAllGlobals();
  });

  it("M07/M08 coalesces a reset burst into one in-flight GET and one latest follow-up", async () => {
    await mount();
    const initialReads = sessionReads().length;
    const first = deferredView();
    const latest = deferredView();
    let reads = 0;
    readView = () => (++reads === 1 ? first.promise : latest.promise);
    await act(async () => {
      for (let revision = 2; revision <= 128; revision++) emit(reset(revision));
    });
    expect(reads).toBe(1);
    await act(async () => first.finish(view(2, "intermediate output")));
    expect(reads).toBe(2);
    const finalOutput = "complete output\n".repeat(8192);
    await act(async () => latest.finish(view(128, finalOutput)));
    expectToolOutput(finalOutput);
    expect(acp.state.running).toBe(true);
    expect(sessionReads()).toHaveLength(initialReads + 2);
    for (const [path] of sessionReads().slice(initialReads)) {
      const query = new URL(String(path), "http://localhost").searchParams;
      expect(query.get("expectedEpoch")).toBe("memory-epoch");
      expect(query.get("expectedIncarnation")).toBe("7");
    }
    expect(TestEventSource.instances.filter(({ url }) => url.includes(`${sessionId}/events`))).toHaveLength(1);
    expect(source().readyState).not.toBe(TestEventSource.CLOSED);
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === "POST")).toBe(false);
  });

  it("M08 installs the folded latest tool and accepts a contiguous suffix without duplicating history", async () => {
    await mount();
    currentView = view(40, "latest complete output");
    await act(async () => emit(reset(40)));
    expectToolOutput("latest complete output");
    const reads = sessionReads().length;
    await act(async () => emit(replacement(40, "latest complete output plus suffix")));
    expectToolOutput("latest complete output plus suffix");
    expect(sessionReads()).toHaveLength(reads);
    const userMessages = acp.state.timeline.filter((item) => item.type === "message");
    expect(userMessages.map(({ blocks }) => blocks)).toEqual([
      [{ type: "text", text: "Earlier question" }], prompt,
    ]);
    expect(acp.state.timeline.filter((item) => item.type === "assistant")).toHaveLength(1);
  });

  it("M08 ignores an older GET response after newer contiguous deltas have been applied", async () => {
    await mount();
    const snapshot = deferredView();
    readView = () => snapshot.promise;
    // A control delta starts a refresh without breaking SSE revision order.
    await act(async () => emit({
      type: "bridge/session_delta", bridgeEpoch: "memory-epoch", sessionId,
      sessionIncarnation: 7, fromRevision: 1, viewRevision: 2,
      change: { kind: "control_update", control: "mode", modeId: "build" },
    }));
    await act(async () => emit(replacement(2, "output at revision 3")));
    expectToolOutput("output at revision 3");
    await act(async () => snapshot.finish(view(2, "output at revision 2")));
    expectToolOutput("output at revision 3");
    const reads = sessionReads().length;
    await act(async () => emit(reset(3)));
    expect(sessionReads()).toHaveLength(reads);
  });

  it.each(["before", "after"] as const)(
    "M09 preserves RPC failure and retry prompt when its event arrives %s the newer reset view",
    async (order) => {
      await mount();
      const snapshot = deferredView();
      readView = () => snapshot.promise;
      // GET may overtake SSE; the stream itself stays ordered: reset 2, failure 3.
      await act(async () => emit(reset(2)));
      if (order === "before") await act(async () => emit(failure(3)));
      currentView = readyView(4);
      await act(async () => snapshot.finish(currentView));
      readView = async () => response(currentView);
      if (order === "after") await act(async () => emit(failure(3)));
      const errors = acp.state.timeline.filter((item) => item.type === "error");
      expect(errors).toHaveLength(1);
      expect(errors[0]).toMatchObject({
        id: "bridge-turn-outcome:turn-1", code: -32603,
        message: "Build agent failed", retryBlocks: prompt, data: { retry: true },
      });
      expect(acp.state.running).toBe(false);
      expect(acp.state.sessionSyncPhase).toBe("ready");
      // The user can retry explicitly against the newer history CAS; no reset
      // may silently resubmit the old prompt or consume the next append slot.
      expect(fetchMock.mock.calls.filter(([path]) => String(path).endsWith("/turns"))).toHaveLength(0);
      await act(async () => { expect(acp.prompt(errors[0].retryBlocks!)).toBe(true); });
      const submissions = fetchMock.mock.calls.filter(([path]) => String(path).endsWith("/turns"));
      expect(submissions).toHaveLength(1);
      expect(new Headers(submissions[0][1]?.headers).get("If-Match")).toBe('"memory-epoch:7:4"');
      expect(JSON.parse(String(submissions[0][1]?.body)).prompt).toEqual(prompt);
    },
  );

  it.each(["end_turn", "cancelled"] as const)(
    "M09 reconstructs a committed %s outcome exactly once after a reset",
    async (stopReason) => {
      await mount();
      currentView = readyView(5, stopReason);
      await act(async () => emit(reset(2)));
      await act(async () => emit({
        type: "bridge/session_turn_complete", bridgeEpoch: "memory-epoch", sessionId,
        sessionIncarnation: 7, viewRevision: 4, historyRevision: "memory-epoch:7:5",
        phase: "ready", operationId: "turn-1", clientIntentId: "intent-1", response: { stopReason },
      }));
      const outcomes = acp.state.timeline.filter((item) => item.type === "stop");
      expect(outcomes).toHaveLength(1);
      expect(outcomes[0].response.stopReason).toBe(stopReason);
      expectToolOutput("final output");
      expect(acp.state.running).toBe(false);
      expect(fetchMock.mock.calls.some(([, init]) => init?.method === "POST")).toBe(false);
    },
  );

  it("M09 shows a late failure without stopping the newer turn installed by a reset", async () => {
    await mount();
    currentView = readyView(5);
    currentView.phase = "running";
    currentView.activeTurn = {
      operationId: "turn-2", clientIntentId: "intent-2",
      prompt: [{ type: "text", text: "Continue with another task" }], updates: [], terminal: null,
    };
    await act(async () => emit(reset(2)));
    const pending = acp.state.pendingPrompt;
    expect(pending).toBeDefined();
    await act(async () => emit(failure(3)));
    expect(acp.state.timeline.filter((item) => item.type === "error")).toEqual([
      expect.objectContaining({ requestId: "turn-1", message: "Build agent failed", retryBlocks: prompt }),
    ]);
    expect(acp.state.running).toBe(true);
    expect(acp.state.sessionSyncPhase).toBe("running");
    expect(acp.state.pendingPrompt).toEqual(pending);
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === "POST")).toBe(false);
  });

  it.each([{ bridgeEpoch: "retired-epoch" }, { sessionIncarnation: 6 }])(
    "M11 rejects a failure belonging to an old owner: %j",
    async (oldOwner) => {
      await mount();
      await act(async () => emit({ ...failure(2), ...oldOwner }));
      expect(acp.state.timeline.filter((item) => item.type === "error")).toEqual([]);
      expect(acp.state.running).toBe(true);
      expectToolOutput("initial output");
      expect(acp.state.sessionOwner).toMatchObject({ bridgeEpoch: "memory-epoch", sessionIncarnation: 7 });
      expect(fetchMock.mock.calls.some(([, init]) => init?.method === "POST")).toBe(false);
    },
  );
});
