import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import type {
  ContentBlock,
  CreateElicitationResponse,
  RequestPermissionResponse,
  SessionInfo,
} from "@agentclientprotocol/sdk";
import type { ServerEvent, TerminalSnapshot } from "../../../shared/bridge";
import {
  type BridgeSessionView,
  type CreatedSessionResult,
  type GlobalBusinessEvent,
  type RuntimeView,
  type SessionBusinessEvent,
  type SessionListResult,
  type StartTurnResult,
  type WorkspaceContextAttachment,
  type WorkspaceContextMatch,
  ApiError,
  parseGlobalBusinessEvent,
  parseSessionBusinessEvent,
  requestJson,
  strongEtag,
  workspaceContextSearchPath,
} from "./business-api";
import { randomId } from "./id";
import { projectPath, readProjectCwdFromPath, readSessionIdFromPath, sessionPath } from "./session-route";
import { appReducer, initialState } from "./state";

export function useAcp() {
  const [state, dispatch] = useReducer(appReducer, initialState);
  const [projectCwd, setProjectCwd] = useState(() => readProjectCwdFromPath(window.location.pathname));
  const stateRef = useRef(state);
  stateRef.current = state;

  const globalEventsRef = useRef<EventSource | undefined>(undefined);
  const sessionEventsRef = useRef<EventSource | undefined>(undefined);
  const activeSessionIdRef = useRef<string | undefined>(undefined);
  const sessionViewRef = useRef<BridgeSessionView | undefined>(undefined);
  const promptAdmissionsRef = useRef(new Map<string, {
    requestId: string;
    baseRevision: string;
  }>());
  const refreshInFlightRef = useRef<{ sessionId: string; pending: boolean } | undefined>(undefined);
  const runtimeRefreshRef = useRef<{ pending: boolean } | undefined>(undefined);
  const sessionListQueueRef = useRef<Promise<unknown>>(Promise.resolve());
  const sessionListSupportedRef = useRef(false);
  const sessionLoadSupportedRef = useRef(false);
  const navigationRef = useRef(0);
  const reconnectRef = useRef<() => void>(() => undefined);
  const refreshSessionRef = useRef<(sessionId: string) => void>(() => undefined);

  const resetSession = useCallback((preserve = false) => {
    sessionEventsRef.current?.close();
    sessionEventsRef.current = undefined;
    activeSessionIdRef.current = undefined;
    sessionViewRef.current = undefined;
    refreshInFlightRef.current = undefined;
    dispatch({ type: preserve ? "session/deselect" : "session/reset" });
  }, []);

  const returnHome = useCallback((replace = false, preserve = true) => {
    navigationRef.current += 1;
    setRoute("/", replace);
    setProjectCwd(undefined);
    resetSession(preserve);
  }, [resetSession]);

  const navigateSession = useCallback((sessionId: string, cwd?: string | null, replace = false) => {
    const validCwd = cwd != null && projectPath(cwd) !== "/" ? cwd : undefined;
    setRoute(sessionPath(sessionId, validCwd), replace);
    setProjectCwd(validCwd);
  }, []);

  const reportError = useCallback((error: unknown) => {
    if (error instanceof ApiError) {
      const body = error.body;
      if (isRecord(body) && typeof body.code === "number") {
        dispatch({
          type: "server/event",
          event: {
            type: "bridge/error",
            message: typeof body.message === "string" ? body.message : error.message,
            code: body.code,
            data: body.data,
          },
        });
        return;
      }
    }
    dispatch({
      type: "client/error",
      message: error instanceof Error ? error.message : String(error),
    });
  }, []);

  const reportRequestError = useCallback((
    error: unknown,
    requestId: string,
    operation: Extract<ServerEvent, { type: "bridge/error" }>["operation"],
  ) => {
    dispatch({
      type: "server/event",
      event: {
        type: "bridge/error",
        requestId,
        operation,
        message: error instanceof Error ? error.message : String(error),
      },
    });
  }, []);

  const hydrateSession = useCallback((view: BridgeSessionView) => {
    if (activeSessionIdRef.current !== view.sessionId) return;
    const current = sessionViewRef.current;
    const admission = promptAdmissionsRef.current.get(view.sessionId);
    if (admission != null) {
      const reflectsAdmission =
        view.activeTurn?.clientIntentId === admission.requestId ||
        (view.phase === "ready" && view.historyRevision !== admission.baseRevision);
      if (!reflectsAdmission && view.phase === "ready") return;
      if (reflectsAdmission) promptAdmissionsRef.current.delete(view.sessionId);
    }
    if (
      current != null &&
      current.sessionId === view.sessionId &&
      current.bridgeEpoch === view.bridgeEpoch &&
      current.sessionIncarnation === view.sessionIncarnation &&
      view.viewRevision < current.viewRevision
    ) return;
    const cwd = view.workspace.cwd;
    if (cwd != null) navigateSession(view.sessionId, cwd, true);
    sessionViewRef.current = view;
    dispatch({ type: "bridge/session_hydrate", view });
  }, [navigateSession]);

  const refreshSession = useCallback((sessionId: string) => {
    if (activeSessionIdRef.current !== sessionId) return;
    if (refreshInFlightRef.current?.sessionId === sessionId) {
      refreshInFlightRef.current.pending = true;
      return;
    }
    const refresh = { sessionId, pending: false };
    refreshInFlightRef.current = refresh;
    void (async () => {
      try {
        do {
          refresh.pending = false;
          const cwd = sessionViewRef.current?.workspace.cwd ?? readProjectCwdFromPath(window.location.pathname);
          const query = sessionCwdQuery(cwd, sessionLoadSupportedRef.current);
          const view = await requestJson<BridgeSessionView>(
            `/api/v1/sessions/${encodeURIComponent(sessionId)}${query}`,
          );
          if (refreshInFlightRef.current !== refresh || activeSessionIdRef.current !== sessionId) return;
          hydrateSession(view);
        } while (refresh.pending && activeSessionIdRef.current === sessionId);
      } catch (error) {
        if (refreshInFlightRef.current !== refresh || activeSessionIdRef.current !== sessionId) return;
        if (isMissingSession(error)) returnHome(true, false);
        else reportError(error);
      } finally {
        if (refreshInFlightRef.current === refresh) {
          refreshInFlightRef.current = undefined;
          if (refresh.pending && activeSessionIdRef.current === sessionId) {
            refreshSessionRef.current(sessionId);
          }
        }
      }
    })();
  }, [hydrateSession, reportError, returnHome]);
  refreshSessionRef.current = refreshSession;

  const connectSessionEvents = useCallback((sessionId: string) => {
    sessionEventsRef.current?.close();
    const cwd = sessionViewRef.current?.workspace.cwd ?? readProjectCwdFromPath(window.location.pathname);
    const query = sessionCwdQuery(cwd, sessionLoadSupportedRef.current);
    const source = new EventSource(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/events${query}`,
    );
    sessionEventsRef.current = source;
    source.onmessage = ({ data }) => {
      let event: SessionBusinessEvent;
      try {
        event = parseSessionBusinessEvent(String(data));
      } catch (error) {
        reportError(error);
        return;
      }
      if (activeSessionIdRef.current !== event.sessionId) return;
      if (event.type === "bridge/session_turn_complete") {
        clearMatchingPromptAdmission(promptAdmissionsRef.current, event);
        if (!advanceSessionViewToTurnOutcome(sessionViewRef, event)) {
          refreshSessionRef.current(sessionId);
          return;
        }
        dispatch({ type: "bridge/turn_complete", event });
        return;
      }
      if (event.type === "bridge/session_turn_failed") {
        clearMatchingPromptAdmission(promptAdmissionsRef.current, event);
        if (!advanceSessionViewToTurnOutcome(sessionViewRef, event)) {
          refreshSessionRef.current(sessionId);
          return;
        }
        dispatch({ type: "bridge/turn_failed", event });
        return;
      }
      const current = sessionViewRef.current;
      if (event.type === "bridge/session_reset") {
        if (
          current == null ||
          current.bridgeEpoch !== event.bridgeEpoch ||
          current.sessionIncarnation !== event.sessionIncarnation ||
          current.viewRevision !== event.viewRevision
        ) {
          refreshSessionRef.current(sessionId);
        }
        return;
      }
      if (
        current == null ||
        current.bridgeEpoch !== event.bridgeEpoch ||
        current.sessionIncarnation !== event.sessionIncarnation ||
        current.viewRevision !== event.fromRevision
      ) {
        refreshSessionRef.current(sessionId);
        return;
      }
      if (event.change.kind === "terminal_update") {
        const incoming = event.change.terminal;
        const previous = Object.hasOwn(current.terminals, incoming.terminalId)
          ? current.terminals[incoming.terminalId]
          : undefined;
        if (incoming.outputAppend && previous == null) {
          refreshSessionRef.current(sessionId);
          return;
        }
        const terminal = mergeTerminalSnapshot(previous, incoming);
        sessionViewRef.current = {
          ...current,
          viewRevision: event.viewRevision,
          terminals: { ...current.terminals, [terminal.terminalId]: terminal },
        };
        dispatch({ type: "server/event", event: { type: "acp/terminal_state", terminal } });
        return;
      }
      sessionViewRef.current = { ...current, viewRevision: event.viewRevision };
      if (event.change.kind === "turn_update" && event.change.update != null) {
        dispatch({
          type: "server/event",
          event: {
            type: "acp/session_update",
            notification: { sessionId, update: event.change.update },
          },
        });
      } else {
        refreshSessionRef.current(sessionId);
      }
    };
    source.onerror = () => {
      // Native EventSource retry plus the server reset token repairs missed deltas.
    };
  }, [reportError]);

  const activateSession = useCallback((session: SessionInfo, transition = true, view?: BridgeSessionView) => {
    navigationRef.current += 1;
    navigateSession(session.sessionId, view?.workspace.cwd ?? session.cwd, !transition);
    if (activeSessionIdRef.current === session.sessionId && sessionViewRef.current != null) {
      refreshSessionRef.current(session.sessionId);
      return;
    }
    const requestId = randomId();
    activeSessionIdRef.current = session.sessionId;
    sessionViewRef.current = undefined;
    sessionEventsRef.current?.close();
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
    connectSessionEvents(session.sessionId);
    if (view != null) hydrateSession(view);
    else refreshSessionRef.current(session.sessionId);
  }, [connectSessionEvents, hydrateSession, navigateSession]);

  const readSessionList = useCallback(async (cursor?: string) => {
    if (!sessionListSupportedRef.current) return { sessions: [] } as SessionListResult;
    const suffix = cursor == null ? "" : `?${new URLSearchParams({ cursor })}`;
    const response = await requestJson<SessionListResult>(`/api/v1/sessions${suffix}`);
    dispatch({
      type: "server/event",
      event: {
        type: "acp/sessions_listed",
        requestId: randomId(),
        cursor,
        response,
      },
    });
    return response;
  }, []);

  const withSessionList = useCallback(<T,>(operation: () => Promise<T>): Promise<T> => {
    const result = sessionListQueueRef.current.then(operation, operation);
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
      if (cursors.has(listed.nextCursor)) throw new Error("Agent session list repeated a cursor");
      cursors.add(listed.nextCursor);
      listed = await readSessionList(listed.nextCursor);
    }
    return listed;
  }), [readSessionList, withSessionList]);

  const refreshRuntimeOnce = useCallback(async () => {
    const navigation = navigationRef.current;
    const routeProjectCwd = readProjectCwdFromPath(window.location.pathname);
    const routeSessionId = readSessionIdFromPath(window.location.pathname);
    const stillCurrent = () => navigationRef.current === navigation &&
      readProjectCwdFromPath(window.location.pathname) === routeProjectCwd &&
      readSessionIdFromPath(window.location.pathname) === routeSessionId;
    try {
      const runtime = await requestJson<RuntimeView>("/api/v1/runtime");
      if (!stillCurrent()) return;
      dispatch({ type: "socket/open" });
      if (runtime.hello != null) dispatch({ type: "server/event", event: runtime.hello });
      if (runtime.initialized != null) {
        dispatch({ type: "server/event", event: runtime.initialized });
      }
      sessionListSupportedRef.current = runtime.initialized?.response.agentCapabilities?.sessionCapabilities?.list != null;
      sessionLoadSupportedRef.current = runtime.initialized?.response.agentCapabilities?.loadSession === true;
      if (runtime.error != null) dispatch({ type: "server/event", event: runtime.error });
      if (runtime.phase != null) dispatch({ type: "server/event", event: runtime.phase });
      else if (!runtime.connected) dispatch({ type: "socket/closed" });

      if (!runtime.connected || runtime.phase?.phase !== "ready") return;

      await withSessionList(async () => {
        if (!stillCurrent()) return;
        let listed = await readSessionList();
        if (!stillCurrent() || (routeSessionId == null && routeProjectCwd == null)) return;

        let selected = listed.sessions.find(({ sessionId }) => sessionId === routeSessionId);
        // Complete this page's discovery before choosing its session workspace.
        const cursors = new Set<string>();
        while (listed.nextCursor != null) {
          if (cursors.has(listed.nextCursor)) throw new Error("Agent session list repeated a cursor");
          cursors.add(listed.nextCursor);
          listed = await readSessionList(listed.nextCursor);
          if (!stillCurrent()) return;
          selected ??= listed.sessions.find(({ sessionId }) => sessionId === routeSessionId);
        }
        // Projects show their complete session metadata without materializing a
        // conversation. Only an explicit session URL opens a session stream.
        if (routeSessionId == null) return;
        if (activeSessionIdRef.current === routeSessionId) {
          connectSessionEvents(routeSessionId);
          refreshSessionRef.current(routeSessionId);
          return;
        }
        // Materialized sessions remain readable even if they are not listed.
        // Only the bridge's explicit not-found response sends this route home.
        const cwdQuery = sessionCwdQuery(selected?.cwd ?? routeProjectCwd,
          runtime.initialized?.response.agentCapabilities?.loadSession);
        const view = await requestJson<BridgeSessionView>(
          `/api/v1/sessions/${encodeURIComponent(routeSessionId)}${cwdQuery}`,
        );
        if (stillCurrent()) {
          activateSession(selected ?? { sessionId: routeSessionId, cwd: "" }, false, view);
        }
      });
    } catch (error) {
      if (!stillCurrent()) return;
      if (isMissingSession(error)) {
        returnHome(true, false);
        return;
      }
      // Business errors leave the route intact; only a transport failure makes
      // the connection stale. Authentication and load failures remain visible.
      if (!(error instanceof ApiError) || error.status >= 500) {
        dispatch({ type: "socket/closed" });
      }
      reportError(error);
    }
  }, [activateSession, connectSessionEvents, readSessionList, reportError, returnHome, withSessionList]);

  const refreshRuntime = useCallback(() => {
    if (runtimeRefreshRef.current != null) {
      runtimeRefreshRef.current.pending = true;
      return;
    }
    const refresh = { pending: false };
    runtimeRefreshRef.current = refresh;
    void (async () => {
      try {
        do {
          refresh.pending = false;
          await refreshRuntimeOnce();
        } while (refresh.pending && runtimeRefreshRef.current === refresh);
      } finally {
        if (runtimeRefreshRef.current === refresh) runtimeRefreshRef.current = undefined;
      }
    })();
  }, [refreshRuntimeOnce]);

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
    void refreshRuntime();
  }, [refreshRuntime, resetSession, returnHome]);

  const handleGlobalEvent = useCallback((event: GlobalBusinessEvent) => {
    switch (event.type) {
      case "bridge/connection":
        if (event.phase === "stopped" || event.phase === "error") {
          promptAdmissionsRef.current.clear();
        }
        dispatch({
          type: "server/event",
          event: { type: "bridge/phase", phase: event.phase },
        });
        if (event.phase === "ready") {
          if (stateRef.current.authTerminal?.status === "succeeded") {
            dispatch({ type: "auth/dismiss_terminal" });
          }
          void refreshRuntime();
        }
        return;
      case "bridge/connection_error":
        dispatch({
          type: "server/event",
          event: {
            type: "bridge/error",
            message: event.message,
            code: event.code,
            data: event.data,
          },
        });
        void refreshRuntime();
        return;
      default:
        dispatch({ type: "server/event", event: event as ServerEvent });
    }
  }, [refreshRuntime]);

  const connectGlobalEvents = useCallback(() => {
    globalEventsRef.current?.close();
    const source = new EventSource("/api/v1/events");
    globalEventsRef.current = source;
    source.onopen = () => {
      dispatch({ type: "socket/open" });
      void refreshRuntime();
    };
    source.onmessage = ({ data }) => {
      try {
        handleGlobalEvent(parseGlobalBusinessEvent(String(data)));
      } catch (error) {
        reportError(error);
      }
    };
    source.onerror = () => dispatch({ type: "socket/closed" });
  }, [handleGlobalEvent, refreshRuntime, reportError]);

  const reconnect = useCallback(() => reconnectRef.current(), []);

  useEffect(() => {
    let disposed = false;
    const reconnectStreams = () => {
      if (disposed) return;
      connectGlobalEvents();
    };
    reconnectRef.current = reconnectStreams;
    connectGlobalEvents();
    const restoreRoute = () => {
      navigationRef.current += 1;
      const cwd = readProjectCwdFromPath(window.location.pathname);
      const sessionId = readSessionIdFromPath(window.location.pathname);
      if (cwd == null && sessionId == null) returnHome(true);
      else {
        setProjectCwd(cwd);
        setRoute(sessionId == null ? projectPath(cwd!) : sessionPath(sessionId, cwd), true);
        if (sessionId == null || activeSessionIdRef.current !== sessionId) resetSession(true);
      }
      void refreshRuntime();
    };
    restoreRoute();

    const recoverClosedStreams = () => {
      if (
        stateRef.current.phase === "stopped" || stateRef.current.phase === "error" ||
        globalEventsRef.current?.readyState === EventSource.CLOSED ||
        (activeSessionIdRef.current != null &&
          sessionEventsRef.current?.readyState === EventSource.CLOSED)
      ) reconnectStreams();
    };
    window.addEventListener("online", recoverClosedStreams);
    window.addEventListener("focus", recoverClosedStreams);
    window.addEventListener("pageshow", recoverClosedStreams);
    window.addEventListener("popstate", restoreRoute);
    document.addEventListener("visibilitychange", recoverClosedStreams);
    return () => {
      disposed = true;
      navigationRef.current += 1;
      reconnectRef.current = () => undefined;
      window.removeEventListener("online", recoverClosedStreams);
      window.removeEventListener("focus", recoverClosedStreams);
      window.removeEventListener("pageshow", recoverClosedStreams);
      window.removeEventListener("popstate", restoreRoute);
      document.removeEventListener("visibilitychange", recoverClosedStreams);
      globalEventsRef.current?.close();
      sessionEventsRef.current?.close();
      refreshInFlightRef.current = undefined;
    };
  }, [connectGlobalEvents, refreshRuntime, resetSession, returnHome]);

  const searchWorkspaceContext = useCallback(async (query: string) => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null) {
      throw new Error("Wait for an active ACP session before searching workspace context");
    }
    const response = await requestJson<{ matches: WorkspaceContextMatch[] }>(
      workspaceContextSearchPath(query, sessionId),
    );
    return response.matches;
  }, []);

  const readWorkspaceContext = useCallback(async (path: string) => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null) {
      throw new Error("Wait for an active ACP session before adding workspace context");
    }
    const response = await requestJson<{ attachment: WorkspaceContextAttachment }>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/context/read`,
      { method: "POST", body: JSON.stringify({ path }) },
    );
    return response.attachment;
  }, []);

  const prompt = useCallback((blocks: ContentBlock[]) => {
    const current = stateRef.current;
    const view = sessionViewRef.current;
    if (
      current.session == null || current.running || current.pendingPrompt != null ||
      current.sessionTransition != null || current.pendingSessionControl != null ||
      current.runtimeOperation != null || view == null || view.phase !== "ready" ||
      view.historyRevision == null
    ) return false;
    const requestId = randomId();
    const sessionId = current.session.sessionId;
    promptAdmissionsRef.current.set(sessionId, {
      requestId,
      baseRevision: view.historyRevision,
    });
    dispatch({ type: "user/prompt", requestId, sessionId, blocks });
    void (async () => {
      // A reset token can settle the visible turn before its authoritative
      // session GET completes. Resolve the append point immediately before
      // admission so a queued prompt never reuses the preceding revision.
      const latest = await requestJson<BridgeSessionView>(
        `/api/v1/sessions/${encodeURIComponent(sessionId)}`,
      );
      if (
        latest.sessionId !== sessionId || latest.phase !== "ready" ||
        latest.historyRevision == null
      ) {
        throw new Error("The ACP session is not ready to accept this prompt");
      }
      const admission = promptAdmissionsRef.current.get(sessionId);
      if (admission?.requestId === requestId) {
        promptAdmissionsRef.current.set(sessionId, {
          ...admission,
          baseRevision: latest.historyRevision,
        });
      }
      if (activeSessionIdRef.current === sessionId) sessionViewRef.current = latest;
      return requestJson<StartTurnResult>(
        `/api/v1/sessions/${encodeURIComponent(sessionId)}/turns`,
        {
          method: "POST",
          headers: {
            "If-Match": strongEtag(latest.historyRevision),
            "Idempotency-Key": requestId,
          },
          body: JSON.stringify({ prompt: blocks }),
        },
      );
    })().then(() => refreshSessionRef.current(sessionId)).catch((error) => {
      const admission = promptAdmissionsRef.current.get(sessionId);
      if (admission?.requestId === requestId) {
        promptAdmissionsRef.current.delete(sessionId);
      }
      reportRequestError(error, requestId, "session/prompt");
      refreshSessionRef.current(sessionId);
    });
    return true;
  }, [reportRequestError]);

  const cancel = useCallback(() => {
    const view = sessionViewRef.current;
    if (view?.activeTurn == null || view.phase !== "running") return;
    void requestJson(
      `/api/v1/sessions/${encodeURIComponent(view.sessionId)}/turns/${encodeURIComponent(view.activeTurn.operationId)}/cancel`,
      { method: "POST" },
    ).then(() => refreshSessionRef.current(view.sessionId)).catch(reportError);
  }, [reportError]);

  const setMode = useCallback((modeId: string) => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null || !["ready", "running", "reconciling"].includes(sessionViewRef.current?.phase ?? "")) return;
    const requestId = randomId();
    dispatch({ type: "session/control_start", kind: "mode", requestId, sessionId });
    void requestJson(`/api/v1/sessions/${encodeURIComponent(sessionId)}/mode`, {
      method: "POST",
      body: JSON.stringify({ modeId }),
    }).then(() => refreshSessionRef.current(sessionId)).catch((error) => {
      reportRequestError(error, requestId, "session/set_mode");
    });
  }, [reportRequestError]);

  const setConfig = useCallback((configId: string, value: string | boolean) => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null || !["ready", "running", "reconciling"].includes(sessionViewRef.current?.phase ?? "")) return;
    const requestId = randomId();
    dispatch({ type: "session/control_start", kind: "config", requestId, sessionId });
    void requestJson(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/configuration/${encodeURIComponent(configId)}`,
      { method: "POST", body: JSON.stringify({ value }) },
    ).then(() => refreshSessionRef.current(sessionId)).catch((error) => {
      reportRequestError(error, requestId, "session/set_config_option");
    });
  }, [reportRequestError]);

  const respondPermission = useCallback((
    permissionId: string,
    outcome: RequestPermissionResponse["outcome"],
  ) => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null) return;
    const requestId = randomId();
    dispatch({ type: "permission/respond_start", permissionId, requestId });
    void requestJson(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/interactions/${encodeURIComponent(permissionId)}/response`,
      { method: "POST", body: JSON.stringify({ kind: "permission", outcome }) },
    ).then(() => refreshSessionRef.current(sessionId)).catch((error) => {
      reportRequestError(error, requestId, "permission/respond");
    });
  }, [reportRequestError]);

  const respondElicitation = useCallback((
    elicitationId: string,
    response: CreateElicitationResponse,
  ) => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null) return;
    const requestId = randomId();
    dispatch({ type: "elicitation/respond_start", elicitationId, requestId });
    void requestJson(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/interactions/${encodeURIComponent(elicitationId)}/response`,
      { method: "POST", body: JSON.stringify({ kind: "elicitation", response }) },
    ).then(() => refreshSessionRef.current(sessionId)).catch((error) => {
      reportRequestError(error, requestId, "elicitation/respond");
    });
  }, [reportRequestError]);

  const authenticate = useCallback((methodId: string) => {
    const method = stateRef.current.initialized?.authMethods?.find(({ id }) => id === methodId);
    const terminal = method != null && "type" in method && method.type === "terminal";
    if (terminal) {
      void requestJson("/api/v1/auth/terminal", {
        method: "POST",
        body: JSON.stringify({ methodId, cols: 80, rows: 24 }),
      }).catch(reportError);
      return;
    }
    void requestJson<{ requestId: string; response: unknown }>(
      `/api/v1/auth/${encodeURIComponent(methodId)}`,
      { method: "POST" },
    ).then(({ requestId, response }) => {
      handleGlobalEvent({ type: "acp/authenticated", requestId, methodId, response });
      void refreshRuntime();
    }).catch(reportError);
  }, [handleGlobalEvent, refreshRuntime, reportError]);

  const writeAuthTerminal = useCallback((requestId: string, data: string) => {
    void requestJson(`/api/v1/auth/terminal/${encodeURIComponent(requestId)}/input`, {
      method: "POST",
      body: JSON.stringify({ data }),
    }).catch(reportError);
  }, [reportError]);

  const resizeAuthTerminal = useCallback((requestId: string, cols: number, rows: number) => {
    void requestJson(`/api/v1/auth/terminal/${encodeURIComponent(requestId)}/resize`, {
      method: "POST",
      body: JSON.stringify({ cols, rows }),
    }).catch(reportError);
  }, [reportError]);

  const cancelAuthTerminal = useCallback((requestId: string) => {
    void requestJson(`/api/v1/auth/terminal/${encodeURIComponent(requestId)}/cancel`, {
      method: "POST",
    }).catch(reportError);
  }, [reportError]);

  const dismissAuthTerminal = useCallback(() => dispatch({ type: "auth/dismiss_terminal" }), []);

  const logout = useCallback(() => {
    void requestJson<{ requestId: string; response: unknown }>("/api/v1/auth/logout", {
      method: "POST",
    }).then(({ requestId, response }) => {
      handleGlobalEvent({ type: "acp/logged_out", requestId, response });
    }).catch(reportError);
  }, [handleGlobalEvent, reportError]);

  const newSession = useCallback((cwd: string): boolean => {
    if (stateRef.current.phase !== "ready") return false;
    const navigation = ++navigationRef.current;
    const requestId = randomId();
    dispatch({ type: "session/transition_start", kind: "new", requestId, cwd });
    void requestJson<CreatedSessionResult>("/api/v1/sessions", {
      method: "POST",
      body: JSON.stringify({ cwd }),
    }).then((created) => {
      if (navigationRef.current !== navigation) {
        void refreshSessionList().catch(reportError);
        return;
      }
      navigateSession(created.sessionId, created.view.workspace.cwd ?? created.cwd ?? cwd);
      activeSessionIdRef.current = created.sessionId;
      sessionViewRef.current = undefined;
      sessionEventsRef.current?.close();
      connectSessionEvents(created.sessionId);
      hydrateSession(created.view);
      void refreshSessionList();
    }).catch((error) => {
      if (navigationRef.current === navigation) reportRequestError(error, requestId, "session/new");
    });
    return true;
  }, [connectSessionEvents, hydrateSession, navigateSession, refreshSessionList, reportError, reportRequestError]);

  const listSessions = useCallback((cursor?: string) => {
    void refreshSessionList(cursor).catch(reportError);
  }, [refreshSessionList, reportError]);

  const attachSession = useCallback((session: SessionInfo) => activateSession(session), [activateSession]);

  const closeSession = useCallback(() => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null || stateRef.current.sessionTransition || stateRef.current.pendingSessionControl || stateRef.current.runtimeOperation) return;
    const requestId = randomId();
    dispatch({ type: "session/transition_start", kind: "close", requestId, sessionId });
    void requestJson(`/api/v1/sessions/${encodeURIComponent(sessionId)}/close`, {
      method: "POST",
    }).then(() => {
      if (activeSessionIdRef.current === sessionId) returnHome(true, false);
      void refreshSessionList();
    }).catch((error) => reportRequestError(error, requestId, "session/close"));
  }, [refreshSessionList, reportRequestError, returnHome]);

  const forkSession = useCallback(() => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null || stateRef.current.running) return;
    const navigation = ++navigationRef.current;
    const requestId = randomId();
    dispatch({ type: "session/transition_start", kind: "fork", requestId, sessionId });
    void requestJson<CreatedSessionResult>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/fork`,
      { method: "POST" },
    ).then((forked) => {
      if (navigationRef.current !== navigation) {
        void refreshSessionList().catch(reportError);
        return;
      }
      navigateSession(forked.sessionId, forked.view.workspace.cwd ?? forked.cwd ?? stateRef.current.cwd);
      activeSessionIdRef.current = forked.sessionId;
      sessionViewRef.current = undefined;
      sessionEventsRef.current?.close();
      connectSessionEvents(forked.sessionId);
      hydrateSession(forked.view);
      void refreshSessionList();
    }).catch((error) => {
      if (navigationRef.current === navigation) reportRequestError(error, requestId, "session/fork");
    });
  }, [connectSessionEvents, hydrateSession, navigateSession, refreshSessionList, reportError, reportRequestError]);

  const deleteSession = useCallback((sessionId: string) => {
    if (stateRef.current.pendingSessionDeletions.some((pending) => pending.sessionId === sessionId)) {
      return;
    }
    const requestId = randomId();
    dispatch({ type: "session/delete_start", requestId, sessionId, stage: "deleting" });
    void requestJson(`/api/v1/sessions/${encodeURIComponent(sessionId)}`, {
      method: "DELETE",
    }).then(() => {
      if (activeSessionIdRef.current === sessionId) {
        returnHome(true, false);
      }
      dispatch({
        type: "server/event",
        event: { type: "acp/session_deleted", requestId, sessionId },
      });
      void refreshSessionList();
    }).catch((error) => reportRequestError(error, requestId, "session/delete"));
  }, [refreshSessionList, reportRequestError, returnHome]);

  const dismissExternalFlow = useCallback((elicitationId: string) => {
    dispatch({ type: "elicitation/dismiss_flow", elicitationId });
  }, []);

  return {
    state,
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
  };
}

function sessionCwdQuery(cwd: string | null | undefined, canLoad: boolean | undefined): string {
  return canLoad === true && cwd != null ? `?${new URLSearchParams({ cwd })}` : "";
}

function mergeTerminalSnapshot(
  previous: TerminalSnapshot | undefined,
  incoming: TerminalSnapshot,
): TerminalSnapshot {
  const appended = terminalOutputBytes(incoming);
  let bytes = appended;
  if (incoming.outputAppend && previous != null) {
    const prefix = terminalOutputBytes(previous);
    bytes = new Uint8Array(prefix.length + appended.length);
    bytes.set(prefix);
    bytes.set(appended, prefix.length);
  }
  let start = incoming.retainedBytes == null ? 0 : Math.max(0, bytes.length - incoming.retainedBytes);
  while (start < bytes.length && (bytes[start] & 0xc0) === 0x80) start += 1;
  bytes = bytes.subarray(start);
  // Keep the original bytes: a read can end in the middle of a UTF-8 character.
  const output = new TextDecoder().decode(bytes, {
    stream: incoming.exitStatus == null && !incoming.released,
  });
  const parts: string[] = [];
  for (let offset = 0; offset < bytes.length; offset += 16_384) {
    parts.push(String.fromCharCode(...bytes.subarray(offset, offset + 16_384)));
  }
  return {
    ...incoming,
    output,
    outputBytes: btoa(parts.join("")),
    outputAppend: false,
    retainedBytes: bytes.length,
  };
}

function terminalOutputBytes(snapshot: TerminalSnapshot): Uint8Array {
  return snapshot.outputBytes == null
    ? new TextEncoder().encode(snapshot.output)
    : Uint8Array.from(atob(snapshot.outputBytes), (character) => character.charCodeAt(0));
}

function advanceSessionViewToTurnOutcome(
  sessionViewRef: { current: BridgeSessionView | undefined },
  event: Extract<SessionBusinessEvent, {
    type: "bridge/session_turn_complete" | "bridge/session_turn_failed";
  }>,
): boolean {
  const current = sessionViewRef.current;
  if (
    current == null || current.sessionId !== event.sessionId ||
    current.bridgeEpoch !== event.bridgeEpoch ||
    current.sessionIncarnation !== event.sessionIncarnation ||
    (current.activeTurn != null &&
      current.activeTurn.operationId !== event.operationId &&
      current.activeTurn.clientIntentId !== event.clientIntentId) ||
    event.viewRevision < current.viewRevision
  ) return false;
  sessionViewRef.current = {
    ...current,
    viewRevision: event.viewRevision,
    historyRevision: event.historyRevision,
    phase: event.phase,
    activeTurn: event.phase === "ready" ? null : current.activeTurn,
  };
  return true;
}

function clearMatchingPromptAdmission(
  admissions: Map<string, { requestId: string }>,
  event: Extract<SessionBusinessEvent, {
    type: "bridge/session_turn_complete" | "bridge/session_turn_failed";
  }>,
): void {
  if (admissions.get(event.sessionId)?.requestId === event.clientIntentId) {
    admissions.delete(event.sessionId);
  }
}

function setRoute(path: string, replace = false): void {
  if (window.location.pathname === path && !window.location.search && !window.location.hash) return;
  if (replace) window.history.replaceState(null, "", path);
  else window.history.pushState(null, "", path);
}

function isMissingSession(error: unknown): boolean {
  return error instanceof ApiError && error.status === 404 &&
    isRecord(error.body) && error.body.code === "session_not_found";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
