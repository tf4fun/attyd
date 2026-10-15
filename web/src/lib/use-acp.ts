// The web UI on protocol v2 (docs/protocol.md, migration step 7).
//
// One stream follows `global`, `catalog` and the session the page shows; the
// generic replica engine applies its events, and the page is a projection of
// the replicas' `log` and `live` plus this page's own UI state (C1). Writes go
// over HTTP: their answers settle only this page's own requests (a refusal
// keeps the input, C2; a write without a definite answer is never resent, C3),
// and a create or fork navigates with its own result. Nothing an HTTP answer
// says is written into a replica.

import i18n from "../i18n";
import { useEffect, useState, useSyncExternalStore } from "react";
import type {
  CreateElicitationResponse,
  RequestPermissionResponse,
  SessionInfo,
} from "@agentclientprotocol/sdk";
import type { TerminalSnapshot, WorkspaceContextAttachment, WorkspaceContextMatch } from "../../../shared/bridge";
import type {
  Answer,
  CatalogPage,
  End,
  GlobalLive,
  Interaction,
  LogEntry,
  Operation,
  OpArgs,
  PromptBlock,
  ProtocolError,
  SessionLive,
} from "../../../shared/protocol";
import {
  OpsClient,
  ReplicaClient,
  ResourceClient,
  WriteQueue,
  type HttpFetch,
  type StreamSource,
  type WriteOutcome,
} from "../../../shared/protocol-client";
import type { Follow } from "../../../shared/replica";
import { createAttachmentReader, type AttachmentReader } from "./attachments";
import { projectPath, readProjectCwdFromPath, readSessionIdFromPath, sessionPath } from "./session-route";
import {
  initialState,
  type AgentAuthResponse,
  type AgentAuthStatus,
  type AppState,
  type AuthTerminalState,
  type BridgeError,
  type PendingElicitation,
  type PendingPermission,
  type TimelineItem,
} from "./state";
import {
  catalogLive,
  externalFlow,
  globalLive,
  isRecord,
  listedSessions,
  needsAuthentication,
  operationError,
  pendingInteractions,
  projectProcess,
  projectTimeline,
  promptInput,
  sessionLive,
  sessionTerminals,
  standaloneTerminals,
} from "./workspace-projection";

const GLOBAL = "global";
const CATALOG = "catalog";

type Route =
  | { kind: "home" }
  | { kind: "project"; cwd: string }
  | { kind: "session"; sessionId: string; cwd?: string };

/** A prompt in this page's queue; `state` says where it is (C2, C3). */
export interface QueueEntry {
  id: string;
  sessionId: string;
  blocks: PromptBlock[];
  state: "queued" | "paused" | "sending" | "refused" | "uncertain";
  error?: string;
}

export interface WorkspaceState extends AppState {
  /** The queued prompts of the shown session, in the order they are sent. */
  queue: QueueEntry[];
  queuePaused: boolean;
  /** Login terminals, each its own resource (§4.6). */
  authTerminals: AuthTerminalState[];
  /** Sessions the bridge holds (`global.sessions`): they open without a load. */
  openSessionIds: string[];
  /** Held sessions whose operation runs (`global.sessions`). */
  busySessionIds: string[];
  /** Session terminals no tool card of the thread shows (S7). */
  standaloneTerminals: TerminalSnapshot[];
  attachments?: AttachmentReader;
  /** The route's session while it cannot be shown yet, and why. */
  sessionOpening?: { sessionId: string; error?: string };
  projectCwd?: string;
}

interface CatalogState {
  sessions: SessionInfo[];
  nextCursor: string | null;
  /** The revision the first page was listed at. */
  revision?: string;
  /** The revision a list failed at; it is not listed again until it changes or the user asks. */
  failedRevision?: string;
  listing: boolean;
  refresh: boolean;
  /** Whether the first page has been listed, has failed, or cannot be listed. */
  settled: boolean;
  /** The `op.id` the last list was sent against. */
  sentAgainst?: string;
}

interface AnswerState {
  /** Answers the user gave, by interaction, not yet sent. */
  drafts: Map<string, Answer>;
  /** The answer write in flight or sent, and the interactions it covered. */
  sent?: { op: string; ids: string[]; state: "sending" | "sent" | "uncertain" };
  errors: Map<string, string>;
}

interface HeldPrompt {
  id: string;
  blocks: PromptBlock[];
}

/** The ops client whose answers also notify the page: the write queue reports none. */
class NotifyingOps extends OpsClient {
  constructor(fetch: HttpFetch, replicas: ReplicaClient, private readonly answered: () => void) {
    super(fetch, replicas);
  }

  override write(scope: string, write: OpArgs): Promise<WriteOutcome> {
    const sent = super.write(scope, write);
    void sent.then(() => setTimeout(this.answered, 0));
    return sent;
  }
}

export class Workspace {
  readonly #listeners = new Set<() => void>();
  // The clients of one connection: `start()` makes them, its cleanup closes them,
  // so a StrictMode remount gets a working stream again.
  #replicas!: ReplicaClient;
  #ops!: NotifyingOps;
  #queue!: WriteQueue;
  #resources!: ResourceClient;
  #route: Route = readRoute();
  /** Counts navigations; a write's late result never moves a page the user moved since. */
  #navigation = 0;
  #follow = "";
  /** The session scope aligned on the current stream. */
  #aligned: string | undefined;
  #lastEnd: { scope: string; end: End } | undefined;
  readonly #ended = new Map<string, End>();
  #catalog: CatalogState = { sessions: [], nextCursor: null, listing: false, refresh: false, settled: false };
  #authRequired = false;
  /** Operations whose end the page has acted on. */
  readonly #seen = new Set<string>();
  /** Operations this page started on `global`: their failures are its to show. */
  readonly #mine = new Set<string>();
  #errors: Array<Extract<TimelineItem, { type: "error" }>> = [];
  #transition: { kind: "new" | "fork"; navigation: number } | undefined;
  readonly #answers = new Map<string, AnswerState>();
  readonly #held = new Map<string, HeldPrompt[]>();
  readonly #paused = new Set<string>();
  #nextId = 0;
  #reader: AttachmentReader | undefined;
  /** The cwd each session reported while shown, for leaving it once it is gone. */
  readonly #sessionCwd = new Map<string, string>();
  #agentPhase: string | undefined;
  #snapshot: WorkspaceState | undefined;
  #closed = false;

  constructor() {
    this.#connect();
  }

  #connect(): void {
    const fetchJson: HttpFetch = (url, init) => fetch(url, init);
    const replicas = new ReplicaClient((url) => this.#open(url));
    this.#replicas = replicas;
    this.#ops = new NotifyingOps(fetchJson, replicas, () => {
      if (this.#replicas === replicas) this.#changed();
    });
    this.#queue = new WriteQueue(this.#ops, replicas);
    this.#resources = new ResourceClient(fetchJson);
    replicas.subscribe((scope) => {
      if (this.#replicas === replicas) this.#received(scope);
    });
    this.#follow = "";
    this.#aligned = undefined;
  }

  start(): () => void {
    if (this.#closed) this.#connect();
    this.#closed = false;
    const popstate = () => {
      this.#setRoute(readRoute());
    };
    window.addEventListener("popstate", popstate);
    this.#sync();
    return () => {
      this.#closed = true;
      window.removeEventListener("popstate", popstate);
      this.#replicas.close();
    };
  }

  subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  getSnapshot = (): WorkspaceState => {
    this.#snapshot ??= this.#project();
    return this.#snapshot;
  };

  // -------------------------------------------------------------------------
  // Stream

  /** Opens an `EventSource`, noting each `end` it carries before the replica engine applies it. */
  #open(url: string): StreamSource {
    const source = new EventSource(url);
    const noteEnd = (data: string) => this.#noteEnd(data);
    let message: StreamSource["onmessage"] = null;
    let error: StreamSource["onerror"] = null;
    return {
      get onmessage() { return message; },
      set onmessage(handler) {
        message = handler;
        source.onmessage = handler == null ? null : (event: MessageEvent<string>) => {
          noteEnd(event.data);
          handler({ data: event.data });
        };
      },
      get onerror() { return error; },
      set onerror(handler) {
        error = handler;
        source.onerror = handler == null ? null : () => handler();
      },
      close: () => source.close(),
    };
  }

  #noteEnd(data: string): void {
    try {
      const event: unknown = JSON.parse(data);
      if (isRecord(event) && typeof event.scope === "string" && isRecord(event.end)) {
        this.#lastEnd = { scope: event.scope, end: event.end as unknown as End };
      }
    } catch {
      // The replica engine treats it as a broken stream.
    }
  }

  #received(scope: string): void {
    if (this.#replicas.get(scope) == null) {
      const end = this.#lastEnd?.scope === scope ? this.#lastEnd.end : { reason: "load_failed" as const };
      this.#ended.set(scope, end);
      this.#scopeEnded(scope, end);
    } else if (scope.startsWith("session/")) {
      this.#aligned = scope;
    }
    this.#lastEnd = undefined;
    this.#queue.update(scope);
    this.#changed();
  }

  #scopeEnded(scope: string, end: End): void {
    const route = this.#route;
    if (route.kind !== "session" || `session/${route.sessionId}` !== scope) return;
    if (end.reason === "deleted") {
      const cwd = this.#sessionCwd.get(route.sessionId) ?? this.#knownCwd(route.sessionId) ?? route.cwd;
      this.#navigate(cwd == null ? { kind: "home" } : { kind: "project", cwd }, true);
      return;
    }
    if (needsAuthentication(end.error)) {
      this.#authRequired = true;
      return;
    }
    // A session nothing knows of is not one to report: send the page home (§5.1).
    if (this.#knownCwd(route.sessionId) == null) this.#navigate({ kind: "home" }, true);
  }

  /** The cwd the bridge or the catalog reports for a session. */
  #knownCwd(sessionId: string): string | undefined {
    return this.#global()?.sessions?.[sessionId]?.cwd ??
      this.#catalog.sessions.find((session) => session.sessionId === sessionId)?.cwd;
  }

  /** The scopes the page follows: the global ones and the session it shows. */
  #desiredFollow(): Follow[] {
    const follow: Follow[] = [{ scope: GLOBAL }, { scope: CATALOG }];
    const route = this.#route;
    if (route.kind !== "session") return follow;
    const scope = `session/${route.sessionId}`;
    if (this.#ended.has(scope)) return follow;
    const global = this.#global();
    if (global == null) return follow;
    const held = global.sessions?.[route.sessionId];
    if (held != null) return [...follow, { scope, cwd: held.cwd }];
    const listed = this.#catalog.sessions.find((session) => session.sessionId === route.sessionId);
    if (listed != null) return [...follow, { scope, cwd: listed.cwd }];
    // An unheld session is loaded with its own cwd: look for it in the catalog first.
    if (!this.#catalog.settled || this.#catalog.listing || this.#catalog.nextCursor != null) return follow;
    if (route.cwd != null) return [...follow, { scope, cwd: route.cwd }];
    return follow;
  }

  #applyFollow(force = false): void {
    if (this.#closed) return;
    const follow = this.#desiredFollow();
    const key = JSON.stringify(follow);
    if (!force && key === this.#follow) return;
    this.#follow = key;
    this.#aligned = undefined;
    this.#replicas.follow(follow);
  }

  // -------------------------------------------------------------------------
  // Reconciliation of this page's own state with the replicas

  #changed(): void {
    this.#sync();
  }

  #sync(): void {
    if (this.#closed) return;
    this.#settleAuth();
    this.#listCatalog();
    this.#resolveRoute();
    this.#applyFollow();
    this.#settleAnswers();
    this.#feed();
    this.#canonicalize();
    const sessionId = this.#route.kind === "session" ? this.#route.sessionId : undefined;
    // Attachments are read per session; another session's reads are dropped with it.
    if (sessionId == null) this.#reader = undefined;
    else if (this.#reader?.sessionId !== sessionId) this.#reader = createAttachmentReader(sessionId);
    this.#snapshot = undefined;
    for (const listener of [...this.#listeners]) listener();
  }

  #global(): GlobalLive | undefined {
    return globalLive(this.#replicas.get(GLOBAL)?.live);
  }

  #sessionScope(): string | undefined {
    return this.#route.kind === "session" ? `session/${this.#route.sessionId}` : undefined;
  }

  /** The session the page shows: aligned on the current stream. */
  #shown(): { scope: string; live: SessionLive; log: readonly LogEntry[]; confirmed: boolean } | undefined {
    const scope = this.#sessionScope();
    if (scope == null || this.#aligned !== scope) return undefined;
    const replica = this.#replicas.get(scope);
    const live = sessionLive(replica?.live);
    if (replica == null || live == null) return undefined;
    return { scope, live, log: replica.log, confirmed: replica.confirmed };
  }

  #settleAuth(): void {
    const global = this.#global();
    if (global == null) return;
    const op = global.op;
    const phase = global.agent.phase;
    const restarted = this.#agentPhase != null && this.#agentPhase !== "ready" && phase === "ready";
    this.#agentPhase = phase;
    let signedIn = global.agent.authenticatedMethodId != null;
    if (!this.#seen.has(op.id) && op.status !== "running") {
      this.#seen.add(op.id);
      const login = op.kind === "terminal_login" && op.status === "completed" &&
        isRecord(op.result) && op.result.exitCode === 0;
      if ((op.kind === "authenticate" && op.status === "completed") || login) signedIn = true;
      if (op.status === "failed" && needsAuthentication(op.error)) this.#authRequired = true;
      if (op.kind === "create" && op.status === "failed" && needsAuthentication(op.error)) this.#authRequired = true;
    }
    const shown = this.#shown();
    const sessionOp = shown?.live.op;
    if (sessionOp != null && !this.#seen.has(sessionOp.id) && sessionOp.status === "failed") {
      this.#seen.add(sessionOp.id);
      if (needsAuthentication(sessionOp.error)) this.#authRequired = true;
    }
    const waiting = this.#authRequired || [...this.#ended.values()].some((end) => needsAuthentication(end.error));
    if ((signedIn || restarted) && waiting) {
      this.#authRequired = false;
      // What failed for want of authentication is tried again (§5.4).
      for (const [scope, end] of this.#ended) {
        if (end.reason === "load_failed" && needsAuthentication(end.error)) this.#ended.delete(scope);
      }
      this.#catalog = { ...this.#catalog, refresh: true, failedRevision: undefined };
    }
  }

  #listCatalog(): void {
    const catalog = catalogLive(this.#replicas.get(CATALOG)?.live);
    const global = this.#global();
    if (catalog == null || global == null || this.#catalog.listing) return;
    const capabilities = global.agent.initialize?.agentCapabilities?.sessionCapabilities;
    if (global.agent.initialize == null) return;
    if (capabilities?.list == null) {
      if (!this.#catalog.settled) this.#catalog = { ...this.#catalog, settled: true, nextCursor: null };
      return;
    }
    if (catalog.op.status === "running" || catalog.op.id === this.#catalog.sentAgainst) return;
    const route = this.#route;
    // A session the catalog has not listed yet is looked for on its later pages.
    const looking = route.kind === "session" &&
      !this.#catalog.sessions.some((session) => session.sessionId === route.sessionId);
    const stale = this.#catalog.revision !== catalog.revision && this.#catalog.failedRevision !== catalog.revision;
    if (this.#catalog.refresh || stale) {
      this.#list(undefined);
    } else if (looking && this.#catalog.nextCursor != null && this.#catalog.revision === catalog.revision) {
      this.#list(this.#catalog.nextCursor);
    }
  }

  #list(cursor: string | undefined): void {
    const catalog = catalogLive(this.#replicas.get(CATALOG)?.live);
    if (catalog == null) return;
    const revision = cursor == null ? undefined : this.#catalog.revision;
    this.#catalog = { ...this.#catalog, listing: true, refresh: false, sentAgainst: catalog.op.id };
    void this.#ops.write(CATALOG, { kind: "list", args: cursor == null ? {} : { cursor, ...(revision ? { revision } : {}) } })
      .then((outcome) => {
        const listed = { ...this.#catalog, listing: false };
        if (outcome.kind === "ended" && "result" in outcome.outcome && isRecord(outcome.outcome.result)) {
          const page = outcome.outcome.result as unknown as CatalogPage;
          this.#catalog = {
            ...listed,
            sessions: cursor == null ? page.sessions : mergeSessions(this.#catalog.sessions, page.sessions),
            nextCursor: page.nextCursor,
            revision: page.revision,
            failedRevision: undefined,
            settled: true,
          };
        } else if (outcome.kind === "refused" && outcome.error.code === "catalog_changed") {
          this.#catalog = { ...listed, refresh: true };
        } else if (outcome.kind === "refused" && (outcome.error.code === "stale" || outcome.error.code === "not_allowed")) {
          // Another list ran: list again once the replica shows a later `op` ended.
          this.#catalog = { ...listed, refresh: cursor == null };
        } else if (outcome.kind === "blocked") {
          this.#catalog = { ...listed, refresh: cursor == null, sentAgainst: undefined };
        } else {
          const error = outcome.kind === "ended" && "error" in outcome.outcome ? outcome.outcome.error
            : outcome.kind === "refused" ? outcome.error : undefined;
          if (needsAuthentication(error)) this.#authRequired = true;
          this.#catalog = { ...listed, settled: true, nextCursor: null, failedRevision: catalog.revision };
          this.#report("list", outcome);
        }
        this.#changed();
      });
  }

  #resolveRoute(): void {
    const route = this.#route;
    if (route.kind !== "session" || this.#global() == null) return;
    const scope = `session/${route.sessionId}`;
    if (this.#ended.has(scope) || this.#replicas.get(scope) != null) return;
    if (this.#knownCwd(route.sessionId) != null || route.cwd != null || this.#authRequired) return;
    if (!this.#catalog.settled || this.#catalog.listing || this.#catalog.nextCursor != null) return;
    // No cwd anywhere: the session does not exist for this Agent.
    this.#navigate({ kind: "home" }, true);
  }

  /** The URL names the session's own project once the session reports it. */
  #canonicalize(): void {
    const shown = this.#shown();
    const route = this.#route;
    if (shown == null || route.kind !== "session") return;
    this.#sessionCwd.set(route.sessionId, shown.live.cwd);
    const path = sessionPath(route.sessionId, shown.live.cwd);
    if (path !== "/" && window.location.pathname !== path) {
      window.history.replaceState(null, "", path);
      this.#route = { ...route, cwd: shown.live.cwd };
    }
  }

  // -------------------------------------------------------------------------
  // Navigation

  #setRoute(route: Route): void {
    const previous = this.#route;
    this.#route = route;
    if (JSON.stringify(previous) === JSON.stringify(route)) {
      this.#changed();
      return;
    }
    this.#navigation += 1;
    this.#errors = [];
    const previousScope = previous.kind === "session" ? `session/${previous.sessionId}` : undefined;
    if (previousScope != null && (route.kind !== "session" || `session/${route.sessionId}` !== previousScope)) {
      this.#ended.delete(previousScope);
    }
    if (route.kind === "session") this.#ended.delete(`session/${route.sessionId}`);
    this.#changed();
  }

  #navigate(route: Route, replace = false): void {
    const path = route.kind === "home" ? "/" : route.kind === "project" ? projectPath(route.cwd)
      : sessionPath(route.sessionId, route.cwd);
    if (window.location.pathname !== path || window.location.search || window.location.hash) {
      if (replace) window.history.replaceState(null, "", path);
      else window.history.pushState(null, "", path);
    }
    this.#setRoute(route.kind === "project" && path === "/" ? { kind: "home" } : route);
  }

  goHome = (): void => this.#navigate({ kind: "home" });

  openProject = (cwd: string): void => this.#navigate({ kind: "project", cwd });

  attachSession = (session: SessionInfo): void =>
    this.#navigate({ kind: "session", sessionId: session.sessionId, ...(session.cwd ? { cwd: session.cwd } : {}) });

  /** Leaving a session is navigation only: no public close exists (§10, S6). */
  closeSession = (): void => {
    const shown = this.#shown();
    const route = this.#route;
    const cwd = shown?.live.cwd ?? (route.kind === "session" ? route.cwd : undefined);
    this.#navigate(cwd == null ? { kind: "home" } : { kind: "project", cwd });
  };

  retrySessionOpening = (): void => {
    const scope = this.#sessionScope();
    if (scope != null) this.#ended.delete(scope);
    this.#applyFollow(true);
    this.#changed();
  };

  reconnect = (): void => {
    this.#applyFollow(true);
  };

  // -------------------------------------------------------------------------
  // Writes

  /** Shows what a write's answer says when the page cannot leave it to the replica. */
  #report(operation: string, outcome: WriteOutcome): void {
    let message: string | undefined;
    let code: number | undefined;
    let data: unknown;
    if (outcome.kind === "refused") {
      message = i18n.t("errors.refused", { operation, message: outcome.error.message });
      data = outcome.error;
    } else if (outcome.kind === "uncertain") {
      message = i18n.t("errors.uncertainWrite", { operation });
    } else if (outcome.kind === "ended" && "error" in outcome.outcome) {
      const error: ProtocolError = outcome.outcome.error;
      const agent = isRecord(error.data) ? error.data : undefined;
      message = agent != null && typeof agent.message === "string" ? agent.message : error.message;
      code = agent != null && typeof agent.code === "number" ? agent.code : undefined;
      data = agent?.data ?? error.data;
    }
    if (message == null) return;
    this.#nextId += 1;
    this.#errors = [...this.#errors, {
      id: `local:${this.#nextId}`, type: "error", message, operation,
      ...(code != null ? { code } : {}), ...(data !== undefined ? { data } : {}),
    }];
  }

  #write(scope: string, write: OpArgs, label: string): Promise<WriteOutcome> {
    return this.#ops.write(scope, write).then((outcome) => {
      if (scope === GLOBAL && (outcome.kind === "accepted" || outcome.kind === "ended")) this.#mine.add(outcome.op);
      this.#report(label, outcome);
      this.#changed();
      return outcome;
    });
  }

  newSession = (cwd: string): boolean => {
    const global = this.#global();
    if (global == null || global.op.status === "running" || this.#transition != null) return false;
    const navigation = this.#navigation;
    this.#transition = { kind: "new", navigation };
    void this.#ops.write(GLOBAL, { kind: "create", args: { cwd } }).then((outcome) => {
      this.#transition = undefined;
      if (outcome.kind === "ended" && "result" in outcome.outcome && isRecord(outcome.outcome.result) &&
        typeof outcome.outcome.result.sessionId === "string") {
        // The create's own result names the session (§6.3); a later operation does not matter.
        if (this.#navigation === navigation) {
          this.#navigate({ kind: "session", sessionId: outcome.outcome.result.sessionId, cwd });
        }
      } else {
        if (outcome.kind === "ended" && "error" in outcome.outcome && needsAuthentication(outcome.outcome.error)) {
          this.#authRequired = true;
        }
        if (this.#navigation === navigation) this.#report("create", outcome);
      }
      this.#changed();
    });
    this.#changed();
    return true;
  };

  forkSession = (): void => {
    const shown = this.#shown();
    if (shown == null || shown.live.op.status === "running" || this.#transition != null) return;
    const navigation = this.#navigation;
    const cwd = shown.live.cwd;
    this.#transition = { kind: "fork", navigation };
    void this.#ops.write(shown.scope, { kind: "fork", args: {} }).then((outcome) => {
      this.#transition = undefined;
      if (outcome.kind === "ended" && "result" in outcome.outcome && isRecord(outcome.outcome.result) &&
        typeof outcome.outcome.result.sessionId === "string") {
        if (this.#navigation === navigation) {
          this.#navigate({ kind: "session", sessionId: outcome.outcome.result.sessionId, cwd });
        }
      } else if (this.#navigation === navigation) {
        this.#report("fork", outcome);
      }
      this.#changed();
    });
    this.#changed();
  };

  deleteSession = (sessionId: string): void => {
    void this.#write(GLOBAL, { kind: "delete", args: { sessionId } }, "delete");
  };

  setMode = (modeId: string): void => {
    const scope = this.#shown()?.scope;
    if (scope != null) void this.#write(scope, { kind: "set_mode", args: { modeId } }, "set_mode");
  };

  setConfig = (configId: string, value: string | boolean): void => {
    const scope = this.#shown()?.scope;
    if (scope != null) void this.#write(scope, { kind: "set_config", args: { configId, value } }, "set_config");
  };

  /** Stops the running turn; the queue waits until the user sends again. */
  cancel = (): void => {
    const shown = this.#shown();
    if (shown == null || shown.live.op.status !== "running") return;
    void this.#write(shown.scope, { kind: "cancel", args: {} }, "cancel");
  };

  listSessions = (cursor?: string): void => {
    if (cursor == null) {
      // The user asks again: a new attempt, which may go against the same `op`
      // a refused list was sent against. Nothing is resent without this (C3).
      this.#catalog = { ...this.#catalog, refresh: true, failedRevision: undefined, sentAgainst: undefined };
      this.#changed();
    } else if (!this.#catalog.listing) {
      this.#list(cursor);
    }
  };

  // -------------------------------------------------------------------------
  // Prompts (C2, C3)

  /**
   * Queues a prompt for the shown session. A prompt sent while the turn is
   * idle goes ahead of paused ones; the queue sends one at a time, against the
   * `op` the replica shows ended.
   */
  prompt = (blocks: PromptBlock[], ahead = false): boolean => {
    const scope = this.#shown()?.scope;
    if (scope == null) return false;
    this.#nextId += 1;
    const held = this.#held.get(scope) ?? [];
    const prompt = { id: `held:${this.#nextId}`, blocks };
    this.#held.set(scope, ahead ? [prompt, ...held] : [...held, prompt]);
    this.#paused.delete(scope);
    this.#changed();
    return true;
  };

  /** Sends prompts from the page's queue into the write queue, one at a time. */
  #feed(): void {
    for (const [scope, held] of this.#held) {
      if (held.length === 0 || this.#paused.has(scope)) continue;
      if (this.#queue.items(scope).some(({ state }) => state !== "uncertain")) continue;
      const [next, ...rest] = held;
      this.#held.set(scope, rest);
      this.#queue.enqueue(scope, { kind: "prompt", args: { prompt: next.blocks } });
    }
  }

  /** Pauses the queue: nothing waiting is sent until the user sends again (Stop). */
  pauseQueue = (): void => {
    const scope = this.#shown()?.scope;
    if (scope == null) return;
    this.#paused.add(scope);
    const back = this.#queue.items(scope).filter(({ state }) => state === "queued" || state === "refused");
    for (const item of back) this.#queue.remove(scope, item.id);
    if (back.length > 0) {
      this.#held.set(scope, [
        ...back.map((item) => ({ id: `held:${(this.#nextId += 1)}`, blocks: promptOf(item.write) })),
        ...(this.#held.get(scope) ?? []),
      ]);
    }
    this.#changed();
  };

  /** Takes a queued prompt back; returns its blocks. */
  takeQueued = (id: string): PromptBlock[] | undefined => {
    const scope = this.#shown()?.scope;
    if (scope == null) return undefined;
    const held = this.#held.get(scope) ?? [];
    const prompt = held.find((item) => item.id === id);
    if (prompt != null) {
      this.#held.set(scope, held.filter((item) => item.id !== id));
      this.#changed();
      return prompt.blocks;
    }
    const queued = this.#queue.items(scope).find((item) => `queue:${item.id}` === id);
    if (queued == null || queued.state === "sending") return undefined;
    this.#queue.remove(scope, queued.id);
    this.#changed();
    return promptOf(queued.write);
  };

  clearQueue = (): void => {
    const scope = this.#shown()?.scope;
    if (scope == null) return;
    this.#held.delete(scope);
    for (const item of this.#queue.items(scope)) this.#queue.remove(scope, item.id);
    this.#paused.delete(scope);
    this.#changed();
  };

  /** Sends a queued prompt next: it goes first, and the running turn is cancelled. */
  sendQueuedNow = (id: string): void => {
    const scope = this.#shown()?.scope;
    if (scope == null) return;
    const blocks = this.takeQueued(id);
    if (blocks == null) return;
    this.#paused.add(scope);
    this.pauseQueue();
    this.#nextId += 1;
    this.#held.set(scope, [{ id: `held:${this.#nextId}`, blocks }, ...(this.#held.get(scope) ?? [])]);
    this.#paused.delete(scope);
    this.cancel();
    this.#changed();
  };

  /** The user sends an uncertain prompt again, as new input (C3). */
  resendQueued = (id: string): void => {
    const scope = this.#shown()?.scope;
    const queued = scope == null ? undefined : this.#queue.items(scope).find((item) => `queue:${item.id}` === id);
    if (scope == null || queued == null) return;
    this.#queue.remove(scope, queued.id);
    this.prompt(promptOf(queued.write));
  };

  // -------------------------------------------------------------------------
  // Answers (§6.2: one write answers every pending interaction)

  #interactionScope(interactionId: string): string | undefined {
    for (const scope of [this.#shown()?.scope, GLOBAL, CATALOG]) {
      if (scope == null) continue;
      const op = this.#scopeOp(scope);
      if (op?.status === "running" && Object.hasOwn(op.interactions ?? {}, interactionId)) return scope;
    }
    return undefined;
  }

  #scopeOp(scope: string): Operation | undefined {
    const live = this.#replicas.get(scope)?.live;
    return isRecord(live) && isRecord(live.op) ? live.op as unknown as Operation : undefined;
  }

  #answerState(scope: string): AnswerState {
    let state = this.#answers.get(scope);
    if (state == null) {
      state = { drafts: new Map(), errors: new Map() };
      this.#answers.set(scope, state);
    }
    return state;
  }

  #respond(interactionId: string, answer: Answer): void {
    const scope = this.#interactionScope(interactionId);
    if (scope == null) return;
    const state = this.#answerState(scope);
    state.drafts.set(interactionId, answer);
    state.errors.delete(interactionId);
    this.#sendAnswers(scope);
    this.#changed();
  }

  respondPermission = (permissionId: string, outcome: RequestPermissionResponse["outcome"]): void =>
    this.#respond(permissionId, { outcome });

  respondElicitation = (elicitationId: string, response: CreateElicitationResponse): void =>
    this.#respond(elicitationId, { response });

  #sendAnswers(scope: string): void {
    const op = this.#scopeOp(scope);
    const state = this.#answers.get(scope);
    if (op == null || state == null || op.status !== "running" || state.sent?.op === op.id) return;
    const pending = Object.keys(op.interactions ?? {});
    if (pending.length === 0 || !pending.every((id) => state.drafts.has(id))) return;
    const answers = Object.fromEntries(pending.map((id) => [id, state.drafts.get(id)!]));
    const sent = { op: op.id, ids: pending, state: "sending" as "sending" | "sent" | "uncertain" };
    state.sent = sent;
    void this.#ops.answer(scope, answers).then((outcome) => {
      if (state.sent !== sent) return;
      switch (outcome.kind) {
        case "accepted":
        case "ended":
          sent.state = "sent";
          break;
        case "uncertain":
          // It may have been delivered: the interactions stay pending here
          // until the replica shows otherwise, and nothing is sent again (C3).
          sent.state = "uncertain";
          break;
        case "refused":
          // The input stays with the user (C2): the form keeps its values.
          state.sent = undefined;
          for (const id of sent.ids) {
            state.drafts.delete(id);
            state.errors.set(id, i18n.t("interaction.refused", { ns: "cards", message: outcome.error.message }));
          }
          break;
        case "blocked":
          state.sent = undefined;
          break;
      }
      this.#changed();
    });
  }

  /** Drops answers to interactions the replica no longer shows pending. */
  #settleAnswers(): void {
    for (const [scope, state] of this.#answers) {
      const op = this.#scopeOp(scope);
      const pending = new Set(op?.status === "running" ? Object.keys(op.interactions ?? {}) : []);
      for (const id of [...state.drafts.keys()]) if (!pending.has(id)) state.drafts.delete(id);
      for (const id of [...state.errors.keys()]) if (!pending.has(id)) state.errors.delete(id);
      if (state.sent != null && (state.sent.op !== op?.id || !state.sent.ids.some((id) => pending.has(id)))) {
        state.sent = undefined;
      }
      // The pending set may have shrunk to what the user answered already.
      this.#sendAnswers(scope);
    }
  }

  #interactionStatus(scope: string, interactionId: string): Pick<PendingPermission, "responseRequestId" | "responseStatus" | "responseError"> {
    const state = this.#answers.get(scope);
    if (state == null) return {};
    const error = state.errors.get(interactionId);
    if (state.sent?.ids.includes(interactionId)) {
      return {
        responseRequestId: state.sent.op,
        ...(state.sent.state === "uncertain" ? { responseStatus: i18n.t("interaction.uncertain", { ns: "cards" }) } : {}),
      };
    }
    if (state.drafts.has(interactionId)) {
      return { responseRequestId: "draft", responseStatus: i18n.t("interaction.waitingOthers", { ns: "cards" }) };
    }
    return error == null ? {} : { responseError: error };
  }

  // -------------------------------------------------------------------------
  // Resources (§7)

  dismissExternalFlow = (interactionId: string): void => {
    for (const scope of [this.#shown()?.scope, GLOBAL, CATALOG]) {
      if (scope == null) continue;
      const live = this.#replicas.get(scope)?.live;
      if (isRecord(live) && isRecord(live.urlFlows) && Object.hasOwn(live.urlFlows, interactionId)) {
        void this.#resources.dismiss(scope, interactionId).then(() => this.#changed());
        return;
      }
    }
  };

  authenticate = (methodId: string): void => {
    const global = this.#global();
    const method = global?.agent.initialize?.authMethods?.find(({ id }) => id === methodId);
    if (global == null || method == null || global.op.status === "running") return;
    if ("type" in method && method.type === "terminal") {
      void this.#write(GLOBAL, { kind: "terminal_login", args: { methodId, cols: 80, rows: 24 } }, "terminal_login");
    } else {
      void this.#write(GLOBAL, { kind: "authenticate", args: { methodId } }, "authenticate");
    }
  };

  logout = (): void => {
    void this.#write(GLOBAL, { kind: "logout", args: {} }, "logout");
  };

  /** Terminal input: in order, one request at a time per terminal, never resent (§7, C3). */
  writeAuthTerminal = (terminalId: string, data: string): void => {
    void this.#resources.input(GLOBAL, terminalId, data);
  };

  resizeAuthTerminal = (terminalId: string, cols: number, rows: number): void => {
    void this.#resources.resize(GLOBAL, terminalId, cols, rows);
  };

  cancelAuthTerminal = (terminalId: string): void => {
    const op = this.#global()?.op;
    if (op?.id === terminalId && op.status === "running") void this.#write(GLOBAL, { kind: "cancel", args: {} }, "cancel");
  };

  dismissAuthTerminal = (terminalId: string): void => {
    void this.#resources.dismiss(GLOBAL, terminalId).then(() => this.#changed());
  };

  // -------------------------------------------------------------------------
  // Reads (§8)

  searchWorkspaceContext = async (query: string): Promise<WorkspaceContextMatch[]> => {
    const shown = this.#shown();
    if (shown == null) throw new Error(i18n.t("errors.searchNotReady"));
    const body = await readJson(`/api/v2/sessions/${encodeURIComponent(shown.live.sessionId)}/files?${new URLSearchParams({ query })}`);
    return isRecord(body) && Array.isArray(body.matches) ? body.matches as WorkspaceContextMatch[] : [];
  };

  readWorkspaceContext = async (path: string): Promise<WorkspaceContextAttachment> => {
    const shown = this.#shown();
    if (shown == null) throw new Error(i18n.t("errors.contextNotReady"));
    const body = await readJson(`/api/v2/sessions/${encodeURIComponent(shown.live.sessionId)}/files/content?${new URLSearchParams({ path })}`);
    if (!isRecord(body) || !isRecord(body.attachment)) throw new Error(i18n.t("errors.contextNotReady"));
    return body.attachment as unknown as WorkspaceContextAttachment;
  };

  // -------------------------------------------------------------------------
  // Projection (C1)

  #project(): WorkspaceState {
    const global = this.#global();
    const agent = global?.agent;
    const route = this.#route;
    const shown = this.#shown();
    const live = shown?.live;
    const op = live?.op;
    const sessionId = route.kind === "session" ? route.sessionId : undefined;
    const scope = shown?.scope;

    // A session is shown once aligned, unless a first load is still filling an empty log.
    const loading = live != null && op?.kind === "load" && op.status === "running" && shown!.log.length === 0 &&
      !shown!.confirmed && pendingInteractions(op).length === 0;
    const visible = live != null && !loading;
    // The end of the route's session: its replica is gone once it ended.
    const routeScope = this.#sessionScope();
    const ended = routeScope == null ? undefined : this.#ended.get(routeScope);
    const sessionOpening = route.kind !== "session" || visible ? undefined : {
      sessionId: route.sessionId,
      ...(ended?.reason === "load_failed" && !needsAuthentication(ended.error)
        ? { error: ended.error?.message ?? i18n.t("errors.promptNotReady") } : {}),
    };

    const methods = agent?.initialize?.authMethods ?? [];
    const canLogout = agent?.initialize?.agentCapabilities?.auth?.logout != null;
    let authStatus: AgentAuthStatus | undefined;
    if (methods.length > 0 || canLogout) {
      authStatus = agent?.authenticatedMethodId != null ? "authenticated"
        : global?.op.kind === "logout" && global.op.status === "completed" ? "logged_out"
        : this.#authRequired ? "required" : "available";
    }
    const globalOp = global?.op;
    const authOp = globalOp != null && ["authenticate", "logout", "terminal_login"].includes(globalOp.kind) ? globalOp : undefined;

    const authTerminals: AuthTerminalState[] = Object.values(global?.terminals ?? {}).map((terminal) => {
      const login = globalOp?.id === terminal.terminalId ? globalOp : undefined;
      const status: AuthTerminalState["status"] = terminal.exitStatus == null ? "running"
        : terminal.exitStatus.exitCode === 0 ? "succeeded"
        : login?.status === "cancelled" || (login?.cancelRequested && terminal.exitStatus.exitCode == null) ? "cancelled"
        : "failed";
      return {
        requestId: terminal.terminalId,
        methodId: terminal.methodId,
        status,
        output: terminal.output,
        truncated: terminal.truncated,
        exitCode: terminal.exitStatus?.exitCode,
        ...(login?.error ? { message: login.error.message } : {}),
      };
    }).filter((terminal) => terminal.status !== "succeeded");

    // Interactions: the shown session's operation, and the global and catalog ones.
    const permissions: PendingPermission[] = [];
    const elicitations: PendingElicitation[] = [];
    const interactionScopes = [
      ...(visible && scope != null && op != null ? [[scope, op] as const] : []),
      ...(globalOp != null ? [[GLOBAL, globalOp] as const] : []),
      ...(() => {
        const catalog = catalogLive(this.#replicas.get(CATALOG)?.live);
        return catalog != null ? [[CATALOG, catalog.op] as const] : [];
      })(),
    ];
    // One operation occupying several scopes is the `op` of each (§6.4): show its interactions once.
    const projectedOps = new Set<string>();
    for (const [owner, ownerOp] of interactionScopes) {
      if (projectedOps.has(ownerOp.id)) continue;
      projectedOps.add(ownerOp.id);
      for (const interaction of pendingInteractions(ownerOp)) {
        const status = this.#interactionStatus(owner, interaction.interactionId);
        pushInteraction(interaction, status, permissions, elicitations);
      }
    }
    const externalFlows = [
      ...(visible && live != null ? Object.values(live.urlFlows ?? {}).map((flow) => externalFlow(flow, live.sessionId)) : []),
      ...Object.values(global?.urlFlows ?? {}).map((flow) => externalFlow(flow)),
      ...Object.values(catalogLive(this.#replicas.get(CATALOG)?.live)?.urlFlows ?? {}).map((flow) => externalFlow(flow)),
    ];

    // The thread.
    let timeline: TimelineItem[] = [];
    let activePlan: AppState["activePlan"];
    let agentActivity: AppState["agentActivity"];
    if (visible && live != null && op != null) {
      timeline = projectTimeline(live.sessionId, shown!.log, op);
      if (op.kind === "prompt" && op.status === "running") {
        const process = projectProcess(live.sessionId, op.id, op.process ?? [], promptInput(op));
        activePlan = process.activePlan;
        agentActivity = process.activity ?? { kind: "waiting" };
      }
    }
    // Failures of the global operations this page started, and its own write answers.
    if (globalOp != null && globalOp.status === "failed" && globalOp.error != null && this.#mine.has(globalOp.id) &&
      authOp == null) {
      timeline = [...timeline, operationError(`op:${globalOp.id}:error`, globalOp, globalOp.error)];
    }
    timeline = [...timeline, ...this.#errors];

    const terminalSnapshots = live != null ? sessionTerminals(live) : [];
    const controls = live?.controls;
    const running = visible && op?.kind === "prompt" && op.status === "running";
    const runtimeKind = op?.status === "running" && (op.kind === "fork" || op.kind === "set_mode" || op.kind === "set_config")
      ? op.kind : undefined;

    const summaries = Object.values(global?.sessions ?? {});
    const sessions = listedSessions(this.#catalog.sessions, global);
    const deleting = globalOp?.kind === "delete" && globalOp.status === "running" && isRecord(globalOp.input) &&
      typeof globalOp.input.sessionId === "string" ? globalOp.input.sessionId : undefined;

    const queue = scope == null ? [] : this.#queueEntries(scope, sessionId!);

    return {
      ...initialState,
      phase: agent?.phase ?? "starting",
      socketOpen: global != null,
      transport: agent?.launch.transport ?? "stdio",
      command: agent?.launch.command ?? [],
      defaultCwd: agent?.launch.cwd ?? "",
      cwd: live?.cwd ?? (route.kind === "project" ? route.cwd : ""),
      additionalDirectories: agent?.launch.additionalDirectories ?? [],
      mcpServers: agent?.launch.mcpServers ?? [],
      readOnly: agent?.launch.readOnly ?? false,
      initialized: agent?.initialize ?? undefined,
      authStatus,
      pendingAuth: authOp?.status === "running" ? {
        requestId: authOp.id,
        kind: authOp.kind === "terminal_login" ? "terminal" : authOp.kind === "logout" ? "logout" : "authenticate",
        ...(isRecord(authOp.input) && typeof authOp.input.methodId === "string" ? { methodId: authOp.input.methodId } : {}),
      } : undefined,
      authError: authOp?.status === "failed" ? authOp.error?.message : undefined,
      lastAuthResponse: authOp?.status === "completed" && authOp.kind !== "terminal_login" ? {
        kind: authOp.kind === "logout" ? "logout" : "authenticate",
        response: (isRecord(authOp.result) ? authOp.result : {}) as AgentAuthResponse["response"],
      } : undefined,
      session: visible && live != null ? {
        sessionId: live.sessionId,
        ...(controls?.modes != null ? { modes: controls.modes } : {}),
      } : undefined,
      sessions,
      nextSessionCursor: this.#catalog.nextCursor,
      openSessionIds: summaries.map(({ sessionId: id }) => id),
      busySessionIds: summaries.filter(({ op: summary }) => summary.status === "running").map(({ sessionId: id }) => id),
      attentionSessionIds: summaries.filter(({ pending }) => pending.interactions > 0 || pending.urlFlows > 0)
        .map(({ sessionId: id }) => id),
      pendingSessionDeletions: deleting != null ? [{ requestId: globalOp!.id, sessionId: deleting }] : [],
      pendingSessionControl: runtimeKind === "set_mode" || runtimeKind === "set_config" ? {
        requestId: op!.id, sessionId: live!.sessionId, kind: runtimeKind === "set_mode" ? "mode" : "config",
      } : undefined,
      runtimeOperation: runtimeKind != null ? {
        requestId: op!.id, operation: runtimeKind === "fork" ? "fork" : runtimeKind === "set_mode" ? "mode" : "config",
      } : undefined,
      sessionTransition: this.#transition == null || this.#transition.navigation !== this.#navigation ? undefined : {
        kind: this.#transition.kind,
        requestId: `transition:${this.#transition.kind}`,
        backup: {
          cwd: "", availableCommands: [], configOptions: null, timeline: [], permissions: [], elicitations: [],
          running: false, terminalSnapshots: [],
        },
      },
      pendingPrompt: running ? { requestId: op!.id, sessionId: live!.sessionId, blocks: [], cancelRequested: op!.cancelRequested } : undefined,
      availableCommands: controls?.commands ?? [],
      modeId: controls?.modes?.currentModeId,
      configOptions: controls?.configOptions ?? null,
      title: controls?.info.title ?? undefined,
      usage: controls?.usage ?? undefined,
      timeline,
      terminalSnapshots,
      permissions,
      elicitations,
      externalFlows,
      running,
      sessionSyncPhase: !visible ? undefined : op?.status === "running" ? (running ? "running" : "reconciling") : "ready",
      agentActivity,
      activePlan,
      queue,
      queuePaused: scope != null && this.#paused.has(scope),
      authTerminals,
      standaloneTerminals: standaloneTerminals(terminalSnapshots, timeline),
      attachments: this.#reader,
      sessionOpening,
      projectCwd: route.kind === "project" ? route.cwd : undefined,
    };
  }

  #queueEntries(scope: string, sessionId: string): QueueEntry[] {
    const paused = this.#paused.has(scope);
    return [
      ...this.#queue.items(scope).map((item): QueueEntry => ({
        id: `queue:${item.id}`,
        sessionId,
        blocks: promptOf(item.write),
        state: item.state,
        ...(item.error ? { error: item.error.message } : {}),
      })),
      ...(this.#held.get(scope) ?? []).map((item): QueueEntry => ({
        id: item.id, sessionId, blocks: item.blocks, state: paused ? "paused" : "queued",
      })),
    ];
  }
}

function pushInteraction(
  interaction: Interaction,
  status: Pick<PendingPermission, "responseRequestId" | "responseStatus" | "responseError">,
  permissions: PendingPermission[],
  elicitations: PendingElicitation[],
): void {
  if (interaction.kind === "permission") {
    permissions.push({ permissionId: interaction.interactionId, request: interaction.request, ...status });
  } else {
    elicitations.push({ elicitationId: interaction.interactionId, request: interaction.request, ...status });
  }
}

function promptOf(write: OpArgs): PromptBlock[] {
  return write.kind === "prompt" ? write.args.prompt : [];
}

function mergeSessions(current: SessionInfo[], incoming: SessionInfo[]): SessionInfo[] {
  const sessions = new Map(current.map((session) => [session.sessionId, session]));
  for (const session of incoming) sessions.set(session.sessionId, session);
  return [...sessions.values()];
}

async function readJson(path: string): Promise<unknown> {
  const response = await fetch(path);
  const body: unknown = await response.json().catch(() => undefined);
  if (!response.ok) {
    const message = isRecord(body) && isRecord(body.error) && typeof body.error.message === "string"
      ? body.error.message : `${path} failed (${response.status})`;
    throw new Error(message);
  }
  return body;
}

function readRoute(): Route {
  const pathname = window.location.pathname;
  const sessionId = readSessionIdFromPath(pathname);
  const cwd = readProjectCwdFromPath(pathname);
  if (sessionId != null) return { kind: "session", sessionId, ...(cwd != null ? { cwd } : {}) };
  if (cwd != null) return { kind: "project", cwd };
  return { kind: "home" };
}

export function useAcp() {
  const [workspace] = useState(() => new Workspace());
  useEffect(() => workspace.start(), [workspace]);
  const state = useSyncExternalStore(workspace.subscribe, workspace.getSnapshot);
  return {
    state,
    sessionOpening: state.sessionOpening,
    projectCwd: state.projectCwd,
    retrySessionOpening: workspace.retrySessionOpening,
    reconnect: workspace.reconnect,
    authenticate: workspace.authenticate,
    writeAuthTerminal: workspace.writeAuthTerminal,
    resizeAuthTerminal: workspace.resizeAuthTerminal,
    cancelAuthTerminal: workspace.cancelAuthTerminal,
    dismissAuthTerminal: workspace.dismissAuthTerminal,
    logout: workspace.logout,
    prompt: workspace.prompt,
    cancel: workspace.cancel,
    pauseQueue: workspace.pauseQueue,
    takeQueued: workspace.takeQueued,
    clearQueue: workspace.clearQueue,
    sendQueuedNow: workspace.sendQueuedNow,
    resendQueued: workspace.resendQueued,
    setMode: workspace.setMode,
    setConfig: workspace.setConfig,
    respondPermission: workspace.respondPermission,
    respondElicitation: workspace.respondElicitation,
    dismissExternalFlow: workspace.dismissExternalFlow,
    newSession: workspace.newSession,
    listSessions: workspace.listSessions,
    attachSession: workspace.attachSession,
    goHome: workspace.goHome,
    openProject: workspace.openProject,
    forkSession: workspace.forkSession,
    closeSession: workspace.closeSession,
    deleteSession: workspace.deleteSession,
    searchWorkspaceContext: workspace.searchWorkspaceContext,
    readWorkspaceContext: workspace.readWorkspaceContext,
  };
}
