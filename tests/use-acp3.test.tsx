// @vitest-environment happy-dom

// The revision 3 workspace, read side, against a fake stream: its connection lifecycle and
// what the page shows of the documents (docs/protocol.md §3, §4). Snapshots carry valid
// digests, because the replica engine installs nothing it cannot confirm.
import { act, StrictMode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { entryHash, scopeDigest, sessionDigest } from "../shared/digest";
import type { StreamEvent } from "../shared/document-replica";
import type { Entry, PromptState } from "../shared/entries";
import { useAcp } from "../web/src/lib/use-acp3";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

class FakeEventSource {
  static sources: FakeEventSource[] = [];
  onmessage: ((event: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;
  constructor(readonly url: string) {
    FakeEventSource.sources.push(this);
  }
  close() {
    this.closed = true;
  }
  get scopes(): string[] {
    return new URL(this.url, "http://test").searchParams.getAll("scope");
  }
  get cwds(): string[] {
    return new URL(this.url, "http://test").searchParams.getAll("cwd");
  }
}

function open(): FakeEventSource[] {
  return FakeEventSource.sources.filter((source) => !source.closed);
}

function current(): FakeEventSource {
  const sources = open();
  expect(sources).toHaveLength(1);
  return sources[0]!;
}

async function settle(): Promise<void> {
  for (let round = 0; round < 5; round += 1) {
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
  }
}

async function emit(event: StreamEvent, source = current()): Promise<void> {
  await act(async () => source.onmessage?.({ data: JSON.stringify(event) }));
  await settle();
}

// -- documents ------------------------------------------------------------------------------

function op(scope: string, fields: Record<string, unknown> = {}) {
  return {
    id: "o0", scope, after: null, kind: "none", status: "completed", input: null, cancelRequested: false, ...fields,
  };
}

const agent = (fields: Record<string, unknown> = {}) => ({
  phase: "ready",
  error: null,
  launch: { transport: "stdio", command: ["agent"], cwd: "/w", readOnly: false, additionalDirectories: [], mcpServers: [] },
  initialize: { protocolVersion: 1, agentCapabilities: {} },
  authenticatedMethodId: null,
  ...fields,
});

function globalLive(fields: Record<string, unknown> = {}) {
  return {
    op: op("global", { id: "g0" }), interactions: {}, urlFlows: {}, agent: agent(), sessions: {}, terminals: {}, ...fields,
  };
}

function catalogLive(fields: Record<string, unknown> = {}) {
  return { op: op("catalog", { id: "c0" }), interactions: {}, urlFlows: {}, revision: "1", pages: [], ...fields };
}

const controls = () => ({
  modes: { currentModeId: "ask", availableModes: [{ id: "ask", name: "Ask" }, { id: "plan", name: "Plan" }] },
  configOptions: null,
  commands: [{ name: "plan", description: "Plan" }],
  info: { title: "One", updatedAt: null, meta: null },
  usage: null,
});

function sessionLive(fields: Record<string, unknown> = {}) {
  return {
    op: op("session/s1", { id: "o1", kind: "open" }), interactions: {}, urlFlows: {},
    sessionId: "s1", cwd: "/w", controls: controls(), terminals: {}, closing: false, ...fields,
  };
}

const text = (value: string) => ({ type: "text", text: value });
const user = (value: string): Entry => ({ kind: "message", role: "user", chunks: [{ content: text(value) }] });
const reply = (value: string): Entry => ({ kind: "message", role: "agent", chunks: [{ content: text(value) }] });

const held = {
  s1: { sessionId: "s1", cwd: "/w", title: "One", op: { kind: "open", status: "completed" }, pending: { interactions: 0, urlFlows: 0 } },
};

/** A snapshot of `scope` at `rev`, with the digest the bridge would send. */
function snapshot(scope: string, body: Record<string, unknown>, entries?: Entry[], rev = 0, epoch = "E1"): StreamEvent {
  const digest = scope.startsWith("session/")
    ? sessionDigest({ epoch, rev, ...body }, entries === undefined ? null : entries.map(entryHash))
    : scopeDigest({ epoch, rev, ...body });
  return {
    scope,
    snapshot: {
      epoch, rev, digest, doc: body,
      ...(entries === undefined ? {} : {
        entries: { base: "replace", length: entries.length, changed: entries.map((entry, index) => [index, entry] as const) },
      }),
    },
  } as StreamEvent;
}

const globalSnapshot = (live = globalLive()) => snapshot("global", { live });
const catalogSnapshot = (live = catalogLive()) => snapshot("catalog", { live });
const sessionSnapshot = (
  entries: Entry[] = [], prompts: PromptState[] = [], live = sessionLive(), history: "complete" | "partial" = "complete",
) => snapshot("session/s1", { history, prompts, live }, entries);

let hook: ReturnType<typeof useAcp>;
function Probe() {
  hook = useAcp();
  return null;
}

describe("revision 3 workspace, read side", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    FakeEventSource.sources = [];
    vi.stubGlobal("EventSource", FakeEventSource);
    vi.stubGlobal("fetch", vi.fn(async () => new Response("{}", { status: 404 })));
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
    window.history.replaceState(null, "", "/");
  });

  async function mount(path: string, strict = false) {
    window.history.replaceState(null, "", path);
    await act(async () => root.render(strict ? <StrictMode><Probe /></StrictMode> : <Probe />));
    await settle();
  }

  it("follows global and catalog on the revision 3 stream and shows the Agent", async () => {
    await mount("/");
    const source = current();
    expect(new URL(source.url, "http://test").pathname).toBe("/api/v3/stream");
    expect(source.scopes).toEqual(["global", "catalog"]);
    expect(hook.state.phase).toBe("starting");
    await emit(globalSnapshot());
    await emit(catalogSnapshot());
    expect(hook.state.phase).toBe("ready");
    expect(hook.state.command).toEqual(["agent"]);
    expect(hook.state.defaultCwd).toBe("/w");
  });

  it("keeps one working stream across a StrictMode effect replay and closes it on unmount", async () => {
    await mount("/", true);
    expect(current().scopes).toEqual(["global", "catalog"]);
    await emit(globalSnapshot());
    expect(hook.state.phase).toBe("ready");
    await act(async () => root.unmount());
    expect(open()).toEqual([]);
    root = createRoot(container);
  });

  it("offers held sessions as open, and those whose operation runs as busy", async () => {
    await mount("/");
    await emit(globalSnapshot(globalLive({
      sessions: { ...held, s2: { ...held.s1, sessionId: "s2", op: { kind: "prompt", status: "running" }, pending: { interactions: 1, urlFlows: 0 } } },
    })));
    expect(hook.state.openSessionIds).toEqual(["s1", "s2"]);
    expect(hook.state.busySessionIds).toEqual(["s2"]);
    expect(hook.state.attentionSessionIds).toEqual(["s2"]);
  });

  it("lists the sessions the catalog's pages hold, and the held ones", async () => {
    await mount("/");
    await emit(globalSnapshot(globalLive({ sessions: held })));
    await emit(catalogSnapshot(catalogLive({
      pages: [{ sessions: [{ sessionId: "a", cwd: "/a", title: "A" }], nextCursor: "c1" }, { sessions: [{ sessionId: "b", cwd: "/b" }], nextCursor: null }],
    })));
    expect(hook.state.sessions.map(({ sessionId }) => sessionId).sort()).toEqual(["a", "b", "s1"]);
    expect(hook.state.nextSessionCursor).toBeNull();
  });

  it("shows a held session's whole thread, tool calls included, and its controls", async () => {
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({ sessions: held })));
    await emit(catalogSnapshot());
    const source = current();
    expect(source.scopes).toEqual(["global", "catalog", "session/s1"]);
    await emit(sessionSnapshot(
      [user("run it"), reply("Running"), { kind: "tool", call: { toolCallId: "t1", title: "Run", status: "completed" } }, reply("Done")],
      [{ entry: 0, end: { response: { stopReason: "end_turn" } } }],
    ), source);
    expect(hook.state.timeline.map((item) => item.type)).toEqual(["message", "assistant", "tool", "assistant", "stop"]);
    expect(hook.state.session?.sessionId).toBe("s1");
    expect(hook.state.modeId).toBe("ask");
    expect(hook.state.availableCommands.map(({ name }) => name)).toEqual(["plan"]);
    expect(hook.state.title).toBe("One");
    expect(hook.state.running).toBe(false);
    expect(hook.state.sessionSyncPhase).toBe("ready");
    expect(hook.sessionOpening).toBeUndefined();
  });

  it("shows a running prompt as running, with what the Agent is doing", async () => {
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({ sessions: held })));
    await emit(catalogSnapshot());
    await emit(sessionSnapshot(
      [user("go"), reply("Working")],
      [{ entry: 0 }],
      sessionLive({ op: op("session/s1", { id: "p1", kind: "prompt", status: "running", cancelRequested: true }) }),
    ));
    expect(hook.state.running).toBe(true);
    expect(hook.state.sessionSyncPhase).toBe("running");
    expect(hook.state.pendingPrompt).toMatchObject({ requestId: "p1", sessionId: "s1", cancelRequested: true });
    expect(hook.state.agentActivity).toEqual({ kind: "responding" });
  });

  it("does not show a session while its first load fills an empty thread, and says it is opening", async () => {
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({ sessions: held })));
    await emit(catalogSnapshot());
    await emit(snapshot("session/s1", {
      prompts: [], live: sessionLive({ op: op("session/s1", { id: "l1", kind: "load", status: "running" }) }),
    }));
    expect(hook.state.session).toBeUndefined();
    expect(hook.sessionOpening).toEqual({ sessionId: "s1" });
  });

  it("canonicalizes the URL to the session's own project", async () => {
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({ sessions: held })));
    await emit(catalogSnapshot());
    await emit(sessionSnapshot([user("hi")]));
    expect(window.location.pathname).toMatch(/\/sessions\/s1$/u);
    expect(window.location.pathname).toContain("w");
  });

  it("loads an unheld session the catalog lists, with the cwd the catalog gives", async () => {
    await mount("/sessions/cold");
    await emit(globalSnapshot());
    await emit(catalogSnapshot(catalogLive({ pages: [{ sessions: [{ sessionId: "cold", cwd: "/cold" }], nextCursor: null }] })));
    const source = current();
    expect(source.scopes).toEqual(["global", "catalog", "session/cold"]);
    expect(source.cwds).toEqual(["session/cold:/cold"]);
  });

  it("goes home when no one knows the session", async () => {
    await mount("/sessions/nowhere");
    await emit(globalSnapshot());
    await emit(catalogSnapshot());
    expect(window.location.pathname).toBe("/");
    expect(current().scopes).toEqual(["global", "catalog"]);
  });

  it("shows a known session's load failure and subscribes again on Retry", async () => {
    await mount("/sessions/s1");
    await emit(globalSnapshot());
    await emit(catalogSnapshot(catalogLive({ pages: [{ sessions: [{ sessionId: "s1", cwd: "/w", title: "One" }], nextCursor: null }] })));
    const loading = current();
    expect(loading.scopes).toContain("session/s1");
    await emit({ scope: "session/s1", end: { reason: "load_failed", error: { code: "agent_error", message: "Synthetic load failure" } } }, loading);
    expect(hook.sessionOpening).toMatchObject({ sessionId: "s1", error: "Synthetic load failure" });
    const before = FakeEventSource.sources.filter((source) => source.scopes.includes("session/s1")).length;
    await act(async () => hook.retrySessionOpening());
    await settle();
    expect(FakeEventSource.sources.filter((source) => source.scopes.includes("session/s1"))).toHaveLength(before + 1);
    expect(hook.sessionOpening?.error).toBeUndefined();
  });

  it("returns to the project when the session it shows is deleted", async () => {
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({ sessions: held })));
    await emit(catalogSnapshot());
    await emit(sessionSnapshot([user("hi")]));
    await emit({ scope: "session/s1", end: { reason: "deleted" } });
    expect(window.location.pathname).not.toMatch(/sessions\/s1/u);
    expect(window.location.pathname).toContain("w");
  });

  it("asks for sign-in when a load failed for want of authentication", async () => {
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({
      agent: agent({ initialize: { protocolVersion: 1, agentCapabilities: {}, authMethods: [{ id: "m", name: "Login" }] } }),
    })));
    await emit(catalogSnapshot(catalogLive({ pages: [{ sessions: [{ sessionId: "s1", cwd: "/w" }], nextCursor: null }] })));
    await emit(
      { scope: "session/s1", end: { reason: "load_failed", error: { code: "agent_error", message: "auth", data: { code: -32_000, message: "auth" } } } },
    );
    expect(hook.state.authStatus).toBe("required");
    expect(hook.sessionOpening?.error).toBeUndefined();
  });

  it("shows each pending interaction once, whichever scope shows it, and url flows", async () => {
    const permission = {
      interactionId: "X", kind: "permission",
      request: { sessionId: "s1", toolCall: { toolCallId: "t", title: "T" }, options: [{ optionId: "allow", name: "Allow", kind: "allow_once" }] },
    };
    const flow = {
      interactionId: "F", elicitationId: "e", request: { mode: "url", message: "Sign in", url: "https://example.test/", elicitationId: "e" },
    };
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({
      op: op("global", { id: "d1", kind: "delete", status: "running" }), interactions: { X: permission }, sessions: held, urlFlows: { F: flow },
    })));
    await emit(catalogSnapshot());
    await emit(sessionSnapshot([user("hi")], [], sessionLive({ interactions: { X: permission } })));
    expect(hook.state.permissions.map(({ permissionId }) => permissionId)).toEqual(["X"]);
    expect(hook.state.externalFlows.map(({ elicitationId }) => elicitationId)).toEqual(["F"]);
  });

  it("shows a login terminal and its output, and Agent terminals the thread does not show", async () => {
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({
      sessions: held,
      terminals: { L: { terminalId: "L", output: "Open the URL", truncated: false, exitStatus: null, methodId: "m", released: false } },
    })));
    await emit(catalogSnapshot());
    await emit(sessionSnapshot([user("hi")], [], sessionLive({
      terminals: { T: { terminalId: "T", output: "out", truncated: false, exitStatus: null, released: false } },
    })));
    expect(hook.state.authTerminals).toMatchObject([{ requestId: "L", methodId: "m", status: "running", output: "Open the URL" }]);
    expect(hook.state.standaloneTerminals.map(({ terminalId }) => terminalId)).toEqual(["T"]);
  });

  it("keeps the frozen output of a released terminal and says so", async () => {
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({ sessions: held })));
    await emit(catalogSnapshot());
    await emit(sessionSnapshot([user("hi")], [], sessionLive({
      terminals: { T: { terminalId: "T", output: "done", truncated: false, exitStatus: { exitCode: 0, signal: null }, released: true } },
    })));
    expect(hook.state.terminalSnapshots).toMatchObject([{ terminalId: "T", output: "done", released: true }]);
  });
});
