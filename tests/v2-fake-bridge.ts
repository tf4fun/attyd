// A scripted protocol v2 bridge for browser tests: state documents, a
// replication stream with owner-qualified scopes, and the commands the web
// UI sends. It commits every change as `ops`, as the real commit step does.

import type { SessionInfo, SessionUpdate } from "@agentclientprotocol/sdk";
import type {
  Entry,
  GlobalState,
  Op,
  Request,
  SessionState,
} from "../shared/protocol";
import { applyOps } from "../shared/replica";

export interface RecordedRequest {
  method: string;
  path: string;
  headers: Record<string, string>;
  body: unknown;
}

export class FakeEventSource {
  readyState = 0;
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  constructor(readonly url: string) {}
  close(): void { this.readyState = 2; }
  get subscriptions(): string[] { return new URL(this.url, "http://test").searchParams.getAll("scope"); }
  deliver(entry: Entry, id: string): void {
    if (this.readyState === 2) return;
    this.onmessage?.({ data: JSON.stringify(entry), lastEventId: id } as MessageEvent);
  }
}

export function globalState(): GlobalState {
  return {
    agent: {
      phase: "ready",
      error: null,
      launch: { transport: "stdio", command: ["agent"], cwd: "/work", readOnly: false, additionalDirectories: [], mcpServers: [] },
      initialize: {
        protocolVersion: 1,
        agentCapabilities: { loadSession: true, sessionCapabilities: { list: {}, delete: {} } },
        authMethods: [],
      },
      authenticatedMethodId: null,
      predecessor: null,
    },
    catalog: { revision: "1" },
    sessions: {},
    requests: {},
    interactions: {},
    urlFlows: {},
  };
}

export function sessionState(sessionId: string, cwd: string, incarnation = 1): SessionState {
  return {
    sessionId,
    incarnation,
    cwd,
    controls: { modes: null, configOptions: null, commands: [], info: { title: null, updatedAt: null, meta: null }, usage: null },
    history: { revision: "r1", phase: "ready", error: null, notice: null, timeline: [], collapsedTurns: [] },
    turn: null,
    outcomes: {},
    operation: null,
    interactions: {},
    urlFlows: {},
    terminals: {},
  };
}

export class FakeBridge {
  epoch = "e1";
  seq = 1;
  global = globalState();
  sessions = new Map<string, SessionState>();
  catalog: SessionInfo[] = [];
  streams: FakeEventSource[] = [];
  requests: RecordedRequest[] = [];
  /** Per-path overrides, consulted before the default handlers. */
  overrides: Array<(request: RecordedRequest) => Response | Promise<Response> | undefined> = [];
  private nextId = 1;

  eventSource = (url: string) => {
    const source = new FakeEventSource(url);
    this.streams.push(source);
    queueMicrotask(() => this.start(source));
    return source;
  };

  fetch = async (input: RequestInfo | URL, init: RequestInit = {}): Promise<Response> => {
    const url = new URL(String(input), "http://test");
    const headers: Record<string, string> = {};
    const source = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
    for (const name of ["idempotency-key", "if-match", "content-type"]) {
      const value = source.get(name);
      if (value != null) headers[name] = value;
    }
    const request: RecordedRequest = {
      method: (init.method ?? "GET").toUpperCase(),
      path: url.pathname + url.search,
      headers,
      body: typeof init.body === "string" ? JSON.parse(init.body) : undefined,
    };
    this.requests.push(request);
    for (const override of this.overrides) {
      const response = await override(request);
      if (response != null) return response;
    }
    return this.serve(request);
  };

  /** Answers a request with the default handlers, bypassing overrides. */
  serve(request: RecordedRequest): Response {
    return this.handle(request, new URL(request.path, "http://test"));
  }

  get stream(): FakeEventSource {
    return this.streams.at(-1)!;
  }

  commands(method: string, prefix: string): RecordedRequest[] {
    return this.requests.filter((request) => request.method === method && request.path.startsWith(prefix));
  }

  /** Commits `ops` to a scope and delivers them to every open stream that carries it. */
  commit(scope: string, ops: Op[]): void {
    const previous = this.seq;
    this.seq += 1;
    if (scope === "global") this.global = applyOps(this.global, ops) as GlobalState;
    else {
      const sessionId = scope.slice("session/".length);
      this.sessions.set(sessionId, applyOps(this.sessions.get(sessionId), ops) as SessionState);
    }
    const last = this.lastSeq.get(scope) ?? previous;
    this.lastSeq.set(scope, this.seq);
    for (const stream of this.streams) {
      if (stream.readyState !== 1 || !stream.subscriptions.some((subscription) => subscriptionScope(subscription) === scope)) continue;
      stream.deliver({ type: "ops", scope, at: { epoch: this.epoch, seq: this.seq }, prev: last, ops }, `${this.epoch}:${this.seq}`);
    }
  }

  private lastSeq = new Map<string, number>();

  /** Ends a session scope for every stream that carries it. */
  end(sessionId: string, reason: "closed" | "deleted"): void {
    this.sessions.delete(sessionId);
    this.seq += 1;
    for (const stream of this.streams) {
      if (stream.readyState !== 1 || !stream.subscriptions.some((subscription) => subscriptionScope(subscription) === `session/${sessionId}`)) continue;
      stream.deliver({ type: "end", scope: `session/${sessionId}`, at: { epoch: this.epoch, seq: this.seq }, reason }, `${this.epoch}:${this.seq}`);
    }
    this.commit("global", [{ op: "remove", path: `/sessions/${pointer(sessionId)}` }]);
  }

  /** The Agent connection ends; streams fail and the next one sees `epoch`. */
  restart(epoch: string): void {
    for (const stream of this.streams) {
      stream.readyState = 2;
      stream.onerror?.({} as Event);
    }
    this.epoch = epoch;
    this.seq = 1;
    this.lastSeq.clear();
    this.sessions.clear();
    this.global = { ...globalState(), agent: { ...globalState().agent, predecessor: { epoch: "e1", reason: "agent_exit" } } };
  }

  /** Holds a session, as a create or open does. */
  hold(sessionId: string, cwd: string, configure: (state: SessionState) => SessionState = (state) => state): SessionState {
    const incarnation = (this.global.sessions[sessionId]?.incarnation ?? 0) + 1;
    const state = configure(sessionState(sessionId, cwd, incarnation));
    this.sessions.set(sessionId, state);
    this.lastSeq.set(`session/${sessionId}`, this.seq + 1);
    this.commit("global", [{ op: "set", path: `/sessions/${pointer(sessionId)}`, value: {
      sessionId, incarnation, cwd, title: null, lifecycle: "active", phase: "ready", work: "idle",
      pending: { permissions: 0, elicitations: 0, urlFlows: 0 },
    } }]);
    return state;
  }

  /** Process bodies of collapsed turns, by turn ID. */
  processes = new Map<string, SessionUpdate[]>();

  /**
   * Commits the running turn: its prompt and final answer join the compact
   * timeline, the rest becomes the turn's process page.
   */
  completeTurn(sessionId: string, revision: string): void {
    const state = this.sessions.get(sessionId)!;
    const turn = state.turn!;
    const final = turn.updates.at(-1)!;
    const prompt = turn.prompt.map((content): SessionUpdate => ({ sessionUpdate: "user_message_chunk", content }));
    const turnId = `turn-${turn.operationId}`;
    this.processes.set(turnId, turn.updates.slice(0, -1));
    const before = state.history.timeline.length;
    this.commit(`session/${sessionId}`, [
      { op: "append", path: "/history/timeline", values: [...prompt, final] },
      { op: "append", path: "/history/collapsedTurns", values: [{
        turnId, operationId: turn.operationId, beforeUpdate: before, afterUpdate: before + prompt.length + 1,
        processCount: turn.updates.length - 1,
        visibleRanges: [{ start: before, end: before + prompt.length }, { start: before + prompt.length, end: before + prompt.length + 1 }],
      }] },
      { op: "set", path: "/history/revision", value: revision },
      { op: "set", path: "/history/phase", value: "ready" },
      { op: "set", path: `/outcomes/${pointer(turn.operationId)}`, value: {
        operationId: turn.operationId, clientIntentId: turn.clientIntentId, status: "completed",
        response: { stopReason: "end_turn" }, afterUpdate: before + prompt.length + 1,
      } },
      { op: "set", path: "/turn", value: null },
    ]);
  }

  appendTurnUpdate(sessionId: string, update: SessionUpdate): void {
    this.commit(`session/${sessionId}`, [{ op: "append", path: "/turn/updates", values: [update] }]);
  }

  private start(source: FakeEventSource): void {
    if (source.readyState === 2) return;
    source.readyState = 1;
    source.onopen?.({} as Event);
    const entries: Entry[] = [];
    for (const subscription of source.subscriptions) {
      const scope = subscriptionScope(subscription);
      const at = { epoch: this.epoch, seq: this.seq };
      if (scope === "global") {
        entries.push({ type: "snapshot", scope, at, state: this.global });
        continue;
      }
      const sessionId = scope.slice("session/".length);
      const owner = subscription.slice(subscription.lastIndexOf("@") + 1);
      const held = this.sessions.get(sessionId);
      if (held != null && owner === `${this.epoch}:${held.incarnation}`) entries.push({ type: "reset", scope, at });
      else entries.push({ type: "end", scope, at, reason: held == null && owner.startsWith(`${this.epoch}:`) ? "closed" : "replaced" });
    }
    entries.forEach((entry, index) => source.deliver(entry, index === entries.length - 1 ? `${this.epoch}:${this.seq}` : ""));
  }

  private record(fields: Partial<Request> & Pick<Request, "operation" | "method">, key: string): Request {
    const requestId = `R${this.nextId++}`;
    return {
      requestId, clientIntentId: key, target: null, interactionId: null, methodId: null, params: {},
      status: "running", ...fields,
    };
  }

  /** Commits a request record and answers the command with it. */
  accept(record: Request, scopes = ["global"]): Response {
    this.commit("global", [{ op: "set", path: `/requests/${record.requestId}`, value: record }]);
    return json({ at: { epoch: this.epoch, seq: this.seq }, scopes, result: { requestId: record.requestId, disposition: "accepted" } });
  }

  finish(requestId: string, fields: Partial<Request>): void {
    this.commit("global", [{ op: "set", path: `/requests/${requestId}`, value: { ...this.global.requests[requestId], ...fields } }]);
  }

  private handle(request: RecordedRequest, url: URL): Response {
    const { pathname } = url;
    const key = request.headers["idempotency-key"] ?? "";
    const body = request.body as Record<string, unknown> | undefined;
    if (request.method === "GET" && pathname === "/api/v2/global") {
      return json({ at: { epoch: this.epoch, seq: this.seq }, state: this.global });
    }
    if (request.method === "GET" && pathname === "/api/v2/catalog") {
      return json({ revision: this.global.catalog.revision, sessions: this.catalog, nextCursor: null });
    }
    if (request.method === "POST" && pathname === "/api/v2/agent/auth-terminals") {
      return this.accept(this.record({
        operation: "auth_terminal", method: "attyd/auth_terminal", methodId: String(body?.methodId),
        terminal: { output: "", truncated: false, exitCode: null, signal: null, message: null },
      }, key));
    }
    if (request.method === "POST" && /^\/api\/v2\/agent\/auth-terminals\/[^/]+\/(input|resize|cancel)$/u.test(pathname)) {
      return json({ at: { epoch: this.epoch, seq: this.seq }, scopes: ["global"], result: {} });
    }
    const sessionMatch = /^\/api\/v2\/sessions\/([^/]+)(\/.*)?$/u.exec(pathname);
    if (request.method === "POST" && pathname === "/api/v2/sessions") {
      const sessionId = `created-${this.nextId}`;
      const cwd = String(body?.cwd);
      const record = this.record({ operation: "create", method: "session/new", params: { cwd } }, key);
      const response = this.accept(record);
      const held = this.hold(sessionId, cwd);
      this.finish(record.requestId, { status: "succeeded", result: { kind: "session", sessionId, incarnation: held.incarnation } });
      return response;
    }
    if (sessionMatch == null) return json({ error: { code: "not_found", message: `no route ${pathname}` } }, 404);
    const sessionId = decodeURIComponent(sessionMatch[1]);
    const rest = sessionMatch[2] ?? "";
    const held = this.sessions.get(sessionId);
    if (request.method === "GET" && rest === "") {
      return held == null
        ? json({ error: { code: "not_found", message: "not held" } }, 404)
        : json({ at: { epoch: this.epoch, seq: this.seq }, state: held });
    }
    if (request.method === "POST" && rest === "/open") {
      const cwd = String(body?.cwd);
      const state = held ?? this.hold(sessionId, cwd);
      const target = { sessionId, incarnation: state.incarnation };
      const record = this.record({ operation: "open", method: "session/load", target, params: { cwd } }, key);
      return this.accept({ ...record, status: "succeeded", result: { kind: "session", ...target } }, ["global", `session/${sessionId}`]);
    }
    if (held == null) return json({ error: { code: "not_found", message: "not held" } }, 404);
    if (request.method === "POST" && rest === "/turns") {
      const operationId = `op-${this.nextId++}`;
      this.commit(`session/${sessionId}`, [
        { op: "set", path: "/turn", value: { operationId, clientIntentId: key, prompt: body?.prompt, updates: [], cancelRequested: false } },
        { op: "set", path: "/history/phase", value: "running" },
      ]);
      return json({ at: { epoch: this.epoch, seq: this.seq }, scopes: [`session/${sessionId}`], result: { operationId, disposition: "accepted" } });
    }
    const page = /^\/history\/([^/]+)\/turns\/([^/]+)\/process$/u.exec(rest);
    if (request.method === "GET" && page != null) {
      const turn = held.history.collapsedTurns.find(({ turnId }) => turnId === decodeURIComponent(page[2]));
      if (decodeURIComponent(page[1]) !== held.history.revision) return json({ error: { code: "gone", message: "gone" } }, 410);
      if (turn == null) return json({ error: { code: "not_found", message: "no turn" } }, 404);
      const items = (this.processes.get(turn.turnId) ?? []).map((update) => [update]);
      return json({
        bridgeEpoch: this.epoch, sessionId, sessionIncarnation: held.incarnation, turnId: turn.turnId,
        historyRevision: held.history.revision, offset: 0, total: items.length, nextOffset: null, items, terminals: {},
      });
    }
    if (request.method === "PUT" && rest === "/mode") {
      const record = this.record({ operation: "set_mode", method: "session/set_mode", target: { sessionId, incarnation: held.incarnation }, params: { modeId: body?.modeId } }, key);
      const response = this.accept(record);
      this.commit(`session/${sessionId}`, [{ op: "set", path: "/controls/modes/currentModeId", value: body?.modeId }]);
      this.finish(record.requestId, { status: "succeeded", result: { kind: "done" } });
      return response;
    }
    if (request.method === "POST" && rest === "/close") {
      const record = this.record({ operation: "close", method: "session/close", target: { sessionId, incarnation: held.incarnation } }, key);
      const response = this.accept(record);
      this.end(sessionId, "closed");
      this.finish(record.requestId, { status: "succeeded", result: { kind: "done" } });
      return response;
    }
    return json({ error: { code: "not_found", message: `no route ${request.method} ${pathname}` } }, 404);
  }
}

function subscriptionScope(subscription: string): string {
  return subscription === "global" ? subscription : subscription.slice(0, subscription.lastIndexOf("@"));
}

export function pointer(token: string): string {
  return token.replaceAll("~", "~0").replaceAll("/", "~1");
}

export function json(value: unknown, status = 200): Response {
  return new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json" } });
}
