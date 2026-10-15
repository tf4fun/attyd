// @vitest-environment happy-dom

// The v2 `useAcp` hook against a fake stream and HTTP endpoint: its
// connection lifecycle, and the page-local rules that settle its own writes.
import { act, StrictMode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Event, GlobalLive, Interaction, Operation, SessionLive } from "../shared/protocol";
import { H0 } from "../shared/replica";
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
}

interface Post {
  url: string;
  body: { expect: string; kind: string; args: Record<string, unknown> };
}

let posts: Post[];
let respond: (post: Post) => { status: number; body: unknown } | Promise<{ status: number; body: unknown }>;

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

async function emit(event: Event, source = current()): Promise<void> {
  await act(async () => source.onmessage?.({ data: JSON.stringify(event) }));
  await settle();
}

function op(fields: Partial<Operation> & { id: string }): Operation {
  return { kind: "none", status: "completed", input: null, cancelRequested: false, interactions: {}, ...fields };
}

function globalLive(fields: Partial<GlobalLive> = {}): GlobalLive {
  return {
    op: op({ id: "g0" }),
    urlFlows: {},
    agent: {
      phase: "ready",
      error: null,
      launch: { transport: "stdio", command: ["agent"], cwd: "/w", readOnly: false, additionalDirectories: [], mcpServers: [] },
      initialize: { protocolVersion: 1, agentCapabilities: { sessionCapabilities: { list: {}, delete: {} } } },
      authenticatedMethodId: null,
    },
    sessions: {},
    terminals: {},
    ...fields,
  } as GlobalLive;
}

function sessionLive(sessionOp: Operation): SessionLive {
  return {
    op: sessionOp,
    urlFlows: {},
    sessionId: "s1",
    cwd: "/w",
    controls: { modes: null, configOptions: null, commands: [], info: { title: null, updatedAt: null, meta: null }, usage: null },
    terminals: {},
  };
}

const held = {
  s1: { sessionId: "s1", cwd: "/w", title: "One", op: { kind: "prompt" as const, status: "running" as const }, pending: { interactions: 0, urlFlows: 0 } },
};

function permission(interactionId: string): Interaction {
  return {
    interactionId,
    kind: "permission",
    request: {
      sessionId: "s1",
      toolCall: { toolCallId: `tool-${interactionId}`, title: `Tool ${interactionId}` },
      options: [{ optionId: "allow", name: "Allow once", kind: "allow_once" }],
    },
  };
}

const align = (scope: string, live: unknown): Event => ({ scope, log: { from: 0, prev: H0, entries: [] }, live: { replace: live } });

let hook: ReturnType<typeof useAcp>;
function Probe() {
  hook = useAcp();
  return null;
}

describe("v2 workspace hook", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    FakeEventSource.sources = [];
    posts = [];
    respond = () => ({ status: 202, body: { op: "accepted" } });
    vi.stubGlobal("EventSource", FakeEventSource);
    vi.stubGlobal("fetch", vi.fn(async (url: string, init?: { method?: string; body?: string }) => {
      if (init?.method !== "POST") return new Response("{}", { status: 404 });
      const post = { url, body: JSON.parse(init.body ?? "{}") as Post["body"] };
      posts.push(post);
      const { status, body } = await respond(post);
      return new Response(JSON.stringify(body), { status });
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

  const lists = () => posts.filter((post) => post.body.kind === "list");
  const answers = () => posts.filter((post) => post.body.kind === "answer");

  async function mount(path: string, strict = false) {
    window.history.replaceState(null, "", path);
    await act(async () => root.render(strict ? <StrictMode><Probe /></StrictMode> : <Probe />));
    await settle();
  }

  it("offers held sessions as open, whether or not the Agent can load them", async () => {
    await mount("/");
    await emit(align("global", globalLive({ sessions: held })));
    expect(hook.state.openSessionIds).toEqual(["s1"]);
  });

  it("answers a catalog interaction while a cold session waits for its cwd", async () => {
    let finishList!: () => void;
    respond = (post) => post.body.kind === "list"
      ? new Promise((resolve) => {
        finishList = () => resolve({ status: 200, body: { op: "l1", result: { revision: "r1", sessions: [{ sessionId: "cold", cwd: "/w" }], nextCursor: null } } });
      })
      : { status: 202, body: { op: "l1" } };
    await mount("/sessions/cold");
    await emit(align("global", globalLive()));
    await emit(align("catalog", { op: op({ id: "c0" }), urlFlows: {}, revision: "r1" }));
    expect(lists()).toHaveLength(1);
    const form: Interaction = {
      interactionId: "E", kind: "elicitation",
      request: { mode: "form", message: "Pick", requestedSchema: { type: "object", properties: {} } } as never,
    };
    await emit({ scope: "catalog", live: { patch: [{ op: "set", path: "/op", value: op({
      id: "l1", kind: "list", status: "running", input: {}, interactions: { E: form },
    }) }] } });
    expect(hook.sessionOpening).toMatchObject({ sessionId: "cold" });
    expect(hook.state.elicitations.map(({ elicitationId }) => elicitationId)).toEqual(["E"]);
    await act(async () => hook.respondElicitation("E", { action: "accept", content: {} }));
    await settle();
    expect(answers().map((post) => [post.url, post.body.expect])).toEqual([["/api/v2/scopes/catalog/ops", "l1"]]);
    finishList();
    await settle();
    await emit({ scope: "catalog", live: { patch: [{ op: "set", path: "/op", value: op({ id: "l1", kind: "list" }) }] } });
    const loading = current();
    expect(loading.scopes).toContain("session/cold");
  });

  it("lets the user leave while a fork's response is held, and keeps them there when it arrives", async () => {
    let finishFork!: () => void;
    respond = (post) => post.body.kind === "fork"
      ? new Promise((resolve) => {
        finishFork = () => resolve({ status: 200, body: { op: "f1", result: { sessionId: "forked" } } });
      })
      : { status: 202, body: { op: "x" } };
    await mount("/sessions/s1");
    await emit(align("global", globalLive({ sessions: held })));
    await emit(align("catalog", { op: op({ id: "c0" }), urlFlows: {}, revision: "r1" }));
    await emit(align("session/s1", sessionLive(op({ id: "o1", kind: "open" }))));
    await act(async () => hook.forkSession());
    await settle();
    expect(posts.filter((post) => post.body.kind === "fork")).toHaveLength(1);
    expect(hook.state.sessionTransition).toBeDefined();
    await act(async () => hook.attachSession({ sessionId: "s2", cwd: "/w" }));
    await settle();
    expect(window.location.pathname).toMatch(/\/sessions\/s2$/u);
    expect(hook.state.sessionTransition).toBeUndefined();
    finishFork();
    await settle();
    expect(window.location.pathname).toMatch(/\/sessions\/s2$/u);
  });

  it("keeps one working stream across a StrictMode effect replay and closes it on unmount", async () => {
    await mount("/", true);
    const source = current();
    expect(source.scopes).toEqual(["global", "catalog"]);
    await emit(align("global", globalLive()));
    expect(hook.state.phase).toBe("ready");
    await act(async () => root.unmount());
    expect(open()).toEqual([]);
    root = createRoot(container);
  });

  it("shows a known session's load failure and subscribes again on Retry", async () => {
    respond = (post) => post.body.kind === "list"
      ? { status: 200, body: { op: "c1", result: { revision: "r1", sessions: [{ sessionId: "s1", cwd: "/w", title: "One" }], nextCursor: null } } }
      : { status: 202, body: { op: "x" } };
    await mount("/sessions/s1");
    await emit(align("global", globalLive()));
    await emit(align("catalog", { op: op({ id: "c0" }), urlFlows: {}, revision: "r1" }));
    const loading = current();
    expect(loading.scopes).toContain("session/s1");
    expect(new URL(loading.url, "http://test").searchParams.getAll("cwd")).toEqual(["session/s1:/w"]);
    await emit({ scope: "session/s1", end: { reason: "load_failed", error: { code: "agent_error", message: "Synthetic load failure" } } }, loading);
    expect(hook.sessionOpening).toMatchObject({ sessionId: "s1", error: "Synthetic load failure" });
    const before = FakeEventSource.sources.filter((source) => source.scopes.includes("session/s1")).length;
    await act(async () => hook.retrySessionOpening());
    await settle();
    expect(FakeEventSource.sources.filter((source) => source.scopes.includes("session/s1"))).toHaveLength(before + 1);
    expect(hook.sessionOpening?.error).toBeUndefined();
  });

  it("lists again on Refresh after a definite refusal, against the same operation, once", async () => {
    let refuse = true;
    respond = (post) => post.body.kind === "list" && refuse
      ? { status: 503, body: { error: { code: "unavailable", message: "Listing is down" } } }
      : { status: 200, body: { op: "c1", result: { revision: "r1", sessions: [], nextCursor: null } } };
    await mount("/");
    await emit(align("global", globalLive()));
    await emit(align("catalog", { op: op({ id: "c0" }), urlFlows: {}, revision: "r1" }));
    expect(lists().map((post) => post.body.expect)).toEqual(["c0"]);
    // A refusal is not retried by itself.
    await emit({ scope: "global", live: { patch: [{ op: "set", path: "/agent/error", value: null }] } });
    expect(lists()).toHaveLength(1);
    refuse = false;
    await act(async () => hook.listSessions());
    await settle();
    expect(lists().map((post) => post.body.expect)).toEqual(["c0", "c0"]);
  });

  it("sends the answers given once the pending set shrinks to them, without a second click", async () => {
    await mount("/sessions/s1");
    await emit(align("global", globalLive({ sessions: held })));
    await emit(align("catalog", { op: op({ id: "c0" }), urlFlows: {}, revision: "r1" }));
    const prompt = op({
      id: "p1", kind: "prompt", status: "running", input: { prompt: [] },
      interactions: { A: permission("A"), B: permission("B") },
    });
    await emit(align("session/s1", sessionLive(prompt)));
    expect(hook.state.permissions.map(({ permissionId }) => permissionId)).toEqual(["A", "B"]);
    await act(async () => hook.respondPermission("A", { outcome: "selected", optionId: "allow" }));
    await settle();
    expect(answers()).toEqual([]);
    // The Agent withdraws B: the answer to A now covers every pending interaction.
    await emit({ scope: "session/s1", live: { patch: [{ op: "remove", path: "/op/interactions/B" }] } });
    expect(answers().map((post) => post.body)).toEqual([{
      expect: "p1", kind: "answer", args: { answers: { A: { outcome: { outcome: "selected", optionId: "allow" } } } },
    }]);
    await emit({ scope: "session/s1", live: { patch: [{ op: "set", path: "/cwd", value: "/w" }] } });
    expect(answers()).toHaveLength(1);
  });

  it("shows an interaction of a delete that occupies global and the session once, and answers it once", async () => {
    await mount("/sessions/s1");
    const deleting = op({
      id: "d1", kind: "delete", status: "running", input: { sessionId: "s1" },
      interactions: { X: permission("X") },
    });
    await emit(align("global", globalLive({ op: deleting, sessions: held })));
    await emit(align("catalog", { op: op({ id: "c0" }), urlFlows: {}, revision: "r1" }));
    await emit(align("session/s1", sessionLive(deleting)));
    expect(hook.state.permissions.map(({ permissionId }) => permissionId)).toEqual(["X"]);
    await act(async () => hook.respondPermission("X", { outcome: "selected", optionId: "allow" }));
    await settle();
    expect(answers().map((post) => post.body.expect)).toEqual(["d1"]);
  });
});
