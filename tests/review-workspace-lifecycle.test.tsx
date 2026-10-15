// @vitest-environment happy-dom

import { act, createElement, StrictMode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CatalogLive, Event, GlobalLive, LogEntry, Operation, SessionLive } from "../shared/protocol";
import { H0 } from "../shared/replica";
import { projectPath, sessionPath } from "../web/src/lib/session-route";
import { useAcp } from "../web/src/lib/use-acp";

const sessionId = "review-lifecycle";
const scope = `session/${sessionId}`;
const cwd = "/work/review";

class FakeEventSource {
  static instances: FakeEventSource[] = [];
  onmessage: ((event: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;

  constructor(readonly url: string) {
    FakeEventSource.instances.push(this);
  }

  close(): void {
    this.closed = true;
  }

  follows(wanted: string): boolean {
    return new URL(this.url, window.location.href).searchParams.getAll("scope").includes(wanted);
  }

  emit(event: Event): void {
    if (!this.closed) this.onmessage?.({ data: JSON.stringify(event) });
  }
}

function operation(id: string, overrides: Partial<Operation> = {}): Operation {
  return { id, kind: "none", status: "completed", input: null, cancelRequested: false, interactions: {}, ...overrides };
}

function globalLive(list = false, loading = false): GlobalLive {
  return {
    op: operation("global-idle"),
    urlFlows: {},
    terminals: {},
    sessions: loading ? {
      [sessionId]: {
        sessionId, cwd, title: "Review session", op: { kind: "load", status: "running" },
        pending: { interactions: 0, urlFlows: 0 },
      },
    } : {},
    agent: {
      phase: "ready", error: null, authenticatedMethodId: null,
      launch: { transport: "stdio", command: ["review-agent"], cwd, readOnly: false, additionalDirectories: [], mcpServers: [] },
      initialize: {
        protocolVersion: 1,
        agentCapabilities: { loadSession: true, sessionCapabilities: list ? { list: {} } : {} },
      },
    },
  };
}

function catalogLive(listed = false): CatalogLive {
  return {
    op: operation(listed ? "catalog-listed" : "catalog-idle", listed ? { kind: "list" } : {}),
    urlFlows: {}, revision: "catalog-revision",
  };
}

function sessionLive(id: string): SessionLive {
  return {
    sessionId, cwd, op: operation(id, { kind: "load", status: "running", input: { cwd } }),
    urlFlows: {}, terminals: {},
    controls: {
      modes: null, configOptions: null, commands: [],
      info: { title: "Review session", updatedAt: null, meta: null }, usage: null,
    },
  };
}

function alignment(scope: string, live: GlobalLive | CatalogLive): Event {
  return { scope, log: { from: 0, prev: H0, entries: [] }, live: { replace: live } };
}

describe("v2 workspace lifecycle review", () => {
  let root: Root | undefined;
  let container: HTMLDivElement;
  let acp: ReturnType<typeof useAcp>;
  let fetchMock: ReturnType<typeof vi.fn<typeof fetch>>;

  function Harness() {
    acp = useAcp();
    return null;
  }

  function activeStream(wanted = "global"): FakeEventSource {
    const active = FakeEventSource.instances.filter((source) => !source.closed && source.follows(wanted));
    expect(active, `an active stream following ${wanted}`).toHaveLength(1);
    return active[0];
  }

  async function mount(path: string, strict = false): Promise<void> {
    window.history.replaceState(null, "", path);
    await act(async () => {
      const harness = createElement(Harness);
      root!.render(strict ? createElement(StrictMode, null, harness) : harness);
    });
  }

  async function emit(source: FakeEventSource, event: Event): Promise<void> {
    await act(async () => source.emit(event));
  }

  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
    fetchMock = vi.fn<typeof fetch>(async (input, init) => {
      expect(String(input)).toBe("/api/v2/scopes/catalog/ops");
      expect(init?.method).toBe("POST");
      expect(JSON.parse(String(init?.body))).toMatchObject({ kind: "list", args: {} });
      return new Response(JSON.stringify({
        op: "catalog-listed",
        result: {
          sessions: [{ sessionId, cwd, title: "Review session" }],
          nextCursor: null, revision: "catalog-revision",
        },
      }), { status: 200 });
    });
    vi.stubGlobal("fetch", fetchMock);
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => {
      root?.unmount();
      root = undefined;
      await vi.runOnlyPendingTimersAsync();
    });
    container.remove();
    window.history.replaceState(null, "", "/");
    vi.clearAllTimers();
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("receives live state after StrictMode effect replay and closes every stream on unmount", async () => {
    await mount("/", true);
    // Allow either immediate cleanup or a deferred cleanup cancelled by setup.
    // Assert the surviving connection, not a particular constructor count.
    await act(async () => { await vi.runOnlyPendingTimersAsync(); });
    const source = activeStream();
    await emit(source, alignment("global", globalLive()));
    expect(acp.state.phase).toBe("ready");
    expect(acp.state.command).toEqual(["review-agent"]);
    await emit(source, { scope: "global", live: { patch: [{ op: "set", path: "/agent/phase", value: "stopped" }] } });
    expect(acp.state.phase).toBe("stopped");
    expect(fetchMock).not.toHaveBeenCalled();

    await act(async () => {
      root!.unmount();
      root = undefined;
      await vi.runOnlyPendingTimersAsync();
    });
    expect(FakeEventSource.instances.length).toBeGreaterThan(0);
    expect(FakeEventSource.instances.every((stream) => stream.closed)).toBe(true);
  });

  it("shows a known session's load failure and resumes loading on an explicit retry", async () => {
    await mount(sessionPath(sessionId, cwd));
    const initial = activeStream();
    await emit(initial, alignment("global", globalLive(true)));
    await emit(initial, alignment("catalog", catalogLive()));
    // The catalog response establishes that this is a known session, so an
    // Agent load error must be shown rather than interpreted as an unknown URL.
    expect(acp.state.sessions.some((session) => session.sessionId === sessionId)).toBe(true);

    const loading = activeStream(scope);
    await emit(loading, alignment("global", globalLive(true, true)));
    await emit(loading, alignment("catalog", catalogLive(true)));
    await emit(loading, { scope, live: { replace: sessionLive("load-first") } });
    expect(acp.sessionOpening).toEqual({ sessionId });
    await emit(loading, { scope: "global", live: { patch: [{ op: "remove", path: `/sessions/${sessionId}` }] } });
    await emit(loading, { scope, end: { reason: "load_failed", error: { code: "agent_error", message: "Review load failed" } } });

    expect(acp.sessionOpening).toEqual({ sessionId, error: "Review load failed" });
    expect(window.location.pathname).toBe(sessionPath(sessionId, cwd));
    const beforeRetry = new Set(FakeEventSource.instances);
    await act(async () => acp.retrySessionOpening());
    const retry = activeStream(scope);
    expect(beforeRetry.has(retry)).toBe(false);
    expect(loading.closed).toBe(true);

    await emit(retry, alignment("global", globalLive(true, true)));
    await emit(retry, alignment("catalog", catalogLive(true)));
    await emit(retry, { scope, live: { replace: sessionLive("load-retry") } });
    expect(acp.sessionOpening).toEqual({ sessionId });
    const history: LogEntry = {
      hash: "BBBBBBBBBBBBBBBBBBBBBA",
      turn: { prompt: [{ type: "text", text: "Question" }], reply: [{ type: "text", text: "Recovered answer" }] },
    };
    await emit(retry, {
      scope, log: { from: 0, prev: H0, entries: [history] },
      live: { patch: [{ op: "set", path: "/op", value: operation("load-retry", { kind: "load", input: { cwd } }) }] },
    });
    expect(acp.sessionOpening).toBeUndefined();
    expect(acp.state.session?.sessionId).toBe(sessionId);
    expect(acp.state.timeline).toContainEqual(expect.objectContaining({
      type: "assistant", chunks: [expect.objectContaining({ blocks: [{ type: "text", text: "Recovered answer" }] })],
    }));
  });

  it("discovers a project's sessions on later catalog pages after its URL is opened", async () => {
    const targetCwd = "/other-workspace";
    const revision = "catalog-revision";
    const nextCursor = "other-workspace-page";
    const requests: Array<{ expect: string; kind: string; args: Record<string, unknown> }> = [];
    const replies: Array<(response: Response) => void> = [];
    let inFlight = 0;
    let maximumInFlight = 0;
    fetchMock.mockImplementation(async (input, init) => {
      expect(String(input)).toBe("/api/v2/scopes/catalog/ops");
      expect(init?.method).toBe("POST");
      requests.push(JSON.parse(String(init?.body)));
      inFlight += 1;
      maximumInFlight = Math.max(maximumInFlight, inFlight);
      return new Promise<Response>((resolve) => replies.push(resolve))
        .finally(() => { inFlight -= 1; });
    });

    await mount(projectPath(targetCwd));
    const source = activeStream();
    await emit(source, alignment("global", globalLive(true)));
    await emit(source, alignment("catalog", { ...catalogLive(), op: operation("c0") }));
    expect(requests).toEqual([{ expect: "c0", kind: "list", args: {} }]);
    await emit(source, { scope: "catalog", live: { patch: [{
      op: "set", path: "/op", value: operation("c1", { kind: "list", status: "running", input: {} }),
    }] } });

    // The first result has arrived, but its operation is still running in the
    // replica. A later page must wait for the stream's completed operation.
    await act(async () => replies[0](new Response(JSON.stringify({
      op: "c1",
      result: { revision, nextCursor, sessions: [{ sessionId: "A", cwd, title: "First workspace" }] },
    }), { status: 200 })));
    expect(acp.state.sessions.map((session) => session.sessionId)).toEqual(["A"]);
    expect(acp.state.nextSessionCursor).toBe(nextCursor);
    expect(inFlight).toBe(0);
    expect(requests).toHaveLength(1);

    await emit(source, { scope: "catalog", live: { patch: [{
      op: "set", path: "/op", value: operation("c1", { kind: "list", input: {} }),
    }] } });
    expect(requests).toEqual([
      { expect: "c0", kind: "list", args: {} },
      { expect: "c1", kind: "list", args: { cursor: nextCursor, revision } },
    ]);
    expect(inFlight).toBe(1);
    await emit(source, { scope: "catalog", live: { patch: [{
      op: "set", path: "/op", value: operation("c2", { kind: "list", input: { cursor: nextCursor, revision } }),
    }] } });
    expect(requests).toHaveLength(2);
    await act(async () => replies[1](new Response(JSON.stringify({
      op: "c2",
      result: { revision, nextCursor: null, sessions: [{ sessionId: "B", cwd: targetCwd, title: "Target workspace" }] },
    }), { status: 200 })));

    expect(acp.projectCwd).toBe(targetCwd);
    expect(window.location.pathname).toBe(projectPath(targetCwd));
    expect(acp.state.sessions.filter((session) => session.cwd === targetCwd)).toEqual([
      { sessionId: "B", cwd: targetCwd, title: "Target workspace" },
    ]);
    expect(acp.state.nextSessionCursor).toBeNull();
    expect(requests).toHaveLength(2);
    expect(inFlight).toBe(0);
    expect(maximumInFlight).toBe(1);
  });
});
