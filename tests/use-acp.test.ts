// @vitest-environment happy-dom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  parseGlobalBusinessEvent,
  parseSessionBusinessEvent,
  strongEtag,
  workspaceContextSearchPath,
  type BridgeSessionView,
} from "../web/src/lib/business-api";
import { projectPath, sessionPath } from "../web/src/lib/session-route";
import { useAcp } from "../web/src/lib/use-acp";
import type { TerminalSnapshot } from "../shared/bridge";

function terminalDelta(terminal: Record<string, unknown>, fromRevision = 1, overrides: Record<string, unknown> = {}) {
  return {
    type: "bridge/session_delta", bridgeEpoch: "epoch", sessionId: "alpha-one",
    sessionIncarnation: 1, fromRevision, viewRevision: fromRevision + 1,
    change: { kind: "terminal_update", terminal },
    ...overrides,
  };
}

function terminalSnapshot(overrides: Partial<TerminalSnapshot> = {}): TerminalSnapshot {
  return {
    sessionId: "alpha-one", terminalId: "terminal-1", output: "", truncated: false, released: false,
    ...overrides,
  };
}

describe("browser REST and SSE transport", () => {
  it("encodes the authoritative history revision as a strong If-Match value", () => {
    expect(strongEtag("epoch:1:7")).toBe('"epoch:1:7"');
    expect(() => strongEtag("")).toThrow("History revision");
    expect(() => strongEtag('bad"revision')).toThrow("History revision");
  });

  it("encodes context search text and session identity as query parameters", () => {
    expect(workspaceContextSearchPath("src/a.ts & tests", "project/session"))
      .toBe("/api/v1/context/search?query=src%2Fa.ts+%26+tests&sessionId=project%2Fsession");
  });

  it("accepts only the global business event vocabulary", () => {
    expect(parseGlobalBusinessEvent(JSON.stringify({
      type: "bridge/connection",
      phase: "ready",
    }))).toEqual({ type: "bridge/connection", phase: "ready" });
    expect(() => parseGlobalBusinessEvent(JSON.stringify({
      type: "acp/session_update",
    }))).toThrow("Unknown global event");
  });

  it("rejects malformed or raw ACP events on the session stream", () => {
    expect(parseSessionBusinessEvent(JSON.stringify({
      type: "bridge/session_delta",
      bridgeEpoch: "epoch",
      sessionId: "session",
      sessionIncarnation: 2,
      fromRevision: 6,
      viewRevision: 7,
      change: { kind: "turn_update", update: { sessionUpdate: "agent_message_chunk" } },
    }))).toMatchObject({ viewRevision: 7 });
    expect(() => parseSessionBusinessEvent(JSON.stringify({
      type: "acp/session_update",
      notification: {},
    }))).toThrow("Unknown session event");
    expect(() => parseSessionBusinessEvent(JSON.stringify({
      type: "bridge/session_reset",
      bridgeEpoch: "epoch",
      sessionId: "session",
      sessionIncarnation: 1,
      viewRevision: "7",
    }))).toThrow("invalid identity or revision");
    expect(parseSessionBusinessEvent(JSON.stringify({
      type: "bridge/session_turn_failed",
      bridgeEpoch: "epoch",
      sessionId: "session",
      sessionIncarnation: 2,
      viewRevision: 8,
      historyRevision: "epoch:2:8",
      phase: "ready",
      operationId: "turn-1",
      clientIntentId: "intent-1",
      prompt: [{ type: "text", text: "Retry me" }],
      error: { code: -32603, message: "Agent failed", data: { retry: true } },
    }))).toMatchObject({
      type: "bridge/session_turn_failed",
      error: { code: -32603 },
    });
    expect(() => parseSessionBusinessEvent(JSON.stringify({
      type: "bridge/session_turn_complete",
      bridgeEpoch: "epoch",
      sessionId: "session",
      sessionIncarnation: 2,
      viewRevision: 8,
      historyRevision: "epoch:2:8",
      phase: "ready",
      operationId: "turn-1",
      clientIntentId: "intent-1",
      response: "invalid",
    }))).toThrow("invalid payload");
  });

  it("validates terminal delta payloads and requires the envelope session identity", () => {
    const terminal = terminalSnapshot({ output: "streamed", outputAppend: true, retainedBytes: 8 });
    expect(parseSessionBusinessEvent(JSON.stringify(terminalDelta({ ...terminal }))))
      .toMatchObject({ change: { kind: "terminal_update", terminal } });
    for (const invalid of [
      { ...terminal, sessionId: "beta-one" },
      { ...terminal, terminalId: null },
      { ...terminal, output: 1 },
      { ...terminal, released: undefined },
      { ...terminal, outputAppend: "yes" },
      { ...terminal, retainedBytes: -1 },
      { ...terminal, exitStatus: { exitCode: -1 } },
    ]) {
      expect(() => parseSessionBusinessEvent(JSON.stringify(terminalDelta(invalid)))).toThrow();
    }
  });

});

describe("project navigation", () => {
  let root: Root;
  let container: HTMLDivElement;
  let acp: ReturnType<typeof useAcp>;
  let fetchMock: ReturnType<typeof vi.fn<typeof fetch>>;

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

  function sessionView(sessionId: string, cwd = "/work/alpha"): BridgeSessionView {
    return {
      bridgeEpoch: "epoch", sessionId, sessionIncarnation: 1, viewRevision: 1,
      historyRevision: "epoch:1:1", phase: "ready", syncError: null,
      timeline: [], activeTurn: null, workspace: { cwd, session: {} },
      controls: {}, interactions: { permissions: {}, elicitations: {}, urlFlows: {} },
      operation: null, terminals: {},
    };
  }

  const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
  const list = [
    { sessionId: "alpha-one", cwd: "/work/alpha", title: "Alpha one" },
    { sessionId: "beta-one", cwd: "/work/beta", title: "Beta one" },
    { sessionId: "alpha-two", cwd: "/work/alpha", title: "Alpha two" },
  ];
  const defaultFetch: typeof fetch = async (input, init) => {
    const path = String(input);
    if (path === "/api/v1/runtime") return response({
      connected: true, generation: 1, hello: null, initialized: {
        type: "acp/initialized", response: { protocolVersion: 1,
          agentCapabilities: { loadSession: true, sessionCapabilities: { list: {}, fork: {} } } },
      },
      error: null, phase: { type: "bridge/phase", phase: "ready" },
    });
    if (path === "/api/v1/sessions") {
      if (init?.method === "POST") return response({ sessionId: "created", view: sessionView("created") });
      return response({ sessions: list.slice(0, 2), nextCursor: "more" });
    }
    if (path === "/api/v1/sessions?cursor=more") return response({ sessions: list.slice(2) });
    if (path.startsWith("/api/v1/sessions/")) {
      const id = decodeURIComponent(path.split("?")[0].slice("/api/v1/sessions/".length));
      if (id === "missing") return response({ error: "Missing session", code: "session_not_found" }, 404);
      if (id.endsWith("/fork")) return response({ sessionId: "forked", view: sessionView("forked") });
      return response(sessionView(id, list.find(({ sessionId }) => sessionId === id)?.cwd));
    }
    throw new Error(`Unexpected request: ${path}`);
  };

  async function mount(path: string) {
    window.history.replaceState(null, "", path);
    await act(async () => root.render(createElement(Harness)));
  }

  async function restore(path: string) {
    await act(async () => {
      window.history.replaceState(null, "", path);
      window.dispatchEvent(new PopStateEvent("popstate"));
    });
  }

  beforeEach(() => {
    (globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    TestEventSource.instances = [];
    vi.stubGlobal("EventSource", TestEventSource);
    fetchMock = vi.fn(defaultFetch);
    vi.stubGlobal("fetch", fetchMock);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    window.history.replaceState(null, "", "/");
    vi.unstubAllGlobals();
  });

  it("uses new and materialized sessions when the Agent has no list capability", async () => {
    fetchMock.mockImplementation(async (input, init) => {
      if (String(input) === "/api/v1/runtime") return response({
        connected: true, generation: 1, hello: null,
        initialized: { type: "acp/initialized", response: { protocolVersion: 1, agentCapabilities: {} } },
        error: null, phase: { type: "bridge/phase", phase: "ready" },
      });
      if (String(input) === "/api/v1/sessions" && init?.method !== "POST") {
        throw new Error("A no-list Agent must not be queried for session history");
      }
      return defaultFetch(input, init);
    });
    await mount("/");
    expect(acp.state.timeline.some(({ type }) => type === "error")).toBe(false);
    await act(async () => { expect(acp.newSession("/work/alpha")).toBe(true); });
    expect(acp.state.session?.sessionId).toBe("created");
    await restore(sessionPath("created", "/work/alpha"));
    expect(acp.state.session?.sessionId).toBe("created");
    expect(acp.state.timeline.some(({ type }) => type === "error")).toBe(false);
    expect(fetchMock.mock.calls.filter(([path, init]) => path === "/api/v1/sessions" && init?.method !== "POST")).toHaveLength(0);
  });

  it("restores a load-only Agent session using the shared route workspace", async () => {
    fetchMock.mockImplementation(async (input, init) => {
      if (String(input) === "/api/v1/runtime") return response({
        connected: true, generation: 1, hello: null,
        initialized: { type: "acp/initialized", response: { protocolVersion: 1, agentCapabilities: { loadSession: true } } },
        error: null, phase: { type: "bridge/phase", phase: "ready" },
      });
      if (String(input) === "/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha") return response(sessionView("alpha-one"));
      if (String(input).startsWith("/api/v1/sessions")) throw new Error(`Unexpected cold request: ${String(input)}`);
      return defaultFetch(input, init);
    });
    await mount(sessionPath("alpha-one", "/work/alpha"));
    expect(acp.state.session?.sessionId).toBe("alpha-one");
    expect(fetchMock.mock.calls.map(([path]) => path)).toContain("/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha");
    expect(acp.state.timeline.some(({ type }) => type === "error")).toBe(false);
  });

  it("loads every project list page without opening a conversation", async () => {
    await mount(projectPath("/work/alpha"));
    expect(acp.projectCwd).toBe("/work/alpha");
    expect(acp.state.session).toBeUndefined();
    expect(acp.state.sessions).toHaveLength(3);
    expect(fetchMock.mock.calls.map(([path]) => path)).toEqual([
      "/api/v1/runtime", "/api/v1/sessions", "/api/v1/sessions?cursor=more",
    ]);
    expect(window.location.pathname).toBe(projectPath("/work/alpha"));
  });

  it("offers the route workspace for an ID absent from an available session list", async () => {
    await mount(sessionPath("unlisted", "/work/alpha"));
    expect(acp.state.session?.sessionId).toBe("unlisted");
    expect(fetchMock.mock.calls.map(([path]) => path)).toContain("/api/v1/sessions/unlisted?cwd=%2Fwork%2Falpha");
  });

  it("keeps listed route workspaces available to both view and stream after other observers refresh", async () => {
    await mount(sessionPath("alpha-one", "/work/wrong"));
    expect(fetchMock.mock.calls.map(([path]) => path)).toContain("/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha");
    expect(TestEventSource.instances.map(({ url }) => url)).toContain("/api/v1/sessions/alpha-one/events?cwd=%2Fwork%2Falpha");
  });

  it("passes the selected workspace on ordinary session clicks for view and stream", async () => {
    await mount(projectPath("/work/alpha"));
    await act(async () => acp.attachSession(list[0]));
    expect(fetchMock.mock.calls.map(([path]) => path)).toContain("/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha");
    expect(TestEventSource.instances.map(({ url }) => url)).toContain("/api/v1/sessions/alpha-one/events?cwd=%2Fwork%2Falpha");
  });

  it("explains why a fork without history loading is unavailable before posting", async () => {
    fetchMock.mockImplementation(async (input, init) => {
      if (String(input) === "/api/v1/runtime") return response({
        connected: true, generation: 1, hello: null,
        initialized: { type: "acp/initialized", response: { protocolVersion: 1,
          agentCapabilities: { sessionCapabilities: { fork: {} } } } },
        error: null, phase: { type: "bridge/phase", phase: "ready" },
      });
      return defaultFetch(input, init);
    });
    await mount(sessionPath("created", "/work/alpha"));
    await act(async () => acp.forkSession());
    expect(fetchMock.mock.calls.some(([path]) => String(path).endsWith("/fork"))).toBe(false);
    expect(JSON.stringify(acp.state.timeline)).toContain("Forking requires Agent history loading");
  });

  it("canonicalizes legacy and mismatched project links using the Agent workspace", async () => {
    await mount("/sessions/alpha-two");
    expect(acp.state.session?.sessionId).toBe("alpha-two");
    expect(acp.projectCwd).toBe("/work/alpha");
    expect(window.location.pathname).toBe(sessionPath("alpha-two", "/work/alpha"));
    await restore(sessionPath("beta-one", "/work/wrong"));
    expect(acp.projectCwd).toBe("/work/beta");
    expect(acp.state.cwd).toBe("/work/beta");
    expect(window.location.pathname).toBe(sessionPath("beta-one", "/work/beta"));
  });

  it("searches workspace context in the selected session after changing projects", async () => {
    fetchMock.mockImplementation(async (input, init) => String(input).startsWith("/api/v1/context/search?")
      ? response({ matches: [] })
      : defaultFetch(input, init));
    await mount(sessionPath("alpha-one", "/work/alpha"));
    await acp.searchWorkspaceContext("src");
    await act(async () => acp.attachSession(list[1]));
    await acp.searchWorkspaceContext("src");
    expect(fetchMock.mock.calls.map(([path]) => path).filter((path) => String(path).startsWith("/api/v1/context/search?")))
      .toEqual([
        "/api/v1/context/search?query=src&sessionId=alpha-one",
        "/api/v1/context/search?query=src&sessionId=beta-one",
      ]);
    await act(async () => acp.openProject("/work/alpha"));
    await expect(acp.searchWorkspaceContext("src")).rejects.toThrow("active ACP session");
  });

  it("applies versioned terminal output and release without fetching the session for each delta", async () => {
    await mount(sessionPath("alpha-one", "/work/alpha"));
    const source = TestEventSource.instances.find(({ url }) => url.split("?")[0].endsWith("alpha-one/events"))!;
    const emit = async (event: unknown) => act(async () => source.onmessage?.({ data: JSON.stringify(event) }));
    await emit(terminalDelta({ ...terminalSnapshot({ output: "starting\n" }) }));
    expect(acp.state.terminalSnapshots).toHaveLength(1);
    expect(acp.state.terminalSnapshots[0].output).toBe("starting\n");
    await emit(terminalDelta({ ...terminalSnapshot({ output: "working\n", outputAppend: true, retainedBytes: 17 }) }, 2));
    expect(acp.state.terminalSnapshots[0].output).toBe("starting\nworking\n");
    expect(acp.state.terminalSnapshots[0].outputAppend).toBe(false);
    await emit(terminalDelta({ ...terminalSnapshot({
      output: "starting\nworking\n", released: true, exitStatus: { exitCode: 0 },
    }) }, 3));
    expect(acp.state.terminalSnapshots[0]).toMatchObject({
      output: "starting\nworking\n", released: true, exitStatus: { exitCode: 0 },
    });
    await emit({
      type: "bridge/session_reset", bridgeEpoch: "epoch", sessionId: "alpha-one",
      sessionIncarnation: 1, viewRevision: 4,
    });
    expect(fetchMock.mock.calls.filter(([path]) => path === "/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha")).toHaveLength(1);
  });

  it("merges raw UTF-8 fragments and trims retained output by bytes", async () => {
    await mount(sessionPath("alpha-one", "/work/alpha"));
    const source = TestEventSource.instances.find(({ url }) => url.split("?")[0].endsWith("alpha-one/events"))!;
    const emit = async (terminal: TerminalSnapshot, revision: number) => act(async () =>
      source.onmessage?.({ data: JSON.stringify(terminalDelta({ ...terminal }, revision)) }));
    await emit(terminalSnapshot({ output: "old🙂" }), 1);
    await emit(terminalSnapshot({ output: "好", outputAppend: true, retainedBytes: 7 }), 2);
    expect(acp.state.terminalSnapshots[0].output).toBe("🙂好");
    await emit(terminalSnapshot({ output: "", outputBytes: "5A==", outputAppend: false, retainedBytes: 1 }), 3);
    expect(acp.state.terminalSnapshots[0].output).toBe("");
    expect(acp.state.terminalSnapshots[0].outputBytes).toBe("5A==");
    await emit(terminalSnapshot({ output: "", outputBytes: "uK0=", outputAppend: true, retainedBytes: 3 }), 4);
    expect(acp.state.terminalSnapshots[0].output).toBe("中");
    expect(acp.state.terminalSnapshots[0].outputBytes).toBe("5Lit");
    await emit(terminalSnapshot({ output: "discard", outputAppend: true, retainedBytes: 0 }), 5);
    expect(acp.state.terminalSnapshots[0].output).toBe("");
    expect(acp.state.terminalSnapshots[0].outputBytes).toBe("");
    await emit(terminalSnapshot({ output: "🙂好", retainedBytes: 5, truncated: true }), 6);
    expect(acp.state.terminalSnapshots[0].output).toBe("好");
    expect(acp.state.terminalSnapshots[0].retainedBytes).toBe(3);
    expect(fetchMock.mock.calls.filter(([path]) => path === "/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha")).toHaveLength(1);
  });

  it.each([
    { fromRevision: 3 },
    { bridgeEpoch: "replacement" },
    { sessionIncarnation: 2 },
  ])("refreshes terminal state after a delta version mismatch: %j", async (mismatch) => {
    await mount(sessionPath("alpha-one", "/work/alpha"));
    const source = TestEventSource.instances.find(({ url }) => url.split("?")[0].endsWith("alpha-one/events"))!;
    const authoritative = terminalSnapshot({ output: "authoritative output", released: true });
    fetchMock.mockImplementation(async (input, init) => String(input) === "/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha"
      ? response({ ...sessionView("alpha-one"), viewRevision: 8, terminals: { "terminal-1": authoritative } })
      : defaultFetch(input, init));
    await act(async () => source.onmessage?.({ data: JSON.stringify(terminalDelta(
      { ...terminalSnapshot({ output: "out of order", outputAppend: true }) }, 1, mismatch,
    )) }));
    expect(acp.state.terminalSnapshots).toEqual([authoritative]);
    expect(fetchMock.mock.calls.filter(([path]) => path === "/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha")).toHaveLength(2);
  });

  it("recovers a missing append base and resumes from authoritative snapshot bytes", async () => {
    await mount(sessionPath("alpha-one", "/work/alpha"));
    const source = TestEventSource.instances.find(({ url }) => url.split("?")[0].endsWith("alpha-one/events"))!;
    fetchMock.mockImplementation(async (input, init) => String(input) === "/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha"
      ? response({ ...sessionView("alpha-one"), viewRevision: 2, terminals: {
        "terminal-1": terminalSnapshot({ output: "", outputBytes: "5A==" }),
      } })
      : defaultFetch(input, init));
    await act(async () => source.onmessage?.({ data: JSON.stringify(terminalDelta({
      ...terminalSnapshot({ output: "unknown suffix", outputAppend: true }),
    })) }));
    await act(async () => source.onmessage?.({ data: JSON.stringify(terminalDelta({
      ...terminalSnapshot({ output: "", outputBytes: "uK0=", outputAppend: true, retainedBytes: 3 }),
    }, 2)) }));
    expect(acp.state.terminalSnapshots[0].output).toBe("中");
    expect(fetchMock.mock.calls.filter(([path]) => path === "/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha")).toHaveLength(2);
  });

  it("rejects cross-session and malformed terminal deltas without advancing the active view", async () => {
    await mount(sessionPath("alpha-one", "/work/alpha"));
    const source = TestEventSource.instances.find(({ url }) => url.split("?")[0].endsWith("alpha-one/events"))!;
    await act(async () => source.onmessage?.({ data: JSON.stringify(terminalDelta({
      ...terminalSnapshot({ output: "different active session", sessionId: "beta-one" }),
    }, 1, { sessionId: "beta-one" })) }));
    for (const terminal of [
      { ...terminalSnapshot({ output: "wrong session", sessionId: "beta-one" }) },
      { ...terminalSnapshot(), output: 9 },
    ]) {
      await act(async () => source.onmessage?.({ data: JSON.stringify(terminalDelta(terminal)) }));
    }
    expect(acp.state.terminalSnapshots).toEqual([]);
    await act(async () => source.onmessage?.({ data: JSON.stringify(terminalDelta({
      ...terminalSnapshot({ output: "accepted" }),
    })) }));
    expect(acp.state.terminalSnapshots[0].output).toBe("accepted");
    expect(fetchMock.mock.calls.filter(([path]) => path === "/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha")).toHaveLength(1);
  });

  it("tracks home, project and session navigation without selecting project sessions", async () => {
    await mount("/");
    await act(async () => acp.openProject("/work/alpha"));
    expect(window.location.pathname).toBe(projectPath("/work/alpha"));
    expect(acp.projectCwd).toBe("/work/alpha");
    expect(acp.state.session).toBeUndefined();
    await act(async () => acp.attachSession(list[0]));
    expect(window.location.pathname).toBe(sessionPath("alpha-one", "/work/alpha"));
    expect(acp.state.session?.sessionId).toBe("alpha-one");
    await restore(projectPath("/work/alpha"));
    expect(acp.projectCwd).toBe("/work/alpha");
    expect(acp.state.session).toBeUndefined();
    expect(acp.state.cachedSessions.has("alpha-one")).toBe(true);
    await restore("/");
    expect(acp.projectCwd).toBeUndefined();
    expect(acp.state.session).toBeUndefined();
  });

  it("returns missing and malformed session links home", async () => {
    await mount(sessionPath("missing", "/work/alpha"));
    expect(window.location.pathname).toBe("/");
    expect(acp.projectCwd).toBeUndefined();
    expect(acp.state.timeline.some(({ type }) => type === "error")).toBe(false);
    await restore("/projects/%2Fwork/sessions/%");
    expect(window.location.pathname).toBe("/");
    expect(acp.state.session).toBeUndefined();
  });

  it("keeps direct empty projects open and creates and forks nested session routes", async () => {
    await mount(projectPath("/work/empty"));
    expect(window.location.pathname).toBe(projectPath("/work/empty"));
    expect(acp.state.session).toBeUndefined();
    await act(async () => { expect(acp.newSession("/work/alpha")).toBe(true); });
    expect(window.location.pathname).toBe(sessionPath("created", "/work/alpha"));
    expect(acp.projectCwd).toBe("/work/alpha");
    expect(acp.state.sessions).toHaveLength(3);
    await act(async () => acp.forkSession());
    expect(window.location.pathname).toBe(sessionPath("forked", "/work/alpha"));
    expect(acp.projectCwd).toBe("/work/alpha");
  });

  it("does not let delayed creation override a later project navigation", async () => {
    let finishCreation!: (value: Response) => void;
    fetchMock.mockImplementation(async (input, init) => {
      if (String(input) === "/api/v1/sessions" && init?.method === "POST") {
        return new Promise<Response>((resolve) => { finishCreation = resolve; });
      }
      return defaultFetch(input, init);
    });
    await mount(projectPath("/work/alpha"));
    await act(async () => { acp.newSession("/work/alpha"); });
    await act(async () => acp.openProject("/work/beta"));
    await act(async () => finishCreation(response({ sessionId: "created", view: sessionView("created") })));
    expect(window.location.pathname).toBe(projectPath("/work/beta"));
    expect(acp.projectCwd).toBe("/work/beta");
    expect(acp.state.session).toBeUndefined();
  });

  it.each(["running", "ready"] as const)("restores a background prompt that is %s after browsing its project", async (returnPhase) => {
    let intentId: string | null = null;
    let admitted = false;
    let completed = false;
    fetchMock.mockImplementation(async (input, init) => {
      if (String(input) === "/api/v1/sessions/alpha-one/turns") {
        admitted = true;
        intentId = new Headers(init?.headers).get("Idempotency-Key");
        return response({ operationId: "turn-1", disposition: "accepted", status: "running" });
      }
      if (String(input) === "/api/v1/sessions/alpha-one?cwd=%2Fwork%2Falpha" && admitted) {
        const view = sessionView("alpha-one");
        view.viewRevision = completed ? 3 : 2;
        view.phase = completed ? "ready" : "running";
        if (completed) {
          view.historyRevision = "epoch:1:3";
          view.timeline = [
            { sessionUpdate: "user_message_chunk", content: { type: "text", text: "Keep working" } },
            { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Completed" } },
          ];
          view.turnOutcomes = [{ operationId: "turn-1", afterUpdate: 2, response: { stopReason: "end_turn" } }];
        } else {
          view.activeTurn = {
            operationId: "turn-1", clientIntentId: intentId!,
            prompt: [{ type: "text", text: "Keep working" }], updates: [], terminal: null,
          };
        }
        return response(view);
      }
      return defaultFetch(input, init);
    });
    await mount(sessionPath("alpha-one", "/work/alpha"));
    await act(async () => { expect(acp.prompt([{ type: "text", text: "Keep working" }])).toBe(true); });
    expect(acp.state.running).toBe(true);
    const previousStream = TestEventSource.instances.find(({ url }) => url.split("?")[0].endsWith("alpha-one/events"))!;
    await act(async () => acp.openProject("/work/alpha"));
    expect(previousStream.readyState).toBe(TestEventSource.CLOSED);
    expect(acp.state.session).toBeUndefined();
    expect(acp.state.cachedSessions.get("alpha-one")?.running).toBe(true);
    completed = returnPhase === "ready";
    await restore(sessionPath("alpha-one", "/work/alpha"));
    expect(acp.state.session?.sessionId).toBe("alpha-one");
    expect(acp.state.running).toBe(returnPhase === "running");
    expect(acp.state.sessionSyncPhase).toBe(returnPhase);
    expect(acp.state.timeline.filter((item) => item.type === "message" &&
      (item.role === "user" || item.role === "protocol-user"))).toHaveLength(1);
    const restoredStream = TestEventSource.instances.filter(({ url }) => url.split("?")[0].endsWith("alpha-one/events")).at(-1)!;
    expect(restoredStream).not.toBe(previousStream);
    expect(restoredStream.readyState).not.toBe(TestEventSource.CLOSED);
    if (returnPhase === "running") {
      await act(async () => restoredStream.onmessage?.({ data: JSON.stringify({
        type: "bridge/session_turn_complete", bridgeEpoch: "epoch", sessionId: "alpha-one",
        sessionIncarnation: 1, viewRevision: 3, historyRevision: "epoch:1:3", phase: "ready",
        operationId: "turn-1", clientIntentId: intentId, response: { stopReason: "end_turn" },
      }) }));
      expect(acp.state.running).toBe(false);
      expect(acp.state.timeline.at(-1)?.type).toBe("stop");
    }
    expect(fetchMock.mock.calls.filter(([path]) => String(path).endsWith("/cancel"))).toHaveLength(0);
  });

  it("does not open a session after late pagination completes following navigation home", async () => {
    let finishPage!: (value: Response) => void;
    fetchMock.mockImplementation(async (input, init) => {
      if (String(input) === "/api/v1/sessions?cursor=more") {
        return new Promise<Response>((resolve) => { finishPage = resolve; });
      }
      return defaultFetch(input, init);
    });
    await mount(sessionPath("alpha-two", "/work/alpha"));
    await act(async () => acp.goHome());
    await act(async () => finishPage(response({ sessions: list.slice(2) })));
    expect(window.location.pathname).toBe("/");
    expect(acp.projectCwd).toBeUndefined();
    expect(acp.state.session).toBeUndefined();
    expect(fetchMock.mock.calls.some(([path]) => path === "/api/v1/sessions/alpha-two")).toBe(false);
  });

  it("lets the newer popstate win over an in-flight project's paginated discovery", async () => {
    let finishPage!: (value: Response) => void;
    let held = false;
    fetchMock.mockImplementation(async (input, init) => {
      if (String(input) === "/api/v1/sessions?cursor=more" && !held) {
        held = true;
        return new Promise<Response>((resolve) => { finishPage = resolve; });
      }
      return defaultFetch(input, init);
    });
    await mount(projectPath("/work/alpha"));
    await restore(sessionPath("beta-one", "/work/beta"));
    await act(async () => finishPage(response({ sessions: list.slice(2) })));
    expect(window.location.pathname).toBe(sessionPath("beta-one", "/work/beta"));
    expect(acp.projectCwd).toBe("/work/beta");
    expect(acp.state.session?.sessionId).toBe("beta-one");
  });
});
