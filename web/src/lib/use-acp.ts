import i18n from "../i18n";
import { useCallback, useEffect, useRef, useState } from "react";
import type {
  ContentBlock,
  CreateElicitationResponse,
  RequestPermissionResponse,
  SessionInfo,
} from "@agentclientprotocol/sdk";
import {
  type BridgeSessionView,
  type BridgeTurnProcessPage,
  type SessionListResult,
  type SessionOwner,
  type WorkspaceContextAttachment,
  type WorkspaceContextMatch,
  sameSessionOwner,
} from "../../../shared/business-api";
import type {
  GlobalState,
  Request,
  RequestAccepted,
  SessionState,
  TurnAccepted,
} from "../../../shared/protocol";
import { GLOBAL_SCOPE, sessionSubscription } from "../../../shared/replica";
import { ApiError, requestJson, workspaceContextSearchPath } from "./api-client";
import { randomId } from "./id";
import { projectPath, readProjectCwdFromPath, readSessionIdFromPath, sessionPath } from "./session-route";
import { appReducer, requestRecordError, type DeferredTurnProcess } from "./state";
import { useAppStore } from "./app-store";
import { sameTurnProcess } from "./process-retention";
import { containsHostedAttachment } from "./hosted-attachment";
import { V2Client, protocolErrorCode, type EventSourceLike } from "./v2-client";
import { ObservedProcesses, bridgeSessionView, turnUpdateDelta } from "./v2-session-view";

interface Owner { sessionId: string; epoch: string; incarnation: number }

function ownerOf(view: SessionOwner): Owner {
  return { sessionId: view.sessionId, epoch: view.bridgeEpoch, incarnation: view.sessionIncarnation };
}

function sessionOwner(owner: Owner): SessionOwner {
  return { bridgeEpoch: owner.epoch, sessionId: owner.sessionId, sessionIncarnation: owner.incarnation };
}

function ownerTag(owner: Owner): string {
  return `${owner.epoch}:${owner.incarnation}`;
}

export interface UseAcpOptions {
  /** Test seam for the stream transport. */
  eventSource?: (url: string) => EventSourceLike;
}

export function useAcp(options: UseAcpOptions = {}) {
  const [state, store] = useAppStore();
  const { dispatch } = store;
  const [projectCwd, setProjectCwd] = useState(() => readProjectCwdFromPath(window.location.pathname));
  // Presentation for a foreground session open, separate from live replication.
  const [sessionOpening, setSessionOpening] = useState<{ sessionId: string; error?: string } | undefined>(() => {
    const sessionId = readSessionIdFromPath(window.location.pathname);
    return sessionId == null ? undefined : { sessionId };
  });

  const clientRef = useRef<V2Client | undefined>(undefined);
  const activeSessionIdRef = useRef<string | undefined>(undefined);
  /** The session owner the stream subscribes to. */
  const subscribedRef = useRef<Owner | undefined>(undefined);
  const epochRef = useRef<string | undefined>(undefined);
  const previousGlobalRef = useRef<GlobalState | undefined>(undefined);
  const previousSessionRef = useRef<{ state: SessionState; view: BridgeSessionView } | undefined>(undefined);
  const processesRef = useRef(new ObservedProcesses());
  const shownFailuresRef = useRef(new Set<string>());
  /** A submitted prompt the replica does not show yet. */
  const pendingTurnRef = useRef<{ key: string; sessionId: string } | undefined>(undefined);
  const navigationRef = useRef(0);
  const routeRestoreRef = useRef<{ sessionId: string; epoch: string } | undefined>(undefined);
  const terminalRequestsRef = useRef(new Map<string, string>());
  const authInputQueueRef = useRef<{ requestId: string; tail: Promise<void>; closed: boolean } | undefined>(undefined);
  const catalogRevisionRef = useRef<string | undefined>(undefined);
  const catalogInvalidationRef = useRef(0);
  const catalogRefreshRef = useRef<{ pending: boolean } | undefined>(undefined);
  const sessionListQueueRef = useRef<Promise<unknown>>(Promise.resolve());
  const restoreRouteRef = useRef<() => void>(() => undefined);
  const invalidateSessionListRef = useRef<() => void>(() => undefined);

  const syncGlobalRef = useRef<() => void>(() => undefined);
  const syncSessionRef = useRef<() => void>(() => undefined);
  const reportErrorRef = useRef<(error: unknown) => void>(() => undefined);
  const client = (): V2Client => clientRef.current ??= new V2Client({
    eventSource: options.eventSource,
    onChange: (scopes) => {
      if (scopes.has(GLOBAL_SCOPE)) syncGlobalRef.current();
      if ([...scopes].some((scope) => scope !== GLOBAL_SCOPE)) syncSessionRef.current();
    },
    onOpen: () => {
      dispatch({ type: "socket/open" });
      // A closed socket terminated the presentation: project everything again.
      if (previousGlobalRef.current != null) {
        previousGlobalRef.current = undefined;
        syncGlobalRef.current();
      }
      if (previousSessionRef.current != null) {
        previousSessionRef.current = undefined;
        syncSessionRef.current();
      }
    },
    onError: () => dispatch({ type: "socket/closed" }),
    onFailure: (error) => reportErrorRef.current(error),
  });

  const closeAuthInput = useCallback((requestId?: string) => {
    const queue = authInputQueueRef.current;
    if (queue != null && (requestId == null || queue.requestId === requestId)) queue.closed = true;
  }, []);

  const subscriptions = () => {
    const owner = subscribedRef.current;
    return owner == null ? [GLOBAL_SCOPE] : [GLOBAL_SCOPE, sessionSubscription(owner.sessionId, owner.epoch, owner.incarnation)];
  };

  const subscribe = useCallback((owner: Owner | undefined) => {
    const current = subscribedRef.current;
    if (current?.sessionId === owner?.sessionId && current?.epoch === owner?.epoch &&
      current?.incarnation === owner?.incarnation) return;
    subscribedRef.current = owner;
    previousSessionRef.current = undefined;
    processesRef.current.clear();
    shownFailuresRef.current.clear();
    client().connect(subscriptions());
  }, []);

  const resetSession = useCallback((preserve = false) => {
    setSessionOpening(undefined);
    activeSessionIdRef.current = undefined;
    pendingTurnRef.current = undefined;
    subscribe(undefined);
    dispatch({ type: preserve ? "session/deselect" : "session/reset" });
  }, [subscribe]);

  const returnHome = useCallback((replace = false, preserve = true) => {
    navigationRef.current += 1;
    setRoute("/", replace);
    setProjectCwd(undefined);
    resetSession(preserve);
  }, [resetSession]);

  const returnToSessionProject = useCallback((sessionId: string) => {
    if (activeSessionIdRef.current !== sessionId) return;
    const cwd = store.getState().sessionView?.workspace.cwd ?? readProjectCwdFromPath(window.location.pathname);
    const path = cwd == null ? "/" : projectPath(cwd);
    navigationRef.current += 1;
    setRoute(path, true);
    setProjectCwd(path === "/" ? undefined : cwd);
    resetSession();
  }, [resetSession]);

  const navigateSession = useCallback((sessionId: string, cwd?: string | null, replace = false) => {
    const validCwd = cwd != null && projectPath(cwd) !== "/" ? cwd : undefined;
    setRoute(sessionPath(sessionId, validCwd), replace);
    setProjectCwd(validCwd);
  }, []);

  const reportError = useCallback((error: unknown) => {
    const failure = protocolFailure(error);
    if (failure != null) {
      dispatch({ type: "request/failed", error: failure });
      return;
    }
    dispatch({ type: "client/error", message: error instanceof Error ? error.message : String(error) });
  }, []);
  reportErrorRef.current = reportError;

  const reportRequestError = useCallback((error: unknown, requestId: string, operation: string) => {
    const failure = protocolFailure(error);
    dispatch({
      type: "request/failed",
      error: {
        ...(failure ?? { message: error instanceof Error ? error.message : String(error) }),
        requestId,
        operation,
      },
    });
  }, []);

  const reportRecordError = useCallback((record: Request | undefined, requestId: string, operation: string) => {
    dispatch({
      type: "request/failed",
      error: {
        ...(record == null
          ? { message: i18n.t("errors.requestResultUnknown", { defaultValue: "The result of this request is no longer available" }) }
          : requestRecordError(record)),
        requestId,
        operation,
      },
    });
  }, []);

  // ---------------------------------------------------------------------------
  // Replication

  const syncSession = useCallback(() => {
    const owner = subscribedRef.current;
    if (owner == null || activeSessionIdRef.current !== owner.sessionId) return;
    const replica = client().replica;
    const scope = `session/${owner.sessionId}`;
    const ended = replica.ended(scope);
    if (ended != null) {
      const view = store.getState().sessionView;
      subscribe(undefined);
      pendingTurnRef.current = undefined;
      if (ended === "replaced") {
        // The session is held under another owner: open it again.
        if (view != null) dispatch({ type: "bridge/connection_replaced" });
        restoreRouteRef.current();
        return;
      }
      dispatch({ type: "session/retired", owner: sessionOwner(owner), deleted: ended === "deleted" });
      returnToSessionProject(owner.sessionId);
      return;
    }
    const document = replica.document<SessionState>(scope);
    if (document == null || document.incarnation !== owner.incarnation) return;
    const previous = previousSessionRef.current;
    if (previous?.state === document) return;
    processesRef.current.observe(previous?.state, document);
    const view = bridgeSessionView(document, owner.epoch, replica.installed(scope), processesRef.current);
    previousSessionRef.current = { state: document, view };

    const pending = pendingTurnRef.current;
    const reflected = pending == null || pending.sessionId !== owner.sessionId ||
      document.turn?.clientIntentId === pending.key ||
      Object.values(document.outcomes).some(({ clientIntentId }) => clientIntentId === pending.key);
    if (reflected) pendingTurnRef.current = undefined;
    const structural = previous == null || previous.state.history !== document.history ||
      previous.state.turn?.operationId !== document.turn?.operationId;
    const turnUpdates = previous == null ? undefined : previous.state.turn == null || document.turn == null
      ? [] : turnUpdateDelta(previous.state.turn.updates, document.turn.updates);
    if (previous != null && (!reflected || (!structural && turnUpdates != null))) {
      // A prompt this client sent stays visible until the replica shows it.
      dispatch({ type: "v2/session_live", view, previous: previous.view, turnUpdates: reflected ? turnUpdates ?? [] : [] });
    } else {
      dispatch({ type: "bridge/session_hydrate", view });
      if (previous == null) {
        navigateSession(document.sessionId, document.cwd, true);
        setSessionOpening((opening) => opening?.sessionId === document.sessionId ? undefined : opening);
      }
    }
    for (const outcome of Object.values(document.outcomes)) {
      if (outcome.status !== "failed" || shownFailuresRef.current.has(outcome.operationId)) continue;
      shownFailuresRef.current.add(outcome.operationId);
      const failure = outcome.error == null ? { message: "The turn failed" } : requestRecordError({
        error: outcome.error, method: "session/prompt",
      } as Request);
      dispatch({
        type: "bridge/turn_failed",
        event: {
          type: "bridge/session_turn_failed",
          bridgeEpoch: owner.epoch,
          sessionId: owner.sessionId,
          sessionIncarnation: owner.incarnation,
          viewRevision: view.viewRevision,
          historyRevision: document.history.revision,
          phase: document.history.phase,
          operationId: outcome.operationId,
          clientIntentId: outcome.clientIntentId,
          prompt: outcome.prompt ?? [],
          error: failure,
        },
        view,
      });
    }
  }, [navigateSession, returnToSessionProject, subscribe]);

  const syncGlobal = useCallback(() => {
    const replica = client().replica;
    const global = replica.document<GlobalState>(GLOBAL_SCOPE);
    if (replica.epoch !== epochRef.current) {
      const replaced = epochRef.current != null;
      epochRef.current = replica.epoch;
      previousGlobalRef.current = undefined;
      catalogRevisionRef.current = undefined;
      terminalRequestsRef.current.clear();
      if (replaced) {
        closeAuthInput();
        pendingTurnRef.current = undefined;
        processesRef.current.clear();
        shownFailuresRef.current.clear();
        previousSessionRef.current = undefined;
        // Keep the route and mounted draft while the old connection's
        // presentation is replaced from the new epoch.
        dispatch({ type: "bridge/connection_replaced" });
      }
    }
    if (global == null) return;
    const previous = previousGlobalRef.current;
    if (previous === global) return;
    previousGlobalRef.current = global;
    for (const request of Object.values(global.requests)) {
      if (request.operation === "auth_terminal" && request.clientIntentId != null) {
        terminalRequestsRef.current.set(request.clientIntentId, request.requestId);
      }
      if (request.operation === "auth_terminal" && request.status !== "running") {
        closeAuthInput(request.requestId);
        if (request.clientIntentId != null) closeAuthInput(request.clientIntentId);
      }
    }
    dispatch({ type: "v2/global", global, previous });
    if (global.agent.phase === "ready" && previous?.agent.phase !== "ready") {
      if (store.getState().authTerminal?.status === "succeeded") dispatch({ type: "auth/dismiss_terminal" });
      restoreRouteRef.current();
    }
    if (global.agent.phase !== "ready") pendingTurnRef.current = undefined;
    if (previous != null && previous.catalog.revision !== global.catalog.revision) {
      catalogInvalidationRef.current += 1;
      invalidateSessionListRef.current();
    }
  }, [closeAuthInput]);

  syncGlobalRef.current = syncGlobal;
  syncSessionRef.current = syncSession;

  // ---------------------------------------------------------------------------
  // Opening sessions

  /** Opens a session (protocol §4.2) and subscribes to the owner it names. */
  const openSession = useCallback(async (sessionId: string, cwd: string, isCurrent: () => boolean) => {
    const key = randomId();
    const { result } = await client().command<RequestAccepted>(
      `/api/v2/sessions/${encodeURIComponent(sessionId)}/open`, { key, body: { cwd } },
    );
    if (!isCurrent()) return;
    const record = client().global()?.requests[result.requestId];
    const incarnation = record?.result?.kind === "session" ? record.result.incarnation : record?.target?.incarnation;
    const epoch = client().replica.epoch;
    if (incarnation == null || epoch == null) {
      const finished = await client().finished(result.requestId);
      if (finished?.status !== "succeeded" || finished.result?.kind !== "session") {
        throw new Error(finished == null ? "The session could not be opened" : requestRecordError(finished).message);
      }
      if (isCurrent()) subscribe({ sessionId, epoch: client().replica.epoch!, incarnation: finished.result.incarnation });
      return;
    }
    subscribe({ sessionId, epoch, incarnation });
  }, [subscribe]);

  const activateSession = useCallback((session: SessionInfo, transition = true) => {
    if (transition) navigationRef.current += 1;
    const navigation = navigationRef.current;
    navigateSession(session.sessionId, session.cwd, !transition);
    if (activeSessionIdRef.current === session.sessionId && store.getState().sessionView != null) return;
    const requestId = randomId();
    setSessionOpening({ sessionId: session.sessionId });
    activeSessionIdRef.current = session.sessionId;
    pendingTurnRef.current = undefined;
    dispatch({ type: "session/view_synced", view: undefined });
    if (transition) {
      dispatch({
        type: "session/transition_start",
        kind: "attach",
        requestId,
        sessionId: session.sessionId,
        cwd: session.cwd,
        title: session.title,
      });
    }
    const held = client().global()?.sessions[session.sessionId];
    const cwd = held?.cwd ?? session.cwd;
    void openSession(session.sessionId, cwd, () =>
      navigationRef.current === navigation && activeSessionIdRef.current === session.sessionId,
    ).catch((error: unknown) => {
      if (navigationRef.current !== navigation || activeSessionIdRef.current !== session.sessionId) return;
      setSessionOpening({ sessionId: session.sessionId, error: error instanceof Error ? error.message : String(error) });
      reportError(error);
    });
  }, [navigateSession, openSession, reportError]);

  // ---------------------------------------------------------------------------
  // The Agent-owned catalog (protocol §6)

  const sessionListSupported = () =>
    client().global()?.agent.initialize?.agentCapabilities?.sessionCapabilities?.list != null;

  const readSessionList = useCallback(async (cursor?: string, isCurrent: () => boolean = () => true) => {
    if (!sessionListSupported()) return { sessions: [] } as SessionListResult;
    const invalidation = catalogInvalidationRef.current;
    const query = new URLSearchParams();
    if (cursor != null) {
      query.set("cursor", cursor);
      if (catalogRevisionRef.current != null) query.set("revision", catalogRevisionRef.current);
    }
    const suffix = query.size === 0 ? "" : `?${query}`;
    const page = await requestJson<{ revision: string; sessions: SessionInfo[]; nextCursor?: string | null }>(
      `/api/v2/catalog${suffix}`,
    );
    const response: SessionListResult = { sessions: page.sessions, nextCursor: page.nextCursor ?? null, catalogRevision: page.revision };
    if (!isCurrent()) return response;
    if (catalogInvalidationRef.current !== invalidation && cursor != null) {
      throw new ApiError("Session catalog changed", 409, { error: { code: "catalog_changed", message: "catalog changed" } });
    }
    catalogRevisionRef.current = page.revision;
    dispatch({ type: "sessions/listed", cursor, response });
    return response;
  }, []);

  const withSessionList = useCallback(<T,>(operation: () => Promise<T>): Promise<T> => {
    const run = async () => {
      try { return await operation(); }
      catch (error) {
        if (protocolErrorCode(error) !== "catalog_changed") throw error;
        return operation();
      }
    };
    const result = sessionListQueueRef.current.then(run, run);
    sessionListQueueRef.current = result.catch(() => undefined);
    return result;
  }, []);

  const refreshSessionList = useCallback((cursor?: string) => withSessionList(async () => {
    const pathname = window.location.pathname;
    const discoverProject = cursor == null && (
      readProjectCwdFromPath(pathname) != null || readSessionIdFromPath(pathname) != null
    );
    let listed = await readSessionList(cursor);
    const cursors = new Set<string>();
    while (discoverProject && listed.nextCursor != null && window.location.pathname === pathname) {
      if (cursors.has(listed.nextCursor)) throw new Error(i18n.t("errors.repeatedCursor"));
      cursors.add(listed.nextCursor);
      listed = await readSessionList(listed.nextCursor);
    }
    return listed;
  }), [readSessionList, withSessionList]);

  const invalidateSessionList = useCallback(() => {
    if (catalogRefreshRef.current != null) {
      catalogRefreshRef.current.pending = true;
      return;
    }
    const refresh = { pending: false };
    catalogRefreshRef.current = refresh;
    void (async () => {
      try {
        do {
          refresh.pending = false;
          await refreshSessionList();
        } while (refresh.pending);
      } catch (error) { reportError(error); }
      finally { catalogRefreshRef.current = undefined; }
    })();
  }, [refreshSessionList, reportError]);
  invalidateSessionListRef.current = invalidateSessionList;

  // ---------------------------------------------------------------------------
  // The route

  /** Restores the routed session once the Agent is ready, in this epoch. */
  const restoreRoute = useCallback(() => {
    const global = client().global();
    const epoch = client().replica.epoch;
    if (global?.agent.phase !== "ready" || epoch == null) return;
    const navigation = navigationRef.current;
    const routeSessionId = readSessionIdFromPath(window.location.pathname);
    const routeProjectCwd = readProjectCwdFromPath(window.location.pathname);
    const stillCurrent = () => navigationRef.current === navigation &&
      readSessionIdFromPath(window.location.pathname) === routeSessionId && client().replica.epoch === epoch;
    const discovery = withSessionList(async () => {
      let listed = await readSessionList(undefined, stillCurrent);
      if (!stillCurrent() || (routeSessionId == null && routeProjectCwd == null)) return undefined;
      let selected = listed.sessions.find(({ sessionId }) => sessionId === routeSessionId);
      const cursors = new Set<string>();
      while (selected == null && listed.nextCursor != null && stillCurrent()) {
        if (cursors.has(listed.nextCursor)) throw new Error(i18n.t("errors.repeatedCursor"));
        cursors.add(listed.nextCursor);
        listed = await readSessionList(listed.nextCursor, stillCurrent);
        selected = listed.sessions.find(({ sessionId }) => sessionId === routeSessionId);
      }
      return selected;
    }).then(
      (selected) => ({ ok: true as const, selected }),
      (error: unknown) => {
        if (stillCurrent()) reportError(error);
        return { ok: false as const, error };
      },
    );
    if (routeSessionId == null) return;
    const restore = routeRestoreRef.current;
    if (restore?.sessionId === routeSessionId && restore.epoch === epoch && subscribedRef.current != null) return;
    routeRestoreRef.current = { sessionId: routeSessionId, epoch };
    activeSessionIdRef.current = routeSessionId;
    if (store.getState().sessionView == null) setSessionOpening({ sessionId: routeSessionId });
    void (async () => {
      let cwd = global.sessions[routeSessionId]?.cwd ?? routeProjectCwd;
      if (cwd == null) {
        const discovered = await discovery;
        if (!stillCurrent()) return;
        if (!discovered.ok) {
          setSessionOpening({ sessionId: routeSessionId, error: errorMessage(discovered.error) });
          return;
        }
        cwd = discovered.selected?.cwd ?? undefined;
      }
      if (cwd == null) {
        returnHome(true, false);
        return;
      }
      await openSession(routeSessionId, cwd, stillCurrent);
    })().catch((error: unknown) => {
      if (!stillCurrent()) return;
      routeRestoreRef.current = undefined;
      if (protocolErrorCode(error) === "not_found") {
        returnHome(true, false);
        return;
      }
      setSessionOpening({ sessionId: routeSessionId, error: errorMessage(error) });
      reportError(error);
    });
  }, [openSession, readSessionList, reportError, returnHome, withSessionList]);
  restoreRouteRef.current = restoreRoute;

  const retrySessionOpening = useCallback(() => {
    const sessionId = readSessionIdFromPath(window.location.pathname);
    if (sessionId == null) return;
    setSessionOpening({ sessionId });
    routeRestoreRef.current = undefined;
    subscribedRef.current = undefined;
    restoreRoute();
  }, [restoreRoute]);

  const openProject = useCallback((cwd: string) => {
    const path = projectPath(cwd);
    if (path === "/") {
      returnHome();
      return;
    }
    navigationRef.current += 1;
    setRoute(path);
    setProjectCwd(cwd);
    resetSession(true);
    void refreshSessionList().catch(reportError);
  }, [refreshSessionList, reportError, resetSession, returnHome]);

  const reconnect = useCallback(() => client().connect(subscriptions()), []);

  useEffect(() => {
    const current = client();
    current.connect(subscriptions());
    const restoreFromHistory = () => {
      navigationRef.current += 1;
      const cwd = readProjectCwdFromPath(window.location.pathname);
      const sessionId = readSessionIdFromPath(window.location.pathname);
      if (cwd == null && sessionId == null) returnHome(true);
      else {
        setProjectCwd(cwd);
        setRoute(sessionId == null ? projectPath(cwd!) : sessionPath(sessionId, cwd), true);
        if (sessionId == null || activeSessionIdRef.current !== sessionId) {
          routeRestoreRef.current = undefined;
          resetSession(true);
        }
      }
      restoreRoute();
    };
    const recoverClosedStream = () => {
      if (current.closed() || store.getState().phase === "stopped" || store.getState().phase === "error") {
        current.connect(subscriptions());
      }
    };
    window.addEventListener("online", recoverClosedStream);
    window.addEventListener("focus", recoverClosedStream);
    window.addEventListener("pageshow", recoverClosedStream);
    window.addEventListener("popstate", restoreFromHistory);
    document.addEventListener("visibilitychange", recoverClosedStream);
    return () => {
      closeAuthInput();
      navigationRef.current += 1;
      window.removeEventListener("online", recoverClosedStream);
      window.removeEventListener("focus", recoverClosedStream);
      window.removeEventListener("pageshow", recoverClosedStream);
      window.removeEventListener("popstate", restoreFromHistory);
      document.removeEventListener("visibilitychange", recoverClosedStream);
      current.dispose();
      clientRef.current = undefined;
    };
  }, []);

  // ---------------------------------------------------------------------------
  // Workspace reads

  const searchWorkspaceContext = useCallback(async (query: string) => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null) throw new Error(i18n.t("errors.searchNotReady"));
    const response = await requestJson<{ matches: WorkspaceContextMatch[] }>(workspaceContextSearchPath(query, sessionId));
    return response.matches;
  }, []);

  const readWorkspaceContext = useCallback(async (path: string) => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null) throw new Error(i18n.t("errors.contextNotReady"));
    const response = await requestJson<{ attachment: WorkspaceContextAttachment }>(
      `/api/v2/sessions/${encodeURIComponent(sessionId)}/files/content?${new URLSearchParams({ path })}`,
    );
    return response.attachment;
  }, []);

  // ---------------------------------------------------------------------------
  // Turns

  const prompt = useCallback((blocks: ContentBlock[]) => {
    const current = store.getState();
    const view = current.sessionView;
    const owner = subscribedRef.current;
    const document = owner == null ? undefined : client().session(owner.sessionId);
    if (
      current.session == null || current.running || current.pendingPrompt != null ||
      current.sessionTransition != null || current.pendingSessionControl != null ||
      current.pendingSessionDeletions.some(({ sessionId }) => sessionId === current.session?.sessionId) ||
      current.runtimeOperation != null || view == null || owner == null || document == null ||
      document.history.phase !== "ready" || document.history.revision == null || document.turn != null
    ) return false;
    const key = randomId();
    const sessionId = owner.sessionId;
    const navigation = navigationRef.current;
    pendingTurnRef.current = { key, sessionId };
    dispatch({ type: "user/prompt", requestId: key, sessionId, blocks });
    void client().command<TurnAccepted>(`/api/v2/sessions/${encodeURIComponent(sessionId)}/turns`, {
      key,
      ifMatch: `${ownerTag(owner)}:${document.history.revision}`,
      body: { prompt: blocks },
    }).catch((error: unknown) => {
      if (pendingTurnRef.current?.key === key) pendingTurnRef.current = undefined;
      if (navigationRef.current !== navigation || subscribedRef.current !== owner) return;
      reportRequestError(error, key, "session/prompt");
    });
    return true;
  }, [reportRequestError]);

  const cancel = useCallback(() => {
    const owner = subscribedRef.current;
    const turn = owner == null ? undefined : client().session(owner.sessionId)?.turn;
    if (owner == null || turn == null || turn.cancelRequested) return;
    void client().command(
      `/api/v2/sessions/${encodeURIComponent(owner.sessionId)}/turns/${encodeURIComponent(turn.operationId)}/cancel`,
      { ifMatch: ownerTag(owner), body: {} },
    ).catch(reportError);
  }, [reportError]);

  // ---------------------------------------------------------------------------
  // Session commands

  const setControl = useCallback((kind: "mode" | "config", path: string, body: unknown, operation: string) => {
    const owner = subscribedRef.current;
    if (owner == null || !["ready", "running", "reconciling"].includes(store.getState().sessionView?.phase ?? "")) return;
    const key = randomId();
    dispatch({ type: "session/control_start", kind, requestId: key, sessionId: owner.sessionId });
    void client().operation(path, { method: "PUT", key, ifMatch: ownerTag(owner), body }).then(({ record }) => {
      if (record?.status === "succeeded") dispatch({ type: "session/control_settled", requestId: key });
      else reportRecordError(record, key, operation);
    }, (error: unknown) => reportRequestError(error, key, operation));
  }, [reportRecordError, reportRequestError]);

  const setMode = useCallback((modeId: string) => {
    const sessionId = subscribedRef.current?.sessionId;
    if (sessionId == null) return;
    setControl("mode", `/api/v2/sessions/${encodeURIComponent(sessionId)}/mode`, { modeId }, "session/set_mode");
  }, [setControl]);

  const setConfig = useCallback((configId: string, value: string | boolean) => {
    const sessionId = subscribedRef.current?.sessionId;
    if (sessionId == null) return;
    setControl("config", `/api/v2/sessions/${encodeURIComponent(sessionId)}/config/${encodeURIComponent(configId)}`,
      { value }, "session/set_config_option");
  }, [setControl]);

  /** `If-Match` of an interaction: its session owner, or the epoch for `global`. */
  const interactionTag = (interactionId: string): string | undefined => {
    const epoch = client().replica.epoch;
    if (epoch == null) return undefined;
    if (client().global()?.interactions[interactionId] != null) return epoch;
    const owner = subscribedRef.current;
    return owner == null ? undefined : ownerTag(owner);
  };

  const respond = useCallback((interactionId: string, body: unknown, operation: "permission/respond" | "elicitation/respond") => {
    const ifMatch = interactionTag(interactionId);
    if (ifMatch == null) return;
    const key = randomId();
    dispatch(operation === "permission/respond"
      ? { type: "permission/respond_start", permissionId: interactionId, requestId: key }
      : { type: "elicitation/respond_start", elicitationId: interactionId, requestId: key });
    // The interaction leaves the document once delivered; the record says
    // whether delivery failed.
    void client().operation(`/api/v2/interactions/${encodeURIComponent(interactionId)}`, { key, ifMatch, body })
      .then(({ record }) => {
        if (record != null && record.status !== "succeeded") reportRecordError(record, key, operation);
      }, (error: unknown) => reportRequestError(error, key, operation));
  }, [reportRecordError, reportRequestError]);

  const respondPermission = useCallback((permissionId: string, outcome: RequestPermissionResponse["outcome"]) => {
    respond(permissionId, { outcome }, "permission/respond");
  }, [respond]);

  const respondElicitation = useCallback((elicitationId: string, response: CreateElicitationResponse) => {
    respond(elicitationId, { response }, "elicitation/respond");
  }, [respond]);

  // ---------------------------------------------------------------------------
  // Global commands (protocol §5.4)

  const authenticate = useCallback((methodId: string) => {
    if (store.getState().pendingAuth != null) return;
    const method = store.getState().initialized?.authMethods?.find(({ id }) => id === methodId);
    const terminal = method != null && "type" in method && method.type === "terminal";
    const key = randomId();
    dispatch({ type: "auth/start", requestId: key, kind: terminal ? "terminal" : "authenticate", methodId });
    void client().command<RequestAccepted>(
      terminal ? "/api/v2/agent/auth-terminals" : "/api/v2/agent/authenticate",
      { key, body: terminal ? { methodId, cols: 80, rows: 24 } : { methodId } },
    ).then(({ result }) => {
      if (terminal) terminalRequestsRef.current.set(key, result.requestId);
    }, (error: unknown) => reportRequestError(error, key, terminal ? "auth/terminal_start" : "auth/authenticate"));
  }, [reportRequestError]);

  const terminalPath = (requestId: string, control: string) =>
    `/api/v2/agent/auth-terminals/${encodeURIComponent(terminalRequestsRef.current.get(requestId) ?? requestId)}/${control}`;

  const writeAuthTerminal = useCallback((requestId: string, data: string) => {
    const acceptsInput = () => {
      const terminal = store.getState().authTerminal;
      return terminal?.requestId === requestId && (terminal.status === "starting" || terminal.status === "running");
    };
    if (!acceptsInput()) return;
    const queue = authInputQueueRef.current?.requestId === requestId
      ? authInputQueueRef.current
      : { requestId, tail: Promise.resolve(), closed: false };
    authInputQueueRef.current = queue;
    if (queue.closed) return;
    // Separate HTTP requests can arrive out of order. Wait for each PTY write
    // before sending the next keystroke, including Enter.
    queue.tail = queue.tail.then(async () => {
      if (authInputQueueRef.current !== queue || queue.closed || !acceptsInput()) return;
      await client().command(terminalPath(requestId, "input"), { body: { data } });
    }).catch((error) => {
      if (authInputQueueRef.current !== queue || queue.closed || !acceptsInput()) return;
      // Delivery may be uncertain: discard the queued suffix and never replay
      // it. A later explicit keystroke starts a fresh queue for this terminal.
      queue.closed = true;
      authInputQueueRef.current = undefined;
      reportRequestError(error, requestId, "auth/terminal_input");
    });
  }, [reportRequestError]);

  const resizeAuthTerminal = useCallback((requestId: string, cols: number, rows: number) => {
    void client().command(terminalPath(requestId, "resize"), { body: { cols, rows } }).catch(reportError);
  }, [reportError]);

  const cancelAuthTerminal = useCallback((requestId: string) => {
    closeAuthInput(requestId);
    void client().command(terminalPath(requestId, "cancel"), { body: {} }).catch(reportError);
  }, [closeAuthInput, reportError]);

  const dismissAuthTerminal = useCallback(() => dispatch({ type: "auth/dismiss_terminal" }), []);

  const logout = useCallback(() => {
    if (store.getState().pendingAuth != null) return;
    const key = randomId();
    dispatch({ type: "auth/start", requestId: key, kind: "logout" });
    void client().command("/api/v2/agent/logout", { key, body: {} })
      .catch((error: unknown) => reportRequestError(error, key, "auth/logout"));
  }, [reportRequestError]);

  /** Moves to the session a create or fork returned, once it finished. */
  const followCreated = useCallback((record: Request | undefined, key: string, operation: string, navigation: number, cwd: string) => {
    if (record?.status !== "succeeded" || record.result?.kind !== "session") {
      if (navigationRef.current === navigation) reportRecordError(record, key, operation);
      return;
    }
    catalogInvalidationRef.current += 1;
    void refreshSessionList().catch(reportError);
    if (navigationRef.current !== navigation) return;
    const { sessionId, incarnation } = record.result;
    const epoch = client().replica.epoch;
    if (epoch == null) return;
    navigateSession(sessionId, client().global()?.sessions[sessionId]?.cwd ?? cwd);
    activeSessionIdRef.current = sessionId;
    pendingTurnRef.current = undefined;
    dispatch({ type: "session/view_synced", view: undefined });
    subscribe({ sessionId, epoch, incarnation });
  }, [navigateSession, refreshSessionList, reportError, reportRecordError, subscribe]);

  const newSession = useCallback((cwd: string): boolean => {
    if (store.getState().phase !== "ready") return false;
    const navigation = ++navigationRef.current;
    const key = randomId();
    dispatch({ type: "session/transition_start", kind: "new", requestId: key, cwd });
    void client().operation("/api/v2/sessions", { key, body: { cwd } }).then(
      ({ record }) => followCreated(record, key, "session/new", navigation, cwd),
      (error: unknown) => {
        if (navigationRef.current === navigation) reportRequestError(error, key, "session/new");
      },
    );
    return true;
  }, [followCreated, reportRequestError]);

  const listSessions = useCallback((cursor?: string) => {
    void refreshSessionList(cursor).catch(reportError);
  }, [refreshSessionList, reportError]);

  const attachSession = useCallback((session: SessionInfo) => activateSession(session), [activateSession]);

  const closeSession = useCallback(() => {
    const owner = subscribedRef.current;
    const sessionId = activeSessionIdRef.current;
    if (owner == null || sessionId !== owner.sessionId || store.getState().sessionTransition ||
      store.getState().pendingSessionControl || store.getState().runtimeOperation) return;
    const key = randomId();
    const navigation = navigationRef.current;
    dispatch({ type: "session/transition_start", kind: "close", requestId: key, sessionId });
    void client().operation(`/api/v2/sessions/${encodeURIComponent(sessionId)}/close`, {
      key, ifMatch: ownerTag(owner), body: {},
    }).then(({ record }) => {
      if (record?.status !== "succeeded") {
        reportRecordError(record, key, "session/close");
        return;
      }
      dispatch({ type: "session/management_complete", requestId: key, sessionId, owner: sessionOwner(owner), deleted: false });
      if (navigationRef.current === navigation && sameSessionOwner(store.getState().sessionView, sessionOwner(owner))) {
        returnToSessionProject(sessionId);
      }
      void refreshSessionList().catch(reportError);
    }, (error: unknown) => reportRequestError(error, key, "session/close"));
  }, [refreshSessionList, reportError, reportRecordError, reportRequestError, returnToSessionProject]);

  const forkSession = useCallback(() => {
    const owner = subscribedRef.current;
    if (owner == null || store.getState().running) return;
    const navigation = ++navigationRef.current;
    const key = randomId();
    const cwd = store.getState().cwd;
    dispatch({ type: "session/transition_start", kind: "fork", requestId: key, sessionId: owner.sessionId });
    void client().operation(`/api/v2/sessions/${encodeURIComponent(owner.sessionId)}/fork`, {
      key, ifMatch: ownerTag(owner), body: {},
    }).then(
      ({ record }) => followCreated(record, key, "session/fork", navigation, cwd),
      (error: unknown) => {
        if (navigationRef.current === navigation) reportRequestError(error, key, "session/fork");
      },
    );
  }, [followCreated, reportRequestError]);

  const deleteSession = useCallback((sessionId: string) => {
    if (store.getState().pendingSessionDeletions.some((pending) => pending.sessionId === sessionId)) return;
    const epoch = client().replica.epoch;
    if (epoch == null) return;
    const key = randomId();
    const navigation = navigationRef.current;
    const held = client().global()?.sessions[sessionId];
    // A cold session is deleted without loading it (protocol §5.3).
    const owner: Owner | undefined = held == null ? undefined : { sessionId, epoch, incarnation: held.incarnation };
    dispatch({ type: "session/delete_start", requestId: key, sessionId });
    void client().operation(`/api/v2/sessions/${encodeURIComponent(sessionId)}`, {
      method: "DELETE", key, ifMatch: owner == null ? `${epoch}:0` : ownerTag(owner),
    }).then(({ record }) => {
      if (record?.status !== "succeeded") {
        reportRecordError(record, key, "session/delete");
        return;
      }
      catalogInvalidationRef.current += 1;
      dispatch({
        type: "session/management_complete", requestId: key, sessionId,
        owner: owner == null ? undefined : sessionOwner(owner), deleted: true,
      });
      if (owner != null && navigationRef.current === navigation &&
        sameSessionOwner(store.getState().sessionView, sessionOwner(owner))) {
        returnToSessionProject(sessionId);
      }
      void refreshSessionList().catch(reportError);
    }, (error: unknown) => reportRequestError(error, key, "session/delete"));
  }, [refreshSessionList, reportError, reportRecordError, reportRequestError, returnToSessionProject]);

  /** Acknowledges a finished URL flow, or hides a waiting one locally. */
  const dismissExternalFlow = useCallback((flowId: string) => {
    dispatch({ type: "elicitation/dismiss_flow", elicitationId: flowId });
    const global = client().global()?.urlFlows[flowId];
    const owner = subscribedRef.current;
    const local = owner == null ? undefined : client().session(owner.sessionId)?.urlFlows[flowId];
    if (global != null && global.status !== "waiting") {
      void client().command(`/api/v2/url-flows/${encodeURIComponent(flowId)}`, { method: "DELETE" }).catch(reportError);
    } else if (owner != null && local != null && local.status !== "waiting") {
      void client().command(
        `/api/v2/sessions/${encodeURIComponent(owner.sessionId)}/url-flows/${encodeURIComponent(flowId)}`,
        { method: "DELETE", ifMatch: ownerTag(owner) },
      ).catch(reportError);
    }
  }, [reportError]);

  // ---------------------------------------------------------------------------
  // Process pages (protocol §6)

  const loadTurnProcess = useCallback(async (
    process: DeferredTurnProcess, offset: number, signal: AbortSignal,
  ): Promise<BridgeTurnProcessPage> => {
    const navigation = navigationRef.current;
    const isCurrent = () => navigationRef.current === navigation &&
      activeSessionIdRef.current === process.owner.sessionId &&
      sameSessionOwner(store.getState().sessionView, process.owner) &&
      store.getState().sessionView?.historyRevision === process.historyRevision;
    if (signal.aborted || !isCurrent()) throw new DOMException("Session history changed", "AbortError");
    const page = await requestJson<BridgeTurnProcessPage>(
      `/api/v2/sessions/${encodeURIComponent(process.owner.sessionId)}/history/${encodeURIComponent(process.historyRevision)}` +
        `/turns/${encodeURIComponent(process.turnId)}/process?${new URLSearchParams({ offset: String(offset) })}`,
      { signal },
    );
    if (signal.aborted || !isCurrent()) throw new DOMException("Session history changed", "AbortError");
    if (!validProcessPage(page, process, offset)) throw new Error("Bridge returned an invalid process page");
    return page;
  }, []);

  const releaseTurnProcess = useCallback((process: DeferredTurnProcess) => {
    const view = store.getState().sessionView;
    if (view == null || activeSessionIdRef.current !== process.owner.sessionId || !sameSessionOwner(view, process.owner) ||
      !store.getState().timeline.some((item) => sameTurnProcess(item.retainedProcess, process))) return;
    // The watched turn's cached process is dropped; the page stays fetchable.
    processesRef.current.release(process.historyRevision, process.turnId);
    dispatch({ type: "session/release_turn_process", process });
  }, []);

  const readThreadForExport = useCallback(async () => {
    if (!store.getState().timeline.some((item) => item.deferredProcess != null) &&
      !containsHostedAttachment(store.getState().timeline)) return store.getState();
    const currentView = store.getState().sessionView;
    if (currentView == null) return undefined;
    const owner = sessionIdentity(currentView);
    const navigation = navigationRef.current;
    try {
      // Export reads one complete, attachment-inclusive view of the same owner.
      // TODO(#18): move to a v2 export read before v1 is removed.
      const query = new URLSearchParams({
        presentation: "compact",
        expectedEpoch: owner.bridgeEpoch,
        expectedIncarnation: String(owner.sessionIncarnation),
        includeAttachmentContent: "true",
      });
      const view = await requestJson<BridgeSessionView>(`/api/v1/sessions/${encodeURIComponent(owner.sessionId)}?${query}`);
      if (navigationRef.current !== navigation || !sameSessionOwner(view, owner) ||
        !sameSessionOwner(store.getState().sessionView, owner)) return undefined;
      return appReducer(store.getState(), { type: "bridge/session_hydrate", view });
    } catch (error) {
      if (navigationRef.current === navigation && sameSessionOwner(store.getState().sessionView, owner)) reportError(error);
      return undefined;
    }
  }, [reportError]);

  return {
    state,
    sessionOpening,
    retrySessionOpening,
    reconnect,
    authenticate,
    writeAuthTerminal,
    resizeAuthTerminal,
    cancelAuthTerminal,
    dismissAuthTerminal,
    logout,
    prompt,
    cancel,
    setMode,
    setConfig,
    respondPermission,
    respondElicitation,
    dismissExternalFlow,
    newSession,
    listSessions,
    attachSession,
    projectCwd,
    openProject,
    goHome: returnHome,
    forkSession,
    closeSession,
    deleteSession,
    searchWorkspaceContext,
    readWorkspaceContext,
    loadTurnProcess,
    releaseTurnProcess,
    readThreadForExport,
  };
}

function sessionIdentity(owner: SessionOwner): SessionOwner {
  return { bridgeEpoch: owner.bridgeEpoch, sessionId: owner.sessionId, sessionIncarnation: owner.sessionIncarnation };
}

/** The reducer's error for a v2 error response, if it is one. */
function protocolFailure(error: unknown): { message: string; code?: number; data?: unknown } | undefined {
  if (!(error instanceof ApiError) || !isRecord(error.body) || !isRecord(error.body.error)) return undefined;
  const failure = error.body.error;
  const message = typeof failure.message === "string" ? failure.message : error.message;
  if (failure.code === "agent_error" && isRecord(failure.data)) {
    return {
      message: typeof failure.data.message === "string" ? failure.data.message : message,
      ...(typeof failure.data.code === "number" ? { code: failure.data.code } : {}),
      ...(failure.data.data === undefined ? {} : { data: failure.data.data }),
    };
  }
  return { message, ...(failure.data === undefined ? {} : { data: failure.data }) };
}

function validProcessPage(page: BridgeTurnProcessPage, process: DeferredTurnProcess, offset: number): boolean {
  return isRecord(page) && sameSessionOwner(page, process.owner) && page.turnId === process.turnId &&
    page.historyRevision === process.historyRevision && page.offset === offset &&
    page.total === process.processCount && Array.isArray(page.items) &&
    page.items.length === Math.min(10, process.processCount - offset) &&
    page.items.every((item) => Array.isArray(item) && item.length > 0 &&
      item.every((update) => isRecord(update) && typeof update.sessionUpdate === "string")) &&
    isRecord(page.terminals) && Object.entries(page.terminals).every(([id, terminal]) =>
      isRecord(terminal) && terminal.terminalId === id && terminal.sessionId === process.owner.sessionId &&
      typeof terminal.output === "string" && typeof terminal.truncated === "boolean" &&
      typeof terminal.released === "boolean") &&
    (page.response == null || (isRecord(page.response) && typeof page.response.stopReason === "string")) &&
    (page.nextOffset === null || page.nextOffset === offset + page.items.length) &&
    offset + page.items.length <= page.total &&
    (page.nextOffset === null) === (offset + page.items.length === page.total);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function setRoute(path: string, replace = false): void {
  if (window.location.pathname === path && !window.location.search && !window.location.hash) return;
  if (replace) window.history.replaceState(null, "", path);
  else window.history.pushState(null, "", path);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
