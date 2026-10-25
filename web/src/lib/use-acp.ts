// The web UI on protocol revision 3 (docs/protocol.md).
//
// One stream follows `global`, `catalog` and the session the page shows; the verifying
// replica engine applies its events, and the page is a projection of the replicas'
// documents plus this page's own UI state (C1). Nothing here writes into a replica.

import i18n from "../i18n";
import { useEffect, useState, useSyncExternalStore } from "react";
import type { CreateElicitationResponse, RequestPermissionResponse, SessionInfo } from "@agentclientprotocol/sdk";
import type { TerminalSnapshot } from "../../../shared/bridge";
import type {
  ContextAttachment,
  ContextMatch,
  ContextMatches,
  ContextRead,
  End,
  Interaction,
  PromptBlock,
} from "../../../shared/protocol";
import type { CatalogLive3, GlobalLive3, SessionBody, SessionLive3, ShownOp } from "../../../shared/documents";
import { ScopeFeed, operationOf, type Follow } from "../../../shared/scope-feed";
import { TreeFeedClient, type StreamSource } from "../../../shared/tree-feed";
import { WriteQueue, type HttpFetch } from "../../../shared/protocol-client";
import {
  ActionsClient,
  OpsClient,
  type ActionOutcome,
  type OperationOutcome,
  type StartingWrite,
  type Write,
  type WriteOutcome,
} from "../../../shared/write-client";
import { originOf } from "../../../shared/write-resolution";
import { createAttachmentReader, type AttachmentReader } from "./attachments";
import { projectEntries } from "./entry-projection";
import { projectPath, readProjectCwdFromPath, readSessionIdFromPath, sessionPath } from "./session-route";
import {
  initialState,
  type AgentAuthResponse,
  type AgentAuthStatus,
  type AppState,
  type AuthTerminalState,
  type PendingElicitation,
  type PendingPermission,
  type TimelineItem,
} from "./state";
import {
  externalFlow,
  isRecord,
  listedSessions,
  needsAuthentication,
  operationError,
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

/** A create or fork this page started, until the document shows what became of it. */
interface Intent {
  kind: "new" | "fork";
  /** The original request, including the instance and held version it was sent against. */
  write: Write;
  /** Once admission is known, losing its retained result cannot undo that fact. */
  accepted: boolean;
  /** The HTTP outcome after a lost answer; null means the read itself failed. */
  read?: OperationOutcome | null;
  navigation: number;
  cwd: string;
  transition: { kind: "new" | "fork"; navigation: number };
}

/** What the page remembers of listing the catalog; the pages themselves are in the document. */
interface ListingState {
  /** The revision a list failed at; it is not listed again until it changes or the user asks. */
  failedRevision?: string;
  /** The user (or a changed catalog) asks for a new first page. */
  refresh: boolean;
  /** The `op.id` the last list was sent against: nothing is sent against it again (C3). */
  sentAgainst?: string;
  /** The list operations this page wrote. */
  readonly mine: Set<string>;
}

/** Where the answer to one interaction is: sent and awaiting the document, or not known to have arrived. */
interface AnswerState {
  state: "sending" | "sent" | "uncertain";
}

interface HeldPrompt {
  id: string;
  blocks: PromptBlock[];
}

/** The operations client whose answers also notify the page: the write queue reports none. */
class NotifyingOps extends OpsClient {
  constructor(
    fetch: HttpFetch,
    replicas: ConstructorParameters<typeof OpsClient>[1],
    private readonly answered: () => void,
  ) {
    super(fetch, replicas);
  }

  override write(scope: string, write: StartingWrite): Promise<WriteOutcome> {
    const sent = super.write(scope, write);
    void sent.then(() => setTimeout(this.answered, 0));
    return sent;
  }
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
  #feed!: ScopeFeed;
  #ops!: NotifyingOps;
  #actions!: ActionsClient;
  #queue!: WriteQueue;
  #route: Route = readRoute();
  /** Counts navigations; a write's late result never moves a page the user moved since. */
  #navigation = 0;
  #follow = "";
  /** The session scope aligned on the current stream. */
  #aligned: string | undefined;
  readonly #ended = new Map<string, End>();
  #authRequired = false;
  #transition: { kind: "new" | "fork"; navigation: number } | undefined;
  /** Failures of this page's own writes, shown in the thread. */
  #errors: Array<Extract<TimelineItem, { type: "error" }>> = [];
  /** Operations this page started on `global`: their failures are its to show. */
  readonly #mine = new Set<string>();
  /** Operations whose end the page has acted on. */
  readonly #seen = new Set<string>();
  #agentPhase: string | undefined;
  readonly #intents: Intent[] = [];
  #listing: ListingState = { refresh: false, mine: new Set() };
  /** The answers this page sent, by interaction. */
  readonly #answers = new Map<string, AnswerState>();
  /** Why an answer was refused, by interaction: the form keeps its values (C2). */
  readonly #answerErrors = new Map<string, string>();
  /** A control in flight on the shown session: no operation holds it (§7). */
  #control: { kind: "mode" | "config"; scope: string; sessionId: string; id: number } | undefined;
  readonly #held = new Map<string, HeldPrompt[]>();
  readonly #paused = new Set<string>();
  #nextId = 0;
  #reader: AttachmentReader | undefined;
  /** The cwd each session reported while shown, for leaving it once it is gone. */
  readonly #sessionCwd = new Map<string, string>();
  #snapshot: WorkspaceState | undefined;
  #closed = false;

  constructor() {
    this.#connect();
  }

  #connect(): void {
    const fetchJson: HttpFetch = (url, init) => fetch(url, init);
    const feed = new ScopeFeed(new TreeFeedClient(openEventSource));
    this.#feed = feed;
    this.#ops = new NotifyingOps(fetchJson, feed, () => {
      if (this.#feed === feed) this.#changed();
    });
    this.#actions = new ActionsClient(fetchJson);
    this.#queue = new WriteQueue(this.#ops, {
      get: (scope) => {
        const replica = feed.get(scope);
        return replica === undefined ? undefined : { live: replica.doc.live, rev: replica.rev };
      },
    });
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
    // A session nothing knows of is not one to report: send the page home (§5.1). The cwd the
    // link itself names counts: the load of a session it names may fail, and the page says so.
    if ((this.#knownCwd(route.sessionId) ?? route.cwd) == null) this.#navigate({ kind: "home" }, true);
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
    if (this.#listing.failedRevision === catalog.revision) return true;
    const last = catalog.pages.at(-1);
    return last != null && last.nextCursor == null;
  }

  /** A list this page wrote that failed: not listed again at that revision, and shown. */
  #settleListing(): void {
    const catalog = this.#catalog();
    if (catalog == null) return;
    const op = catalog.op;
    if (op.kind !== "list" || op.status !== "failed" || !this.#listing.mine.has(op.id) || this.#seen.has(op.id)) return;
    this.#seen.add(op.id);
    this.#listing = { ...this.#listing, failedRevision: catalog.revision };
    if (needsAuthentication(op.error)) this.#authRequired = true;
    if (op.error != null) this.#errors = [...this.#errors, operationError(`op:${op.id}:error`, op, op.error)];
  }

  /** Lists the first page when there is none, and later pages while a session is looked for. */
  #listCatalog(): void {
    const catalog = this.#catalog();
    const global = this.#global();
    if (catalog == null || global?.agent.initialize == null) return;
    if (global.agent.initialize.agentCapabilities?.sessionCapabilities?.list == null) return;
    if (catalog.op.status === "running" || catalog.op.id === this.#listing.sentAgainst) return;
    const route = this.#route;
    // A session the catalog has not listed yet is looked for on its later pages.
    const looking = route.kind === "session" && global.sessions?.[route.sessionId] == null &&
      !this.#catalogSessions().some((session) => session.sessionId === route.sessionId);
    const last = catalog.pages.at(-1);
    if (last == null) {
      if (this.#listing.refresh || this.#listing.failedRevision !== catalog.revision) this.#list(catalog, undefined);
    } else if (this.#listing.refresh) {
      this.#list(catalog, undefined);
    } else if (looking && last.nextCursor != null) {
      this.#list(catalog, last.nextCursor);
    }
  }

  #list(catalog: CatalogLive3, cursor: string | undefined): void {
    this.#listing = { ...this.#listing, refresh: false, sentAgainst: catalog.op.id };
    void this.#ops.write(CATALOG, { kind: "list", args: cursor == null ? {} : { cursor, revision: catalog.revision } })
      .then((outcome) => {
        if (outcome.kind === "accepted") {
          this.#listing.mine.add(outcome.op);
        } else if (outcome.kind === "refused" && outcome.error.code === "catalog_changed") {
          // A definite refusal changed nothing: the first page may be asked for against the same op.
          this.#listing = { ...this.#listing, refresh: true, sentAgainst: undefined };
        } else if (outcome.kind === "refused" && (outcome.error.code === "stale" || outcome.error.code === "not_allowed")) {
          // Another list ran: list again once the document shows a later `op` ended.
          this.#listing = { ...this.#listing, refresh: cursor == null };
        } else if (outcome.kind === "blocked") {
          this.#listing = { ...this.#listing, refresh: cursor == null, sentAgainst: undefined };
        } else {
          this.#listing = { ...this.#listing, failedRevision: catalog.revision };
          this.#report("list", outcome);
        }
        this.#changed();
      });
  }

  listSessions = (cursor?: string): void => {
    const catalog = this.#catalog();
    if (cursor == null) {
      // The user asks again: a new attempt, which may go against the same `op` a refused
      // list was sent against. Nothing is resent without this (C3).
      this.#listing = { ...this.#listing, refresh: true, failedRevision: undefined, sentAgainst: undefined };
      this.#changed();
    } else if (catalog != null && catalog.op.status !== "running") {
      this.#list(catalog, cursor);
    }
  };

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
    this.#settleIntents();
    this.#settleAnswers();
    this.#settleListing();
    this.#listCatalog();
    this.#resolveRoute();
    this.#applyFollow();
    this.#feedQueue();
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
    }
    const sessionOp = this.#shown()?.live.op;
    if (sessionOp != null && !this.#seen.has(sessionOp.id) && sessionOp.status === "failed") {
      this.#seen.add(sessionOp.id);
      if (needsAuthentication(sessionOp.error)) this.#authRequired = true;
    }
    const waiting = this.#authRequired || [...this.#ended.values()].some((end) => needsAuthentication(end.error));
    if ((signedIn || restarted) && waiting) {
      this.#authRequired = false;
      // What failed for want of authentication is tried again (§5.4), the listing included.
      for (const [scope, end] of this.#ended) {
        if (end.reason === "load_failed" && needsAuthentication(end.error)) this.#ended.delete(scope);
      }
      this.#listing = { ...this.#listing, refresh: true, failedRevision: undefined, sentAgainst: undefined };
    }
  }

  /**
   * What became of the creates and forks this page started, read from the documents:
   * the operation it wrote completed with the session (§6.2), or the session whose
   * `origin` is its id is held (§6.6). A late result never moves a page the user moved
   * since.
   */
  #settleIntents(): void {
    const global = this.#global();
    if (global == null) return;
    for (const intent of [...this.#intents]) {
      if (intent.navigation !== this.#navigation) {
        this.#finishIntent(intent);
        continue;
      }
      const { write } = intent;
      const label = intent.kind === "new" ? "create" : "fork";
      const held = this.#feed.get(write.scope);
      const local = this.#ops.outcome(write);
      const localRecord = local?.outcome === "admitted" ? local.record : undefined;
      const readRecord = intent.read?.outcome === "admitted" ? intent.read.record : undefined;
      // A terminal GET can be ahead of a running stream record; status never goes backwards.
      const record = readRecord != null && readRecord.status !== "running" ? readRecord : localRecord ?? readRecord;
      const shownOp = bodyLive(held)?.op;
      const current = shownOp?.id === write.id && shownOp.scope === write.scope ? shownOp : undefined;
      const op = current != null && (current.status !== "running" || record == null || record.status === "running")
        ? current
        : record == null ? undefined : operationOf({ ...record, scope: write.scope }, undefined) as ShownOp;
      const origin = originOf(global.sessions ?? {}, write.id);
      const sessionId = origin ?? (op?.status === "completed" && isRecord(op.result) && typeof op.result.sessionId === "string"
        ? op.result.sessionId : undefined);
      if (sessionId != null) {
        this.#finishIntent(intent);
        this.#navigate({ kind: "session", sessionId, cwd: intent.cwd });
        continue;
      }
      if (op != null || local?.outcome === "admitted" || intent.read?.outcome === "admitted") intent.accepted = true;
      if (op?.status === "failed" || op?.status === "cancelled") {
        this.#finishIntent(intent);
        if (needsAuthentication(op.error)) this.#authRequired = true;
        const fallback = i18n.t(op.status === "failed" ? "errors.operationFailed" : "errors.operationCancelled", { operation: label });
        const item = operationError(`local:${(this.#nextId += 1)}`, op, op.error ?? { code: "unavailable", message: fallback });
        this.#errors = [...this.#errors, { ...item, message: item.message || fallback }];
        continue;
      }
      if (!intent.accepted) {
        // A lagging stream cannot disprove admission. Wait for the read made after HTTP failed.
        if (intent.read === undefined) continue;
        this.#finishIntent(intent);
        this.#reportMessage(label, i18n.t("errors.uncertainWrite", { operation: label }), intent);
        continue;
      }
      const invalidated = (write.instance !== undefined && this.#feed.get(GLOBAL)?.epoch !== write.instance) ||
        this.#feed.endOf(write.scope) !== undefined ||
        (local?.outcome === "unknown" && local.reason !== "expired");
      // Before the GET's admission revision arrives, a missing stream record is only lag.
      const behindRead = readRecord != null && held != null && held.rev < readRecord.rev;
      const retained = current != null || local?.outcome === "admitted";
      const forgotten = !retained && !behindRead && (local?.outcome === "unknown" || local?.outcome === "superseded");
      // A completed GET has no result, and its rev is admission, not completion. Only a
      // terminal stream cut can establish that global has no origin for the completed write.
      const completedInStream = current?.status === "completed" || localRecord?.status === "completed";
      if (completedInStream || invalidated || forgotten) {
        this.#finishIntent(intent);
        this.#reportMessage(label, i18n.t("errors.acceptedResultUnknown", { operation: label }), intent);
      }
    }
  }

  /** Drops what the page holds of interactions no document shows pending any more. */
  #settleAnswers(): void {
    const pending = new Set(this.#pendingInteractions().map(({ interaction }) => interaction.interactionId));
    for (const id of [...this.#answers.keys()]) if (!pending.has(id)) this.#answers.delete(id);
    for (const id of [...this.#answerErrors.keys()]) if (!pending.has(id)) this.#answerErrors.delete(id);
  }

  /** The interactions the shown session, `global` and `catalog` show, each once, with the scope that shows it. */
  #pendingInteractions(): Array<{ scope: string; interaction: Interaction }> {
    const shown = this.#shown();
    const seen = new Set<string>();
    const found: Array<{ scope: string; interaction: Interaction }> = [];
    const sources: Array<[string, Readonly<Record<string, Interaction>> | undefined]> = [
      ...(shown == null ? [] : [[shown.scope, shown.live.interactions] as [string, Readonly<Record<string, Interaction>>]]),
      [GLOBAL, this.#global()?.interactions],
      [CATALOG, this.#catalog()?.interactions],
    ];
    for (const [scope, interactions] of sources) {
      for (const interaction of Object.values(interactions ?? {})) {
        if (seen.has(interaction.interactionId)) continue;
        seen.add(interaction.interactionId);
        found.push({ scope, interaction });
      }
    }
    return found;
  }

  #finishIntent(intent: Intent): void {
    const index = this.#intents.indexOf(intent);
    if (index >= 0) this.#intents.splice(index, 1);
    if (this.#transition === intent.transition) this.#transition = undefined;
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
    this.#transition = undefined;
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

  /**
   * Follows again, and starts the Agent when it is stopped or failed: with no session followed,
   * nothing else would (protocol §7, audit Q8). A connection that is already starting refuses it.
   */
  reconnect = (): void => {
    const phase = this.#global()?.agent.phase;
    if (phase === "stopped" || phase === "error") {
      void this.#actions.restart().then((outcome) => {
        // A refused or unanswered restart leaves the card; the user is told why.
        this.#report("restart", outcome);
        this.#changed();
      });
    }
    this.#applyFollow(true);
  };

  // -------------------------------------------------------------------------
  // Writes

  #reportMessage(operation: string, message: string, intent?: Intent): void {
    if (intent != null && this.#navigation !== intent.navigation) return;
    this.#nextId += 1;
    this.#errors = [...this.#errors, { id: `local:${this.#nextId}`, type: "error", message, operation }];
  }

  /** Shows what a write's answer says when the page cannot leave it to the document. */
  #report(operation: string, outcome: WriteOutcome | ActionOutcome): void {
    let message: string | undefined;
    let data: unknown;
    if (outcome.kind === "refused") {
      message = i18n.t("errors.refused", { operation, message: outcome.error.message });
      data = outcome.error;
    } else if (outcome.kind === "uncertain") {
      message = i18n.t("errors.uncertainWrite", { operation });
    }
    if (message == null) return;
    this.#nextId += 1;
    this.#errors = [...this.#errors, {
      id: `local:${this.#nextId}`, type: "error", message, operation, ...(data !== undefined ? { data } : {}),
    }];
  }

  #write(scope: string, write: StartingWrite, label: string): Promise<WriteOutcome> {
    return this.#ops.write(scope, write).then((outcome) => {
      if (scope === GLOBAL && outcome.kind === "accepted") this.#mine.add(outcome.op);
      this.#report(label, outcome);
      this.#changed();
      return outcome;
    });
  }

  /** Starts a create or fork; its result is read from the documents (§6.2, §6.6). */
  #start(kind: Intent["kind"], scope: string, write: StartingWrite, cwd: string): void {
    const navigation = this.#navigation;
    const label = kind === "new" ? "create" : "fork";
    const transition = this.#transition = { kind, navigation };
    void this.#ops.write(scope, write).then((outcome) => {
      if (navigation !== this.#navigation) return;
      if (outcome.kind === "accepted" && outcome.write != null) {
        this.#intents.push({ kind, write: outcome.write, accepted: true, navigation, cwd, transition });
        if (scope === GLOBAL) this.#mine.add(outcome.op);
      } else if (outcome.kind === "uncertain" && outcome.write != null) {
        const intent: Intent = { kind, write: outcome.write, accepted: false, navigation, cwd, transition };
        this.#intents.push(intent);
        this.#readLost(intent, outcome.write);
      } else {
        if (this.#transition === transition) this.#transition = undefined;
        if (this.#navigation === navigation) this.#report(label, outcome);
      }
      this.#changed();
    });
    this.#changed();
  }

  /**
   * Asks the bridge what became of a lost write (design §9.3). The bridge commits an admission
   * before it answers, so this read, made after the answer was lost, finds the write in the
   * target's log if it was admitted; one that fails leaves it unknown.
   */
  #readLost(intent: Intent, write: Write): void {
    const ops = this.#ops;
    void readJson(ops.resolutionUrl(write))
      .then(
        (body) => ops.outcomeFrom(write, body) ?? null,
        () => null,
      )
      .then((resolution) => {
        intent.read = resolution;
        this.#changed();
      });
  }

  newSession = (cwd: string): boolean => {
    const global = this.#global();
    if (global == null || global.op.status === "running" || this.#transition != null) return false;
    this.#start("new", GLOBAL, { kind: "create", args: { cwd } }, cwd);
    return true;
  };

  forkSession = (): void => {
    const shown = this.#shown();
    if (shown == null || shown.live.op.status === "running" || this.#transition != null) return;
    this.#start("fork", shown.scope, { kind: "fork", args: {} }, shown.live.cwd);
  };

  deleteSession = (sessionId: string): void => {
    void this.#write(GLOBAL, { kind: "delete", args: { sessionId } }, "delete");
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
    void this.#actions.input(GLOBAL, terminalId, data);
  };

  resizeAuthTerminal = (terminalId: string, cols: number, rows: number): void => {
    void this.#actions.resize(GLOBAL, terminalId, cols, rows);
  };

  cancelAuthTerminal = (terminalId: string): void => {
    const op = this.#global()?.op;
    if (op?.id !== terminalId || op.status !== "running") return;
    void this.#ops.cancel(GLOBAL).then((outcome) => {
      this.#report("cancel", outcome);
      this.#changed();
    });
  };

  dismissAuthTerminal = (terminalId: string): void => {
    void this.#actions.dismiss(GLOBAL, terminalId).then(() => this.#changed());
  };

  // -------------------------------------------------------------------------
  // Answers and controls (§7)

  /** Answers one interaction, alone: at most once, whatever else is pending. */
  #respond(interactionId: string, answer: { outcome: unknown } | { response: unknown }): void {
    const found = this.#pendingInteractions().find(({ interaction }) => interaction.interactionId === interactionId);
    if (found == null || this.#answers.has(interactionId)) return;
    const state: AnswerState = { state: "sending" };
    this.#answers.set(interactionId, state);
    this.#answerErrors.delete(interactionId);
    void this.#actions.answer(found.scope, interactionId, answer).then((outcome) => {
      if (this.#answers.get(interactionId) !== state) return;
      if (outcome.kind === "done") {
        // Delivered: the document shows it gone.
        state.state = "sent";
      } else if (outcome.kind === "uncertain") {
        // It may have been delivered: the interaction stays pending until the document says
        // otherwise, and nothing is sent again (C3).
        state.state = "uncertain";
      } else {
        // The input stays with the user (C2): the form keeps its values.
        this.#answers.delete(interactionId);
        if (outcome.kind === "refused" && outcome.error.code !== "not_found") {
          this.#answerErrors.set(interactionId, i18n.t("interaction.refused", { ns: "cards", message: outcome.error.message }));
        }
      }
      this.#changed();
    });
    this.#changed();
  }

  respondPermission = (permissionId: string, outcome: RequestPermissionResponse["outcome"]): void =>
    this.#respond(permissionId, { outcome });

  respondElicitation = (elicitationId: string, response: CreateElicitationResponse): void =>
    this.#respond(elicitationId, { response });

  #interactionStatus(interactionId: string): Pick<PendingPermission, "responseRequestId" | "responseStatus" | "responseError"> {
    const state = this.#answers.get(interactionId);
    if (state != null) {
      return {
        responseRequestId: interactionId,
        ...(state.state === "uncertain" ? { responseStatus: i18n.t("interaction.uncertain", { ns: "cards" }) } : {}),
      };
    }
    const error = this.#answerErrors.get(interactionId);
    return error == null ? {} : { responseError: error };
  }

  /** Sets a mode or an option; held until the Agent answers, also while a prompt runs. */
  #control_(kind: "mode" | "config", label: string, send: (scope: string) => Promise<import("../../../shared/write-client").ActionOutcome>): void {
    const shown = this.#shown();
    if (shown == null) return;
    this.#nextId += 1;
    const control = { kind, scope: shown.scope, sessionId: shown.live.sessionId, id: this.#nextId };
    this.#control = control;
    void send(shown.scope).then((outcome) => {
      if (this.#control === control) this.#control = undefined;
      if (outcome.kind === "refused") {
        this.#nextId += 1;
        this.#errors = [...this.#errors, {
          id: `local:${this.#nextId}`, type: "error", operation: label,
          message: i18n.t("errors.refused", { operation: label, message: outcome.error.message }), data: outcome.error,
        }];
      } else if (outcome.kind === "uncertain") {
        this.#nextId += 1;
        this.#errors = [...this.#errors, {
          id: `local:${this.#nextId}`, type: "error", operation: label, message: i18n.t("errors.uncertainWrite", { operation: label }),
        }];
      }
      this.#changed();
    });
    this.#changed();
  }

  setMode = (modeId: string): void => {
    this.#control_("mode", "set_mode", (scope) => this.#actions.setMode(scope, modeId));
  };

  setConfig = (configId: string, value: string | boolean): void => {
    this.#control_("config", "set_config", (scope) => this.#actions.setConfig(scope, configId, value));
  };

  /** Dismisses the URL flow `interactionId`, wherever it is shown. */
  dismissExternalFlow = (interactionId: string): void => {
    for (const scope of [this.#shown()?.scope, GLOBAL, CATALOG]) {
      if (scope == null) continue;
      const flows = bodyFlows(this.#feed.get(scope));
      if (flows != null && Object.hasOwn(flows, interactionId)) {
        void this.#actions.dismiss(scope, interactionId).then(() => this.#changed());
        return;
      }
    }
  };

  /** Stops the running turn; the queue waits until the user sends again. */
  cancel = (): void => {
    const shown = this.#shown();
    if (shown == null || shown.live.op.status !== "running") return;
    void this.#ops.cancel(shown.scope).then((outcome) => {
      this.#report("cancel", outcome);
      this.#changed();
    });
  };

  // -------------------------------------------------------------------------
  // Prompts (C2, C3)

  /**
   * Queues a prompt for the shown session. A prompt sent while the turn is
   * idle goes ahead of paused ones; the queue sends one at a time, against the
   * `op` the document shows ended.
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
  #feedQueue(): void {
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
    for (const { scope: where, interaction } of this.#pendingInteractions()) {
      // The shown session's own interactions wait until it is shown (a load may still fill it).
      if (where === shown?.scope && !visible) continue;
      const status = this.#interactionStatus(interaction.interactionId);
      if (interaction.kind === "permission") {
        permissions.push({ permissionId: interaction.interactionId, request: interaction.request, ...status });
      } else {
        elicitations.push({ elicitationId: interaction.interactionId, request: interaction.request, ...status });
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
      const projected = projectEntries(live.sessionId, entries ?? [], shown.body.prompts, shown.body.turns ?? []);
      timeline = projected.timeline;
      activePlan = projected.activePlan;
      agentActivity = projected.activity;
    }

    // Failures of the global operations this page started, and its own write answers.
    if (globalOp != null && globalOp.status === "failed" && globalOp.error != null && this.#mine.has(globalOp.id) &&
      authOp == null) {
      timeline = [...timeline, operationError(`op:${globalOp.id}:error`, globalOp, globalOp.error)];
    }
    timeline = [...timeline, ...this.#errors];

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
    const runtimeKind = op?.status === "running" && op.kind === "fork" ? op.kind : undefined;
    const deleting = globalOp?.kind === "delete" && globalOp.status === "running" && isRecord(globalOp.input) &&
      typeof globalOp.input.sessionId === "string" ? globalOp.input.sessionId : undefined;

    const summaries = Object.values(global?.sessions ?? {});
    const sessions = listedSessions(this.#catalogSessions(), global);
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
      lastAuthResponse: authOp?.status === "completed" && authOp.kind !== "terminal_login" ? {
        kind: authOp.kind === "logout" ? "logout" : "authenticate",
        response: (isRecord(authOp.result) ? authOp.result : {}) as AgentAuthResponse["response"],
      } : undefined,
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
      pendingSessionDeletions: deleting != null ? [{ requestId: globalOp!.id, sessionId: deleting }] : [],
      runtimeOperation: runtimeKind != null ? { requestId: op!.id, operation: "fork" } : undefined,
      sessionTransition: this.#transition == null || this.#transition.navigation !== this.#navigation ? undefined : {
        kind: this.#transition.kind,
        requestId: `transition:${this.#transition.kind}`,
      },
      pendingSessionControl: this.#control == null || this.#control.sessionId !== live?.sessionId ? undefined : {
        requestId: `control:${this.#control.id}`, sessionId: this.#control.sessionId, kind: this.#control.kind,
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
      queue: shown == null ? [] : this.#queueEntries(shown.scope, shown.live.sessionId),
      queuePaused: shown != null && this.#paused.has(shown.scope),
      authTerminals,
      standaloneTerminals: standaloneTerminals(terminalSnapshots, timeline),
      attachments: this.#reader,
      sessionOpening,
      projectCwd: route.kind === "project" ? route.cwd : undefined,
    };
  }
}

/** The URL flows a replica's `live` holds. */
function bodyFlows(replica: { readonly doc: Readonly<Record<string, unknown>> } | undefined): Readonly<Record<string, unknown>> | undefined {
  const live = replica?.doc.live;
  return isRecord(live) && isRecord(live.urlFlows) ? live.urlFlows : undefined;
}

/** The `live` of a replica, if it holds one. */
function bodyLive(replica: { readonly doc: Readonly<Record<string, unknown>> } | undefined): { op?: ShownOp } | undefined {
  const live = replica?.doc.live;
  return isRecord(live) ? (live as { op?: ShownOp }) : undefined;
}

function promptOf(write: StartingWrite): PromptBlock[] {
  return write.kind === "prompt" ? write.args.prompt : [];
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
    authenticate: workspace.authenticate,
    writeAuthTerminal: workspace.writeAuthTerminal,
    resizeAuthTerminal: workspace.resizeAuthTerminal,
    cancelAuthTerminal: workspace.cancelAuthTerminal,
    dismissAuthTerminal: workspace.dismissAuthTerminal,
    logout: workspace.logout,
    newSession: workspace.newSession,
    forkSession: workspace.forkSession,
    deleteSession: workspace.deleteSession,
    listSessions: workspace.listSessions,
    respondPermission: workspace.respondPermission,
    respondElicitation: workspace.respondElicitation,
    dismissExternalFlow: workspace.dismissExternalFlow,
    setMode: workspace.setMode,
    setConfig: workspace.setConfig,
    prompt: workspace.prompt,
    cancel: workspace.cancel,
    pauseQueue: workspace.pauseQueue,
    takeQueued: workspace.takeQueued,
    clearQueue: workspace.clearQueue,
    sendQueuedNow: workspace.sendQueuedNow,
    resendQueued: workspace.resendQueued,
    searchWorkspaceContext: workspace.searchWorkspaceContext,
    readWorkspaceContext: workspace.readWorkspaceContext,
  };
}
