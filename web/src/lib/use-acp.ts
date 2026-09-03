import { useCallback, useEffect, useReducer, useRef } from "react";
import type {
  ContentBlock,
  CreateElicitationResponse,
  RequestPermissionResponse,
  SessionInfo,
} from "@agentclientprotocol/sdk";
import type { ServerEvent } from "../../../shared/bridge";
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
  parseGlobalBusinessEvent,
  parseSessionBusinessEvent,
  requestJson,
  strongEtag,
  workspaceContextSearchPath,
} from "./business-api";
import { randomId } from "./id";
import { appReducer, initialState } from "./state";

const LAST_SESSION_STORAGE_KEY = "attyd:last-session-id";

export function useAcp() {
  const [state, dispatch] = useReducer(appReducer, initialState);
  const stateRef = useRef(state);
  stateRef.current = state;

  const globalEventsRef = useRef<EventSource | undefined>(undefined);
  const sessionEventsRef = useRef<EventSource | undefined>(undefined);
  const activeSessionIdRef = useRef<string | undefined>(undefined);
  const sessionViewRef = useRef<BridgeSessionView | undefined>(undefined);
  const refreshInFlightRef = useRef(false);
  const refreshPendingRef = useRef(false);
  const reconnectRef = useRef<() => void>(() => undefined);
  const refreshSessionRef = useRef<(sessionId: string) => void>(() => undefined);

  const reportError = useCallback((error: unknown) => {
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
    if (
      current != null &&
      current.sessionId === view.sessionId &&
      current.bridgeEpoch === view.bridgeEpoch &&
      current.sessionIncarnation === view.sessionIncarnation &&
      view.viewRevision < current.viewRevision
    ) return;
    sessionViewRef.current = view;
    storeSessionId(view.sessionId);
    dispatch({ type: "bridge/session_hydrate", view });
  }, []);

  const refreshSession = useCallback((sessionId: string) => {
    if (activeSessionIdRef.current !== sessionId) return;
    if (refreshInFlightRef.current) {
      refreshPendingRef.current = true;
      return;
    }
    refreshInFlightRef.current = true;
    void (async () => {
      try {
        do {
          refreshPendingRef.current = false;
          const view = await requestJson<BridgeSessionView>(
            `/api/v1/sessions/${encodeURIComponent(sessionId)}`,
          );
          if (activeSessionIdRef.current !== sessionId) return;
          hydrateSession(view);
        } while (refreshPendingRef.current && activeSessionIdRef.current === sessionId);
      } catch (error) {
        if (activeSessionIdRef.current === sessionId) reportError(error);
      } finally {
        refreshInFlightRef.current = false;
        if (refreshPendingRef.current && activeSessionIdRef.current === sessionId) {
          refreshPendingRef.current = false;
          refreshSessionRef.current(sessionId);
        }
      }
    })();
  }, [hydrateSession, reportError]);
  refreshSessionRef.current = refreshSession;

  const connectSessionEvents = useCallback((sessionId: string) => {
    sessionEventsRef.current?.close();
    const source = new EventSource(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/events`,
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

  const activateSession = useCallback((session: SessionInfo, transition = true) => {
    if (activeSessionIdRef.current === session.sessionId && sessionViewRef.current != null) {
      refreshSessionRef.current(session.sessionId);
      return;
    }
    const requestId = randomId();
    activeSessionIdRef.current = session.sessionId;
    sessionViewRef.current = undefined;
    refreshPendingRef.current = false;
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
    refreshSessionRef.current(session.sessionId);
  }, [connectSessionEvents]);

  const refreshSessionList = useCallback(async (cursor?: string) => {
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

  const refreshRuntime = useCallback(async (selectInitialSession: boolean) => {
    try {
      const runtime = await requestJson<RuntimeView>("/api/v1/runtime");
      dispatch({ type: "socket/open" });
      if (runtime.hello != null) dispatch({ type: "server/event", event: runtime.hello });
      if (runtime.initialized != null) {
        dispatch({ type: "server/event", event: runtime.initialized });
      }
      if (runtime.error != null) dispatch({ type: "server/event", event: runtime.error });
      if (runtime.phase != null) dispatch({ type: "server/event", event: runtime.phase });
      else if (!runtime.connected) dispatch({ type: "socket/closed" });

      const listed = await refreshSessionList();
      if (!selectInitialSession || activeSessionIdRef.current != null) return;
      const stored = readStoredSessionId();
      const selected = stored == null
        ? mostRecentSession(listed.sessions)
        : listed.sessions.find(({ sessionId }) => sessionId === stored) ??
          { sessionId: stored, cwd: "" };
      if (selected != null) activateSession(selected, false);
    } catch (error) {
      dispatch({ type: "socket/closed" });
      reportError(error);
    }
  }, [activateSession, refreshSessionList, reportError]);

  const handleGlobalEvent = useCallback((event: GlobalBusinessEvent) => {
    switch (event.type) {
      case "bridge/connection":
        dispatch({
          type: "server/event",
          event: { type: "bridge/phase", phase: event.phase },
        });
        if (event.phase === "ready" && activeSessionIdRef.current != null) {
          refreshSessionRef.current(activeSessionIdRef.current);
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
        return;
      default:
        dispatch({ type: "server/event", event: event as ServerEvent });
    }
  }, []);

  const connectGlobalEvents = useCallback(() => {
    globalEventsRef.current?.close();
    const source = new EventSource("/api/v1/events");
    globalEventsRef.current = source;
    source.onopen = () => {
      dispatch({ type: "socket/open" });
      void refreshRuntime(false);
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
      const sessionId = activeSessionIdRef.current;
      if (sessionId != null) {
        connectSessionEvents(sessionId);
        refreshSessionRef.current(sessionId);
      }
    };
    reconnectRef.current = reconnectStreams;
    connectGlobalEvents();
    void refreshRuntime(true);

    const recoverClosedStreams = () => {
      if (
        globalEventsRef.current?.readyState === EventSource.CLOSED ||
        (activeSessionIdRef.current != null &&
          sessionEventsRef.current?.readyState === EventSource.CLOSED)
      ) reconnectStreams();
    };
    window.addEventListener("online", recoverClosedStreams);
    window.addEventListener("pageshow", recoverClosedStreams);
    document.addEventListener("visibilitychange", recoverClosedStreams);
    return () => {
      disposed = true;
      reconnectRef.current = () => undefined;
      window.removeEventListener("online", recoverClosedStreams);
      window.removeEventListener("pageshow", recoverClosedStreams);
      document.removeEventListener("visibilitychange", recoverClosedStreams);
      globalEventsRef.current?.close();
      sessionEventsRef.current?.close();
    };
  }, [connectGlobalEvents, connectSessionEvents, refreshRuntime]);

  const searchWorkspaceContext = useCallback(async (query: string) => {
    const response = await requestJson<{ matches: WorkspaceContextMatch[] }>(
      workspaceContextSearchPath(query),
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
    dispatch({ type: "user/prompt", requestId, sessionId, blocks });
    void requestJson<StartTurnResult>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/turns`,
      {
        method: "POST",
        headers: {
          "If-Match": strongEtag(view.historyRevision),
          "Idempotency-Key": requestId,
        },
        body: JSON.stringify({ prompt: blocks }),
      },
    ).then(() => refreshSessionRef.current(sessionId)).catch((error) => {
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
    if (sessionId == null || sessionViewRef.current?.phase !== "ready") return;
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
    if (sessionId == null || sessionViewRef.current?.phase !== "ready") return;
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
      void refreshRuntime(false);
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
    const requestId = randomId();
    dispatch({ type: "session/transition_start", kind: "new", requestId, cwd });
    void requestJson<CreatedSessionResult>("/api/v1/sessions", {
      method: "POST",
      body: JSON.stringify({ cwd }),
    }).then((created) => {
      activeSessionIdRef.current = created.sessionId;
      sessionViewRef.current = undefined;
      sessionEventsRef.current?.close();
      connectSessionEvents(created.sessionId);
      hydrateSession(created.view);
      void refreshSessionList();
    }).catch((error) => reportRequestError(error, requestId, "session/new"));
    return true;
  }, [connectSessionEvents, hydrateSession, refreshSessionList, reportRequestError]);

  const listSessions = useCallback((cursor?: string) => {
    void refreshSessionList(cursor).catch(reportError);
  }, [refreshSessionList, reportError]);

  const attachSession = useCallback((session: SessionInfo) => activateSession(session), [activateSession]);

  const closeSession = useCallback(() => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null || stateRef.current.running) return;
    const requestId = randomId();
    dispatch({ type: "session/transition_start", kind: "close", requestId, sessionId });
    void requestJson(`/api/v1/sessions/${encodeURIComponent(sessionId)}/close`, {
      method: "POST",
    }).then(() => {
      sessionEventsRef.current?.close();
      activeSessionIdRef.current = undefined;
      sessionViewRef.current = undefined;
      storeSessionId(undefined);
      dispatch({ type: "session/reset" });
      void refreshSessionList();
    }).catch((error) => reportRequestError(error, requestId, "session/close"));
  }, [refreshSessionList, reportRequestError]);

  const forkSession = useCallback(() => {
    const sessionId = activeSessionIdRef.current;
    if (sessionId == null || stateRef.current.running) return;
    const requestId = randomId();
    dispatch({ type: "session/transition_start", kind: "fork", requestId, sessionId });
    void requestJson<CreatedSessionResult>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/fork`,
      { method: "POST" },
    ).then((forked) => {
      activeSessionIdRef.current = forked.sessionId;
      sessionViewRef.current = undefined;
      sessionEventsRef.current?.close();
      connectSessionEvents(forked.sessionId);
      hydrateSession(forked.view);
      void refreshSessionList();
    }).catch((error) => reportRequestError(error, requestId, "session/fork"));
  }, [connectSessionEvents, hydrateSession, refreshSessionList, reportRequestError]);

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
        sessionEventsRef.current?.close();
        activeSessionIdRef.current = undefined;
        sessionViewRef.current = undefined;
        storeSessionId(undefined);
      }
      dispatch({
        type: "server/event",
        event: { type: "acp/session_deleted", requestId, sessionId },
      });
      void refreshSessionList();
    }).catch((error) => reportRequestError(error, requestId, "session/delete"));
  }, [refreshSessionList, reportRequestError]);

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
    forkSession,
    closeSession,
    deleteSession,
    searchWorkspaceContext,
    readWorkspaceContext,
  };
}

export function mostRecentSession(sessions: SessionInfo[]): SessionInfo | undefined {
  let selected: SessionInfo | undefined;
  let selectedTime = Number.NEGATIVE_INFINITY;
  for (const session of sessions) {
    const time = session.updatedAt == null
      ? Number.NEGATIVE_INFINITY
      : Date.parse(session.updatedAt);
    if (selected == null || time > selectedTime) {
      selected = session;
      selectedTime = Number.isNaN(time) ? Number.NEGATIVE_INFINITY : time;
    }
  }
  return selected;
}

function readStoredSessionId(): string | undefined {
  try {
    return localStorage.getItem(LAST_SESSION_STORAGE_KEY) ?? undefined;
  } catch {
    return undefined;
  }
}

function storeSessionId(sessionId: string | undefined): void {
  try {
    if (sessionId == null) localStorage.removeItem(LAST_SESSION_STORAGE_KEY);
    else localStorage.setItem(LAST_SESSION_STORAGE_KEY, sessionId);
  } catch {
    // Private browsing can deny storage while the live REST/SSE session remains usable.
  }
}
