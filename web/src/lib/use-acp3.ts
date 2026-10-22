// The web UI on protocol revision 3 (docs/protocol.md), read side.
//
// One stream follows `global`, `catalog` and the session the page shows; the verifying
// replica engine applies its events, and the page is a projection of the replicas'
// documents plus this page's own UI state (C1). Nothing here writes into a replica.

import i18n from "../i18n";
import { useEffect, useState, useSyncExternalStore } from "react";
import type { SessionInfo } from "@agentclientprotocol/sdk";
import type { TerminalSnapshot } from "../../../shared/bridge";
import type {
  ContextAttachment,
  ContextMatch,
  ContextMatches,
  ContextRead,
  End,
  Interaction,
} from "../../../shared/protocol";
import type { CatalogLive3, GlobalLive3, SessionBody, SessionLive3 } from "../../../shared/documents";
import type { Follow } from "../../../shared/document-replica";
import { FeedClient, type StreamSource } from "../../../shared/feed-client";
import { createAttachmentReader, type AttachmentReader } from "./attachments";
import { projectEntries } from "./entry-projection";
import { projectPath, readProjectCwdFromPath, readSessionIdFromPath, sessionPath } from "./session-route";
import {
  initialState,
  type AgentAuthStatus,
  type AppState,
  type AuthTerminalState,
  type PendingElicitation,
  type PendingPermission,
  type TimelineItem,
} from "./state";
import { externalFlow, isRecord, listedSessions, needsAuthentication, standaloneTerminals } from "./workspace-projection";

const GLOBAL = "global";
const CATALOG = "catalog";

type Route =
  | { kind: "home" }
  | { kind: "project"; cwd: string }
  | { kind: "session"; sessionId: string; cwd?: string };

export interface WorkspaceState extends AppState {
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

/** The document of a scope as the page reads it. */
function bodyOf(replica: { readonly doc: Readonly<Record<string, unknown>> } | undefined): Readonly<Record<string, unknown>> | undefined {
  return replica?.doc;
}

function globalLive(body: Readonly<Record<string, unknown>> | undefined): GlobalLive3 | undefined {
  const live = body?.live;
  return isRecord(live) && isRecord(live.op) && isRecord(live.agent) ? (live as unknown as GlobalLive3) : undefined;
}

function catalogLive(body: Readonly<Record<string, unknown>> | undefined): CatalogLive3 | undefined {
  const live = body?.live;
  return isRecord(live) && isRecord(live.op) && typeof live.revision === "string" ? (live as unknown as CatalogLive3) : undefined;
}

function sessionBody(body: Readonly<Record<string, unknown>> | undefined): SessionBody | undefined {
  const live = body?.live;
  return isRecord(live) && isRecord(live.op) && typeof live.sessionId === "string" && Array.isArray(body?.prompts)
    ? (body as unknown as SessionBody)
    : undefined;
}

/** An `EventSource` as the feed client uses it. */
function openEventSource(url: string): StreamSource {
  const source = new EventSource(url);
  let message: StreamSource["onmessage"] = null;
  let error: StreamSource["onerror"] = null;
  return {
    get onmessage() { return message; },
    set onmessage(handler) {
      message = handler;
      source.onmessage = handler == null ? null : (event: MessageEvent<string>) => handler({ data: event.data });
    },
    get onerror() { return error; },
    set onerror(handler) {
      error = handler;
      source.onerror = handler == null ? null : () => handler();
    },
    close: () => source.close(),
  };
}

export class Workspace {
  readonly #listeners = new Set<() => void>();
  // The client of one connection: `start()` makes it, its cleanup closes it, so a
  // StrictMode remount gets a working stream again.
  #feed!: FeedClient;
  #route: Route = readRoute();
  /** Counts navigations; a write's late result never moves a page the user moved since. */
  #navigation = 0;
  #follow = "";
  /** The session scope aligned on the current stream. */
  #aligned: string | undefined;
  readonly #ended = new Map<string, End>();
  #authRequired = false;
  #reader: AttachmentReader | undefined;
  /** The cwd each session reported while shown, for leaving it once it is gone. */
  readonly #sessionCwd = new Map<string, string>();
  #snapshot: WorkspaceState | undefined;
  #closed = false;

  constructor() {
    this.#connect();
  }

  #connect(): void {
    const feed = new FeedClient(openEventSource);
    this.#feed = feed;
    feed.subscribe((scope) => {
      if (this.#feed === feed) this.#received(scope);
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
      this.#feed.close();
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

  #received(scope: string): void {
    if (this.#feed.get(scope) == null) {
      const end = this.#feed.endOf(scope) ?? { reason: "load_failed" as const };
      this.#ended.set(scope, end);
      this.#scopeEnded(scope, end);
    } else if (scope.startsWith("session/")) {
      this.#aligned = scope;
    }
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

  #catalogSessions(): SessionInfo[] {
    return (this.#catalog()?.pages ?? []).flatMap((page) => [...page.sessions]);
  }

  /** The cwd the bridge or the catalog reports for a session. */
  #knownCwd(sessionId: string): string | undefined {
    return this.#global()?.sessions?.[sessionId]?.cwd ??
      this.#catalogSessions().find((session) => session.sessionId === sessionId)?.cwd;
  }

  /** Whether the catalog has nothing more to tell about the sessions: listed to its end, or not listable. */
  #catalogSettled(): boolean {
    const catalog = this.#catalog();
    const global = this.#global();
    if (catalog == null || global?.agent.initialize == null) return false;
    if (catalog.op.status === "running") return false;
    if (global.agent.initialize.agentCapabilities?.sessionCapabilities?.list == null) return true;
    const last = catalog.pages.at(-1);
    return last != null && last.nextCursor == null;
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
    const listed = this.#catalogSessions().find((session) => session.sessionId === route.sessionId);
    if (listed != null) return [...follow, { scope, cwd: listed.cwd }];
    // An unheld session is loaded with its own cwd: look for it in the catalog first.
    if (!this.#catalogSettled()) return follow;
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
    this.#feed.follow(follow);
  }

  // -------------------------------------------------------------------------
  // Reconciliation of this page's own state with the replicas

  #changed(): void {
    this.#sync();
  }

  #sync(): void {
    if (this.#closed) return;
    this.#settleAuth();
    this.#resolveRoute();
    this.#applyFollow();
    this.#canonicalize();
    const sessionId = this.#route.kind === "session" ? this.#route.sessionId : undefined;
    // Attachments are read per session; another session's reads are dropped with it.
    if (sessionId == null) this.#reader = undefined;
    else if (this.#reader?.sessionId !== sessionId) this.#reader = createAttachmentReader(sessionId);
    this.#snapshot = undefined;
    for (const listener of [...this.#listeners]) listener();
  }

  #global(): GlobalLive3 | undefined {
    return globalLive(bodyOf(this.#feed.get(GLOBAL)));
  }

  #catalog(): CatalogLive3 | undefined {
    return catalogLive(bodyOf(this.#feed.get(CATALOG)));
  }

  #sessionScope(): string | undefined {
    return this.#route.kind === "session" ? `session/${this.#route.sessionId}` : undefined;
  }

  /** The session the page shows: aligned on the current stream. */
  #shown(): { scope: string; body: SessionBody; live: SessionLive3 } | undefined {
    const scope = this.#sessionScope();
    if (scope == null || this.#aligned !== scope) return undefined;
    const body = sessionBody(bodyOf(this.#feed.get(scope)));
    return body == null ? undefined : { scope, body, live: body.live };
  }

  #settleAuth(): void {
    const global = this.#global();
    if (global == null) return;
    const op = global.op;
    if (op.status === "failed" && needsAuthentication(op.error)) this.#authRequired = true;
    const shown = this.#shown();
    if (shown?.live.op.status === "failed" && needsAuthentication(shown.live.op.error)) this.#authRequired = true;
    if (global.agent.authenticatedMethodId != null) {
      this.#authRequired = false;
      for (const [scope, end] of this.#ended) {
        if (end.reason === "load_failed" && needsAuthentication(end.error)) this.#ended.delete(scope);
      }
    }
  }

  #resolveRoute(): void {
    const route = this.#route;
    if (route.kind !== "session" || this.#global() == null) return;
    const scope = `session/${route.sessionId}`;
    if (this.#ended.has(scope) || this.#feed.get(scope) != null) return;
    if (this.#knownCwd(route.sessionId) != null || route.cwd != null || this.#authRequired) return;
    if (!this.#catalogSettled()) return;
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
  // Reads (§8)

  searchWorkspaceContext = async (query: string): Promise<ContextMatch[]> => {
    const shown = this.#shown();
    if (shown == null) throw new Error(i18n.t("errors.searchNotReady"));
    const body = await readJson(`/api/v2/sessions/${encodeURIComponent(shown.live.sessionId)}/files?${new URLSearchParams({ query })}`);
    return isRecord(body) && Array.isArray(body.matches) ? (body as unknown as ContextMatches).matches : [];
  };

  readWorkspaceContext = async (path: string): Promise<ContextAttachment> => {
    const shown = this.#shown();
    if (shown == null) throw new Error(i18n.t("errors.contextNotReady"));
    const body = await readJson(`/api/v2/sessions/${encodeURIComponent(shown.live.sessionId)}/files/content?${new URLSearchParams({ path })}`);
    if (!isRecord(body) || !isRecord(body.attachment)) throw new Error(i18n.t("errors.contextNotReady"));
    return (body as unknown as ContextRead).attachment;
  };

  // -------------------------------------------------------------------------
  // Projection (C1)

  #project(): WorkspaceState {
    const global = this.#global();
    const catalog = this.#catalog();
    const agent = global?.agent;
    const route = this.#route;
    const shown = this.#shown();
    const live = shown?.live;
    const op = live?.op;
    // A session is shown once aligned, unless a first load is still filling an empty thread.
    const entries = shown?.body.entries;
    const loading = live != null && op?.kind === "load" && op.status === "running" &&
      (entries?.length ?? 0) === 0 && shown!.body.history !== "complete" && Object.keys(live.interactions).length === 0;
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

    // Interactions: those the shown session, `global` and `catalog` show. One
    // operation occupying several scopes is the `op` of each (§6.4): show its once.
    const permissions: PendingPermission[] = [];
    const elicitations: PendingElicitation[] = [];
    const seen = new Set<string>();
    const pending: Interaction[] = [
      ...(visible && live != null ? Object.values(live.interactions) : []),
      ...Object.values(global?.interactions ?? {}),
      ...Object.values(catalog?.interactions ?? {}),
    ];
    for (const interaction of pending) {
      if (seen.has(interaction.interactionId)) continue;
      seen.add(interaction.interactionId);
      if (interaction.kind === "permission") {
        permissions.push({ permissionId: interaction.interactionId, request: interaction.request });
      } else {
        elicitations.push({ elicitationId: interaction.interactionId, request: interaction.request });
      }
    }
    const externalFlows = [
      ...(visible && live != null ? Object.values(live.urlFlows ?? {}).map((flow) => externalFlow(flow, live.sessionId)) : []),
      ...Object.values(global?.urlFlows ?? {}).map((flow) => externalFlow(flow)),
      ...Object.values(catalog?.urlFlows ?? {}).map((flow) => externalFlow(flow)),
    ];

    // The thread.
    let timeline: TimelineItem[] = [];
    let activePlan: AppState["activePlan"];
    let agentActivity: AppState["agentActivity"];
    if (visible && live != null && shown != null) {
      const projected = projectEntries(live.sessionId, entries ?? [], shown.body.prompts);
      timeline = projected.timeline;
      activePlan = projected.activePlan;
      agentActivity = projected.activity;
    }

    const terminalSnapshots: TerminalSnapshot[] = Object.values(live?.terminals ?? {}).map((terminal) => ({
      sessionId: live!.sessionId,
      terminalId: terminal.terminalId,
      output: terminal.output,
      truncated: terminal.truncated,
      exitStatus: terminal.exitStatus,
      released: terminal.released,
    }));
    const controls = live?.controls;
    const running = visible && op?.kind === "prompt" && op.status === "running";

    const summaries = Object.values(global?.sessions ?? {});
    const sessions = listedSessions(this.#catalogSessions(), global as never);
    const last = catalog?.pages.at(-1);

    return {
      ...initialState,
      phase: agent?.phase ?? "starting",
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
      session: visible && live != null ? {
        sessionId: live.sessionId,
        ...(controls?.modes != null ? { modes: controls.modes } : {}),
      } : undefined,
      sessions,
      nextSessionCursor: last?.nextCursor ?? null,
      openSessionIds: summaries.map(({ sessionId: id }) => id),
      busySessionIds: summaries.filter(({ op: summary }) => summary.status === "running").map(({ sessionId: id }) => id),
      attentionSessionIds: summaries.filter(({ pending: counts }) => counts.interactions > 0 || counts.urlFlows > 0)
        .map(({ sessionId: id }) => id),
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
      authTerminals,
      standaloneTerminals: standaloneTerminals(terminalSnapshots, timeline),
      attachments: this.#reader,
      sessionOpening,
      projectCwd: route.kind === "project" ? route.cwd : undefined,
    };
  }
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
    goHome: workspace.goHome,
    openProject: workspace.openProject,
    attachSession: workspace.attachSession,
    closeSession: workspace.closeSession,
    searchWorkspaceContext: workspace.searchWorkspaceContext,
    readWorkspaceContext: workspace.readWorkspaceContext,
  };
}
