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
import { useAcp } from "../web/src/lib/use-acp";

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
  meta: {},
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

interface Post {
  url: string;
  body: { id?: string; expect: string; kind: string; args: Record<string, unknown> };
}

const LOST = Symbol("lost");
let posts: Post[];
/** What a `GET` of `url` answers; unset, every read is `404`. */
let read: ((url: string) => { status: number; body: unknown } | undefined) | undefined;
let respond: (post: Post) => { status: number; body?: unknown } | typeof LOST | Promise<{ status: number; body?: unknown } | typeof LOST>;

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
    posts = [];
    read = undefined;
    respond = (post) => ({ status: 202, body: { op: post.body.id } });
    vi.stubGlobal("EventSource", FakeEventSource);
    vi.stubGlobal("fetch", vi.fn(async (url: string, init?: { method?: string; body?: string }) => {
      if (init?.method !== "POST") {
        const answer = read?.(url);
        return answer === undefined
          ? new Response("{}", { status: 404 })
          : new Response(JSON.stringify(answer.body), { status: answer.status });
      }
      const post = { url, body: JSON.parse(init.body ?? "{}") as Post["body"] };
      posts.push(post);
      const answer = await respond(post);
      if (answer === LOST) throw new TypeError("network error");
      return new Response(answer.body === undefined ? null : JSON.stringify(answer.body), { status: answer.status });
    }));
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
    expect(new URL(source.url, "http://test").pathname).toBe("/api/v2/stream");
    expect(source.scopes).toEqual(["global", "catalog"]);
    expect(hook.state.phase).toBe("starting");
    await emit(globalSnapshot());
    await emit(catalogSnapshot());
    expect(hook.state.phase).toBe("ready");
    expect(hook.state.command).toEqual(["agent"]);
    expect(hook.state.defaultCwd).toBe("/w");
  });

  it("restarts the Agent when the page reconnects and the Agent is stopped", async () => {
    await mount("/");
    await emit(globalSnapshot(globalLive({ agent: agent({ phase: "stopped" }) })));
    await emit(catalogSnapshot());
    expect(hook.state.phase).toBe("stopped");
    respond = () => ({ status: 204 });
    await act(async () => hook.reconnect());
    expect(posts.map(({ url }) => url)).toEqual(["/api/v2/scopes/global/actions/restart"]);
    expect(posts[0].body).toEqual({});
  });

  it("only follows again when the Agent is ready: no restart is sent", async () => {
    await mount("/");
    await emit(globalSnapshot());
    await emit(catalogSnapshot());
    await act(async () => hook.reconnect());
    expect(posts).toEqual([]);
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

  // -- writes: prompts, the queue, cancel ---------------------------------------------------

  const running = (id = "p1") => sessionLive({
    op: op("session/s1", { id, kind: "prompt", status: "running" }),
  });

  /** A shown session, idle or with a prompt running. */
  async function shownSession(live = sessionLive(), prompts: PromptState[] = []) {
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({ sessions: held })));
    await emit(catalogSnapshot());
    await emit(sessionSnapshot([user("earlier")], prompts, live));
  }

  /** The session's `op` replaced, as the bridge publishes it. */
  const opChanged = (rev: number, fields: Record<string, unknown>): StreamEvent => ({
    scope: "session/s1", rev, ops: [{ op: "set", path: "/live/op", value: op("session/s1", fields) }],
  });

  it("sends a prompt to an idle session with an id of its own, against the op the document shows", async () => {
    await shownSession();
    await act(async () => hook.prompt([text("hi")]));
    await settle();
    expect(posts).toHaveLength(1);
    expect(posts[0].url).toBe("/api/v2/scopes/session%2Fs1/ops");
    expect(posts[0].body).toMatchObject({ expect: "o1", kind: "prompt", args: { prompt: [text("hi")] } });
    expect(posts[0].body.id).toMatch(/^[A-Za-z0-9_-]{22}$/u);
    expect(hook.state.queue).toEqual([]);
  });

  it("holds a prompt typed while a turn runs, and sends it once the document shows the op ended", async () => {
    await shownSession(running(), [{ entry: 0 }]);
    await act(async () => hook.prompt([text("next")]));
    await settle();
    expect(posts).toEqual([]);
    expect(hook.state.queue).toMatchObject([{ state: "queued", sessionId: "s1", blocks: [text("next")] }]);
    await emit(opChanged(1, { id: "p1", kind: "prompt", status: "completed" }));
    expect(posts.map((post) => post.body.expect)).toEqual(["p1"]);
    expect(hook.state.queue).toEqual([]);
  });

  it("keeps a prompt the bridge refused, with the reason, and sends it again only against a later op", async () => {
    respond = () => ({ status: 409, body: { error: { code: "stale", message: "Not the current operation" } } });
    await shownSession();
    await act(async () => hook.prompt([text("hi")]));
    await settle();
    expect(hook.state.queue).toMatchObject([{ state: "refused", error: "Not the current operation" }]);
    expect(posts).toHaveLength(1);
    await emit(opChanged(1, { id: "o2", kind: "prompt", status: "completed" }));
    expect(posts.map((post) => post.body.expect)).toEqual(["o1", "o2"]);
  });

  it("never sends again a prompt whose answer was lost", async () => {
    respond = () => LOST;
    await shownSession();
    await act(async () => hook.prompt([text("hi")]));
    await settle();
    expect(hook.state.queue).toMatchObject([{ state: "uncertain" }]);
    await emit(opChanged(1, { id: "o2", kind: "prompt", status: "completed" }));
    expect(posts).toHaveLength(1);
  });

  it("takes a queued prompt back, pauses the queue and clears it", async () => {
    await shownSession(running(), [{ entry: 0 }]);
    await act(async () => hook.prompt([text("one")]));
    await act(async () => hook.prompt([text("two")]));
    const [first] = hook.state.queue;
    let taken: unknown;
    await act(async () => { taken = hook.takeQueued(first.id); });
    expect(taken).toEqual([text("one")]);
    // The next prompt moves up to the write queue once the first left it.
    expect(hook.state.queue.map((entry) => entry.blocks)).toEqual([[text("two")]]);

    await act(async () => hook.pauseQueue());
    expect(hook.state.queuePaused).toBe(true);
    expect(hook.state.queue).toMatchObject([{ state: "paused" }]);
    // A paused queue sends nothing when the turn ends.
    await emit(opChanged(1, { id: "p1", kind: "prompt", status: "completed" }));
    expect(posts).toEqual([]);

    await act(async () => hook.clearQueue());
    expect(hook.state.queue).toEqual([]);
    expect(hook.state.queuePaused).toBe(false);
  });

  it("cancels the running turn against the op it shows, with no id of its own", async () => {
    await shownSession(running(), [{ entry: 0 }]);
    await act(async () => hook.cancel());
    await settle();
    expect(posts).toHaveLength(1);
    expect(posts[0].body).toEqual({ expect: "p1", kind: "cancel", args: {} });
  });

  it("does not cancel an idle session, and shows a cancel the bridge refused", async () => {
    await shownSession();
    await act(async () => hook.cancel());
    expect(posts).toEqual([]);

    respond = () => ({ status: 409, body: { error: { code: "not_allowed", message: "Nothing to cancel" } } });
    await emit(opChanged(1, { id: "p2", kind: "prompt", status: "running" }));
    await act(async () => hook.cancel());
    await settle();
    const error = hook.state.timeline.find((item) => item.type === "error");
    expect(error).toMatchObject({ operation: "cancel" });
    expect((error as { message: string }).message).toContain("Nothing to cancel");
  });

  // -- writes: create, fork, delete, authentication ---------------------------------------

  /** The `op` of `global` replaced, as the bridge publishes it. */
  const globalOpChanged = (rev: number, fields: Record<string, unknown>, more: unknown[] = []): StreamEvent => ({
    scope: "global", rev, ops: [{ op: "set", path: "/live/op", value: op("global", fields) }, ...more],
  } as StreamEvent);

  it("creates a session and goes to it when the document shows the create completed with it", async () => {
    await mount("/");
    await emit(globalSnapshot());
    await emit(catalogSnapshot());
    let started = false;
    await act(async () => { started = hook.state.phase === "ready" && true; });
    expect(started).toBe(true);
    let accepted = false;
    await act(async () => { accepted = hook.newSession("/proj"); });
    await settle();
    expect(accepted).toBe(true);
    expect(posts).toHaveLength(1);
    expect(posts[0].url).toBe("/api/v2/scopes/global/ops");
    expect(posts[0].body).toMatchObject({ expect: "g0", kind: "create", args: { cwd: "/proj" } });
    expect(hook.state.sessionTransition).toEqual({ kind: "new", requestId: "transition:new" });
    const id = posts[0].body.id!;

    await emit(globalOpChanged(1, { id, kind: "create", status: "running" }));
    expect(window.location.pathname).toBe("/");
    await emit(globalOpChanged(2, { id, kind: "create", status: "completed", result: { sessionId: "new1" } }));
    expect(window.location.pathname).toMatch(/\/sessions\/new1$/u);
    expect(hook.state.sessionTransition).toBeUndefined();
  });

  it("finds the created session by its origin when the operation was replaced before the page saw it end", async () => {
    await mount("/");
    await emit(globalSnapshot());
    await emit(catalogSnapshot());
    await act(async () => { hook.newSession("/proj"); });
    await settle();
    const id = posts[0].body.id!;
    await emit(globalOpChanged(1, { id: "someone-else", kind: "delete", status: "running" }, [
      { op: "set", path: "/live/sessions", value: { new2: { ...held.s1, sessionId: "new2", origin: id } } },
    ]));
    expect(window.location.pathname).toMatch(/\/sessions\/new2$/u);
  });

  it("does not move a page the user has left since the create was started", async () => {
    await mount("/");
    await emit(globalSnapshot());
    await emit(catalogSnapshot());
    await act(async () => { hook.newSession("/proj"); });
    await settle();
    const id = posts[0].body.id!;
    await act(async () => hook.openProject("/elsewhere"));
    expect(hook.state.sessionTransition).toBeUndefined();
    await emit(globalOpChanged(1, { id, kind: "create", status: "completed", result: { sessionId: "new1" } }));
    expect(window.location.pathname).not.toMatch(/new1/u);
  });

  it("shows why a create failed, and asks for sign-in when the Agent needs it", async () => {
    await mount("/");
    await emit(globalSnapshot(globalLive({
      agent: agent({ initialize: { protocolVersion: 1, agentCapabilities: {}, authMethods: [{ id: "m", name: "Login" }] } }),
    })));
    await emit(catalogSnapshot());
    await act(async () => { hook.newSession("/proj"); });
    await settle();
    const id = posts[0].body.id!;
    await emit(globalOpChanged(1, {
      id, kind: "create", status: "failed",
      error: { code: "agent_error", message: "no", data: { code: -32_000, message: "Authentication required" } },
    }));
    expect(hook.state.authStatus).toBe("required");
    expect(hook.state.sessionTransition).toBeUndefined();
  });

  it("shows a create the bridge refused, and starts no second create while one is in flight", async () => {
    respond = () => ({ status: 409, body: { error: { code: "stale", message: "Another operation runs" } } });
    await mount("/");
    await emit(globalSnapshot());
    await emit(catalogSnapshot());
    await act(async () => { hook.newSession("/proj"); });
    await settle();
    expect(hook.state.sessionTransition).toBeUndefined();
    const error = hook.state.timeline.find((item) => item.type === "error");
    expect((error as { message: string } | undefined)?.message).toContain("Another operation runs");

    respond = () => new Promise(() => undefined);
    let second = true;
    await act(async () => { hook.newSession("/proj"); second = hook.newSession("/proj"); });
    expect(second).toBe(false);
  });

  it("does not start a create while the Agent runs another global operation", async () => {
    await mount("/");
    await emit(globalSnapshot(globalLive({ op: op("global", { id: "g1", kind: "authenticate", status: "running" }) })));
    await emit(catalogSnapshot());
    let accepted = true;
    await act(async () => { accepted = hook.newSession("/proj"); });
    expect(accepted).toBe(false);
    expect(posts).toEqual([]);
  });

  it("says a create whose answer was lost is not known, and does not hold the page", async () => {
    respond = () => LOST;
    await mount("/");
    await emit(globalSnapshot());
    await emit(catalogSnapshot());
    await act(async () => { hook.newSession("/proj"); });
    await settle();
    expect(hook.state.sessionTransition).toBeUndefined();
    expect(hook.state.timeline.some((item) => item.type === "error")).toBe(true);
  });

  // #176: the replica may not have the commit that admitted the create yet; the bridge's
  // document, read after the answer was lost, has it.
  it("settles a create whose answer was lost from the bridge's document, not from a replica that lags", async () => {
    respond = () => LOST;
    read = (url) => url === "/api/v2/scopes/global"
      ? {
        status: 200,
        body: globalSnapshot(globalLive({ op: op("global", { id: posts[0].body.id, after: "g0", kind: "create", status: "running" }) })),
      }
      : undefined;
    await mount("/");
    await emit(globalSnapshot());
    await emit(catalogSnapshot());
    await act(async () => { hook.newSession("/proj"); });
    await settle();
    expect(hook.state.timeline.some((item) => item.type === "error")).toBe(false);
    const id = posts[0].body.id!;
    await emit(globalOpChanged(1, { id, kind: "create", status: "completed", result: { sessionId: "new1" } }));
    expect(window.location.pathname).toMatch(/\/sessions\/new1$/u);
  });

  it("forks the shown session and goes to the fork in the source's project", async () => {
    await shownSession();
    await act(async () => hook.forkSession());
    await settle();
    expect(posts[0].url).toBe("/api/v2/scopes/session%2Fs1/ops");
    expect(posts[0].body).toMatchObject({ expect: "o1", kind: "fork", args: {} });
    expect(hook.state.sessionTransition).toEqual({ kind: "fork", requestId: "transition:fork" });
    const id = posts[0].body.id!;
    await emit(opChanged(1, { id, kind: "fork", status: "running" }));
    expect(hook.state.runtimeOperation).toEqual({ requestId: id, operation: "fork" });
    await emit(opChanged(2, { id, kind: "fork", status: "completed", result: { sessionId: "forked" } }));
    expect(window.location.pathname).toMatch(/\/sessions\/forked$/u);
    expect(window.location.pathname).toContain("w");
  });

  it("deletes a session and hides it while the delete runs", async () => {
    await mount("/");
    await emit(globalSnapshot(globalLive({ sessions: held })));
    await emit(catalogSnapshot());
    await act(async () => hook.deleteSession("s1"));
    await settle();
    expect(posts[0].body).toMatchObject({ kind: "delete", args: { sessionId: "s1" } });
    const id = posts[0].body.id!;
    await emit(globalOpChanged(1, { id, kind: "delete", status: "running", input: { sessionId: "s1" } }));
    expect(hook.state.pendingSessionDeletions).toEqual([{ requestId: id, sessionId: "s1" }]);
    expect(hook.state.sessions.map(({ sessionId }) => sessionId)).not.toContain("s1");
  });

  it("starts a sign-in the Agent offers, as the kind of method it is", async () => {
    await mount("/");
    await emit(globalSnapshot(globalLive({
      agent: agent({ initialize: {
        protocolVersion: 1, agentCapabilities: {},
        authMethods: [{ id: "plain", name: "Plain" }, { id: "term", name: "Terminal", type: "terminal" }],
      } }),
    })));
    await emit(catalogSnapshot());
    await act(async () => hook.authenticate("plain"));
    await settle();
    expect(posts[0].body).toMatchObject({ kind: "authenticate", args: { methodId: "plain" } });
    await act(async () => hook.authenticate("nope"));
    await settle();
    expect(posts).toHaveLength(1);

    await emit(globalOpChanged(1, { id: posts[0].body.id!, kind: "authenticate", status: "completed", result: {} }));
    await act(async () => hook.authenticate("term"));
    await settle();
    expect(posts[1].body).toMatchObject({ kind: "terminal_login", args: { methodId: "term", cols: 80, rows: 24 } });
  });

  it("writes, resizes, cancels and dismisses a login terminal", async () => {
    await mount("/");
    await emit(globalSnapshot(globalLive({
      op: op("global", { id: "L", kind: "terminal_login", status: "running" }),
      terminals: { L: { terminalId: "L", output: "", truncated: false, exitStatus: null, methodId: "term", released: false } },
    })));
    await emit(catalogSnapshot());
    respond = () => ({ status: 204 });
    await act(async () => hook.writeAuthTerminal("L", "x"));
    await act(async () => hook.resizeAuthTerminal("L", 100, 30));
    await settle();
    expect(posts.map((post) => [post.url, post.body])).toEqual([
      ["/api/v2/scopes/global/resources/L/input", { data: "x" }],
      ["/api/v2/scopes/global/resources/L/resize", { cols: 100, rows: 30 }],
    ]);
    respond = (post) => ({ status: 202, body: { op: post.body.expect } });
    await act(async () => hook.cancelAuthTerminal("L"));
    await settle();
    expect(posts[2].body).toEqual({ expect: "L", kind: "cancel", args: {} });
    respond = () => ({ status: 204 });
    await act(async () => hook.dismissAuthTerminal("L"));
    await settle();
    expect(posts[3].url).toBe("/api/v2/scopes/global/resources/L/dismiss");
  });

  // -- interactions, controls and URL flows -------------------------------------------------

  const permissionOf = (interactionId: string) => ({
    interactionId, kind: "permission",
    request: {
      sessionId: "s1", toolCall: { toolCallId: `tool-${interactionId}`, title: `Tool ${interactionId}` },
      options: [{ optionId: "allow", name: "Allow once", kind: "allow_once" }],
    },
  });
  const allow = { outcome: "selected", optionId: "allow" } as const;
  const gone = (rev: number, id: string, scope = "session/s1"): StreamEvent => ({
    scope, rev, ops: [{ op: "remove", path: `/live/interactions/${id}` }],
  } as StreamEvent);

  it("answers one interaction alone, at its resource, whatever else is pending", async () => {
    respond = () => ({ status: 204 });
    await shownSession(sessionLive({ interactions: { A: permissionOf("A"), B: permissionOf("B") } }));
    expect(hook.state.permissions.map(({ permissionId }) => permissionId)).toEqual(["A", "B"]);
    await act(async () => hook.respondPermission("A", allow));
    await settle();
    expect(posts.map((post) => [post.url, post.body])).toEqual([
      ["/api/v2/scopes/session%2Fs1/resources/A/answer", { outcome: allow }],
    ]);
    // Sent, awaiting the document; B is untouched.
    const [a, b] = hook.state.permissions;
    expect(a.responseRequestId).toBe("A");
    expect(b.responseRequestId).toBeUndefined();
    // An interaction is answered once.
    await act(async () => hook.respondPermission("A", allow));
    await settle();
    expect(posts).toHaveLength(1);
    await emit(gone(1, "A"));
    expect(hook.state.permissions.map(({ permissionId }) => permissionId)).toEqual(["B"]);
    expect(hook.state.permissions[0].responseRequestId).toBeUndefined();
  });

  it("keeps the input and shows why when the bridge refused an answer, and lets the user answer again", async () => {
    respond = () => ({ status: 400, body: { error: { code: "invalid_request", message: "Option not offered" } } });
    await shownSession(sessionLive({ interactions: { A: permissionOf("A") } }));
    await act(async () => hook.respondPermission("A", allow));
    await settle();
    expect(hook.state.permissions[0].responseError).toContain("Option not offered");
    expect(hook.state.permissions[0].responseRequestId).toBeUndefined();
    respond = () => ({ status: 204 });
    await act(async () => hook.respondPermission("A", allow));
    await settle();
    expect(posts).toHaveLength(2);
  });

  it("shows nothing for an answer that found the interaction gone", async () => {
    respond = () => ({ status: 404, body: { error: { code: "not_found", message: "gone" } } });
    await shownSession(sessionLive({ interactions: { A: permissionOf("A") } }));
    await act(async () => hook.respondPermission("A", allow));
    await settle();
    expect(hook.state.permissions[0].responseError).toBeUndefined();
  });

  it("says an answer that may have been delivered is not known, and never sends it again", async () => {
    respond = () => LOST;
    await shownSession(sessionLive({ interactions: { A: permissionOf("A") } }));
    await act(async () => hook.respondPermission("A", allow));
    await settle();
    expect(hook.state.permissions[0].responseStatus).toBeDefined();
    await act(async () => hook.respondPermission("A", allow));
    await settle();
    expect(posts).toHaveLength(1);
  });

  it("answers an interaction of a global operation at global", async () => {
    respond = () => ({ status: 204 });
    const form = {
      interactionId: "E", kind: "elicitation",
      request: { mode: "form", message: "Pick", requestedSchema: { type: "object", properties: {} } },
    };
    await mount("/");
    await emit(globalSnapshot(globalLive({ op: op("global", { id: "c1", kind: "create", status: "running" }), interactions: { E: form } })));
    await emit(catalogSnapshot());
    await act(async () => hook.respondElicitation("E", { action: "accept", content: {} }));
    await settle();
    expect(posts.map((post) => [post.url, post.body])).toEqual([
      ["/api/v2/scopes/global/resources/E/answer", { response: { action: "accept", content: {} } }],
    ]);
  });

  it("dismisses a URL flow in the scope that shows it", async () => {
    respond = () => ({ status: 204 });
    const flow = (id: string) => ({ interactionId: id, elicitationId: id, request: { mode: "url", message: "m", url: "https://example.test/", elicitationId: id } });
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({ sessions: held, urlFlows: { G: flow("G") } })));
    await emit(catalogSnapshot());
    await emit(sessionSnapshot([user("hi")], [], sessionLive({ urlFlows: { S: flow("S") } })));
    await act(async () => hook.dismissExternalFlow("S"));
    await act(async () => hook.dismissExternalFlow("G"));
    await act(async () => hook.dismissExternalFlow("nope"));
    await settle();
    expect(posts.map((post) => post.url)).toEqual([
      "/api/v2/scopes/session%2Fs1/resources/S/dismiss",
      "/api/v2/scopes/global/resources/G/dismiss",
    ]);
  });

  it("sets a mode, held until the Agent answers, also while a prompt runs", async () => {
    let release!: () => void;
    respond = () => new Promise((resolve) => { release = () => resolve({ status: 200, body: {} }); });
    await shownSession(running(), [{ entry: 0 }]);
    await act(async () => hook.setMode("plan"));
    await settle();
    expect(posts.map((post) => [post.url, post.body])).toEqual([
      ["/api/v2/scopes/session%2Fs1/actions/set_mode", { modeId: "plan" }],
    ]);
    expect(hook.state.pendingSessionControl).toMatchObject({ sessionId: "s1", kind: "mode" });
    expect(hook.state.running).toBe(true);
    release();
    await settle();
    expect(hook.state.pendingSessionControl).toBeUndefined();
  });

  it("sets an option, and shows the error the Agent answered with, which changes nothing", async () => {
    respond = () => ({ status: 200, body: { error: { code: "agent_error", message: "Refused by the Agent" } } });
    await shownSession();
    await act(async () => hook.setConfig("verbose", true));
    await settle();
    expect(posts[0].body).toEqual({ configId: "verbose", value: true });
    const error = hook.state.timeline.find((item) => item.type === "error");
    expect((error as { message: string } | undefined)?.message).toContain("Refused by the Agent");
    expect(hook.state.pendingSessionControl).toBeUndefined();
  });

  // -- what the Agent changes while the page watches -----------------------------------------

  /** The ops of the session document at `rev`. */
  const sessionOps = (rev: number, ops: unknown[]): StreamEvent => ({ scope: "session/s1", rev, ops } as StreamEvent);

  it("shows the mode, usage, commands and title the Agent changes without a refresh, also while a prompt runs", async () => {
    await shownSession(running(), [{ entry: 0 }]);
    expect(hook.state.modeId).toBe("ask");
    expect(hook.state.running).toBe(true);
    await emit(sessionOps(1, [
      { op: "set", path: "/live/controls/modes/currentModeId", value: "plan" },
      { op: "set", path: "/live/controls/usage", value: { used: 40, size: 200, cost: null } },
      { op: "set", path: "/live/controls/commands", value: [{ name: "review", description: "Review" }] },
      { op: "set", path: "/live/controls/info/title", value: "Two" },
    ]));
    expect(hook.state.modeId).toBe("plan");
    expect(hook.state.usage).toEqual({ used: 40, size: 200, cost: null });
    expect(hook.state.availableCommands.map(({ name }) => name)).toEqual(["review"]);
    expect(hook.state.title).toBe("Two");
    expect(hook.state.running).toBe(true);
  });

  it("shows the Agent's own config updates", async () => {
    await shownSession();
    expect(hook.state.configOptions).toBeNull();
    const options = [{ type: "boolean", id: "verbose", name: "Verbose", currentValue: true }];
    await emit(sessionOps(1, [{ op: "set", path: "/live/controls/configOptions", value: options }]));
    expect(hook.state.configOptions).toEqual(options);
  });

  it("removes a URL flow the Agent completed or ended from the page", async () => {
    const flow = {
      interactionId: "F", elicitationId: "e", status: "waiting",
      request: { mode: "url", message: "Sign in", url: "https://example.test/", elicitationId: "e" },
    };
    await mount("/sessions/s1");
    await emit(globalSnapshot(globalLive({ sessions: held })));
    await emit(catalogSnapshot());
    await emit(sessionSnapshot([user("hi")], [], sessionLive({ urlFlows: { F: flow } })));
    expect(hook.state.externalFlows.map(({ elicitationId }) => elicitationId)).toEqual(["F"]);
    await emit(sessionOps(1, [{ op: "remove", path: "/live/urlFlows/F" }]));
    expect(hook.state.externalFlows).toEqual([]);
  });

  it("lists a background session as needing input from the global summary without following it", async () => {
    const asking = {
      ...held.s1,
      pending: { interactions: 1, urlFlows: 0 },
    };
    await mount("/");
    await emit(globalSnapshot(globalLive({ sessions: { s1: asking } })));
    await emit(catalogSnapshot());
    expect(current().scopes).toEqual(["global", "catalog"]);
    expect(hook.state.attentionSessionIds).toEqual(["s1"]);
    expect(hook.state.openSessionIds).toEqual(["s1"]);
  });

  // -- listing the catalog ------------------------------------------------------------------

  const listable = () => globalLive({
    agent: agent({ initialize: { protocolVersion: 1, agentCapabilities: { sessionCapabilities: { list: {} } } } }),
  });
  const lists = () => posts.filter((post) => post.body.kind === "list");
  /** The catalog's `op` replaced, and anything else its events change. */
  const catalogChanged = (rev: number, fields: Record<string, unknown>, more: unknown[] = []): StreamEvent => ({
    scope: "catalog", rev, ops: [{ op: "set", path: "/live/op", value: op("catalog", fields) }, ...more],
  } as StreamEvent);
  const page = (ids: string[], nextCursor: string | null) => ({
    sessions: ids.map((sessionId) => ({ sessionId, cwd: `/${sessionId}` })), nextCursor,
  });

  it("lists the first page when the Agent can list and the catalog has none, once per op", async () => {
    await mount("/");
    await emit(globalSnapshot(listable()));
    await emit(catalogSnapshot());
    expect(lists().map((post) => [post.url, post.body.expect, post.body.args])).toEqual([
      ["/api/v2/scopes/catalog/ops", "c0", {}],
    ]);
    // Nothing is sent again against the same op, however often the page is told.
    await emit({ scope: "global", rev: 1, ops: [{ op: "set", path: "/live/agent/error", value: null }] } as StreamEvent);
    expect(lists()).toHaveLength(1);
    // The list ran and its page is in the document: no more is listed on the home page.
    const id = lists()[0].body.id!;
    await emit(catalogChanged(1, { id, kind: "list", status: "completed", result: { revision: "1" } }, [
      { op: "set", path: "/live/pages", value: [page(["a"], "c1")] },
    ]));
    expect(lists()).toHaveLength(1);
    expect(hook.state.sessions.map(({ sessionId }) => sessionId)).toEqual(["a"]);
    expect(hook.state.nextSessionCursor).toBe("c1");
  });

  it("lists nothing when the Agent cannot list", async () => {
    await mount("/");
    await emit(globalSnapshot());
    await emit(catalogSnapshot());
    expect(lists()).toEqual([]);
  });

  it("looks for a session on the later pages, with the revision it started at, until it is found", async () => {
    await mount("/sessions/b");
    await emit(globalSnapshot(listable()));
    await emit(catalogSnapshot(catalogLive({ pages: [page(["a"], "c1")] })));
    expect(lists().map((post) => post.body.args)).toEqual([{ cursor: "c1", revision: "1" }]);
    expect(current().scopes).toEqual(["global", "catalog"]);
    const id = lists()[0].body.id!;
    await emit(catalogChanged(1, { id, kind: "list", status: "completed" }, [
      { op: "set", path: "/live/pages", value: [page(["a"], "c1"), page(["b"], null)] },
    ]));
    expect(lists()).toHaveLength(1);
    // Found: the session is loaded with the cwd the catalog gives.
    expect(current().cwds).toEqual(["session/b:/b"]);
  });

  it("goes home when the last page does not hold the session", async () => {
    await mount("/sessions/nowhere");
    await emit(globalSnapshot(listable()));
    await emit(catalogSnapshot(catalogLive({ pages: [page(["a"], null)] })));
    expect(window.location.pathname).toBe("/");
  });

  it("does not list again at a revision a list failed at, until the user asks", async () => {
    await mount("/");
    await emit(globalSnapshot(listable()));
    await emit(catalogSnapshot());
    const id = lists()[0].body.id!;
    await emit(catalogChanged(1, {
      id, kind: "list", status: "failed", error: { code: "unavailable", message: "Listing is down" },
    }));
    expect(lists()).toHaveLength(1);
    expect(hook.state.timeline.some((item) => item.type === "error")).toBe(true);
    await act(async () => hook.listSessions());
    await settle();
    expect(lists()).toHaveLength(2);
  });

  it("lists again from the start when the catalog changed while a later page was asked for", async () => {
    respond = (post) => post.body.kind === "list" && post.body.args.cursor != null
      ? { status: 409, body: { error: { code: "catalog_changed", message: "The catalog is no longer at revision 1" } } }
      : { status: 202, body: { op: post.body.id } };
    await mount("/sessions/b");
    await emit(globalSnapshot(listable()));
    await emit(catalogSnapshot(catalogLive({ pages: [page(["a"], "c1")] })));
    await settle();
    expect(lists().map((post) => post.body.args)).toEqual([{ cursor: "c1", revision: "1" }, {}]);
  });

  it("lists the first page again when the revision changes", async () => {
    await mount("/");
    await emit(globalSnapshot(listable()));
    await emit(catalogSnapshot(catalogLive({ pages: [page(["a"], null)] })));
    expect(lists()).toEqual([]);
    // A new revision publishes no pages until a list runs.
    await emit({ scope: "catalog", rev: 1, ops: [
      { op: "set", path: "/live/revision", value: "2" }, { op: "set", path: "/live/pages", value: [] },
    ] } as StreamEvent);
    expect(lists().map((post) => post.body.args)).toEqual([{}]);
  });

  it("lists a later page when the user asks for it", async () => {
    await mount("/");
    await emit(globalSnapshot(listable()));
    await emit(catalogSnapshot(catalogLive({ pages: [page(["a"], "c1")] })));
    await act(async () => hook.listSessions("c1"));
    await settle();
    expect(lists().map((post) => post.body.args)).toEqual([{ cursor: "c1", revision: "1" }]);
  });
});

