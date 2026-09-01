import { useCallback, useEffect, useReducer, useRef } from "react";
import type {
  AgentCapabilities,
  ContentBlock,
  CreateElicitationResponse,
  RequestPermissionResponse,
  SessionInfo,
} from "@agentclientprotocol/sdk";
import type { ClientCommand, ServerEvent } from "../../../shared/bridge";
import type {
  WorkspaceContextAttachment,
  WorkspaceContextMatch,
} from "../../../shared/bridge";
import { parseServerEvent } from "../../../shared/bridge";
import { randomId } from "./id";
import { appReducer, initialState } from "./state";

type SessionAttachCommand = "session/load" | "session/resume";

interface StartupState {
  started: boolean;
  capabilities?: AgentCapabilities | null;
  listRequestId?: string;
  attachRequestId?: string;
  sessionRequestId?: string;
  attachMethod?: SessionAttachCommand;
  authRequestId?: string;
  retryAfterAuth?: boolean;
  discoveredSessions?: SessionInfo[];
}

type PendingContextRequest =
  | {
      kind: "search";
      timer: ReturnType<typeof setTimeout>;
      resolve: (matches: WorkspaceContextMatch[]) => void;
      reject: (error: Error) => void;
    }
  | {
      kind: "read";
      timer: ReturnType<typeof setTimeout>;
      resolve: (attachment: WorkspaceContextAttachment) => void;
      reject: (error: Error) => void;
    };

const CONTEXT_REQUEST_TIMEOUT_MS = 8_000;
export const RESUME_RECONNECT_AFTER_MS = 1_000;
export const RESUME_PROBE_TIMEOUT_MS = 2_500;
const WEB_SOCKET_OPEN = 1;
const WEB_SOCKET_CONNECTING = 0;
const LAST_SESSION_STORAGE_KEY = "attyd:last-session-id";

export function shouldReconnectAfterResume(
  hiddenAt: number | undefined,
  now: number,
  socketReadyState: number,
): boolean {
  return socketReadyState !== WEB_SOCKET_OPEN || (
    hiddenAt != null && now - hiddenAt >= RESUME_RECONNECT_AFTER_MS
  );
}

export function useAcp() {
  const [state, dispatch] = useReducer(appReducer, initialState);
  const socketRef = useRef<WebSocket | undefined>(undefined);
  const startup = useRef<StartupState>({ started: false });
  const terminalReloadTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const reconnectRef = useRef<() => void>(() => window.location.reload());
  const pendingContextRequests = useRef(new Map<string, PendingContextRequest>());
  const pendingSessionRequests = useRef(new Map<string, {
    kind: "new" | "attach" | "fork" | "close" | "delete";
    sessionId?: string;
  }>());
  const transmit = useCallback((command: ClientCommand) =>
    sendClientCommand(socketRef.current, command, (message) => {
      dispatch({ type: "client/error", message });
    }), []);
  const reconnect = useCallback(() => reconnectRef.current(), []);

  useEffect(() => {
    startup.current = {
      started: false,
      discoveredSessions: [],
    };
    const preferredSessionId = readStoredSessionId();
    const protocol = location.protocol === "https:" ? "wss:" : "ws:";
    const socket = new WebSocket(`${protocol}//${location.host}/ws`);
    socketRef.current = socket;
    let hiddenAt: number | undefined;
    let reconnecting = false;
    let resumeProbe: {
      nonce: string;
      timer: ReturnType<typeof setTimeout>;
    } | undefined;

    const finishSessionDiscovery = () => {
      startup.current = {
        ...startup.current,
        started: true,
        listRequestId: undefined,
        attachRequestId: undefined,
        sessionRequestId: undefined,
      };
    };

    const attachStartupSession = (
      session: SessionInfo,
      method: SessionAttachCommand,
    ) => {
      const requestId = randomId();
      startup.current = {
        ...startup.current,
        started: true,
        listRequestId: undefined,
        attachRequestId: requestId,
        sessionRequestId: undefined,
      };
      if (sendClientCommand(socket, {
        type: method,
        requestId,
        sessionId: session.sessionId,
      }, (message) => dispatch({ type: "client/error", message }))) {
        pendingSessionRequests.current.set(requestId, {
          kind: "attach",
          sessionId: session.sessionId,
        });
        dispatch({
          type: "session/transition_start",
          kind: "attach",
          requestId,
          sessionId: session.sessionId,
          cwd: session.cwd,
          title: session.title,
        });
      } else {
        finishSessionDiscovery();
      }
    };

    const startSessionDiscovery = (
      capabilities: AgentCapabilities | null | undefined,
    ) => {
      const attachMethod = startupAttachMethod(capabilities);
      startup.current = {
        ...startup.current,
        started: true,
        capabilities,
        attachMethod,
        authRequestId: undefined,
        retryAfterAuth: false,
        discoveredSessions: [],
      };
      if (
        capabilities?.sessionCapabilities?.list != null &&
        attachMethod != null
      ) {
        const requestId = randomId();
        startup.current = {
          ...startup.current,
          listRequestId: requestId,
          attachRequestId: undefined,
          sessionRequestId: undefined,
        };
        if (!sendClientCommand(socket, {
          type: "session/list",
          requestId,
        }, (message) => dispatch({ type: "client/error", message }))) {
          finishSessionDiscovery();
        }
      } else {
        finishSessionDiscovery();
      }
    };

    const rejectPendingContext = (message: string) => {
      for (const pending of pendingContextRequests.current.values()) {
        clearTimeout(pending.timer);
        pending.reject(new Error(message));
      }
      pendingContextRequests.current.clear();
    };

    const reconnectByReload = () => {
      if (reconnecting) return;
      reconnecting = true;
      if (resumeProbe) clearTimeout(resumeProbe.timer);
      resumeProbe = undefined;
      rejectPendingContext("ACP connection is restarting after the page resumed");
      socket.close();
      window.location.reload();
    };
    reconnectRef.current = reconnectByReload;

    const probeConnection = () => {
      if (reconnecting) return;
      if (socket.readyState === WEB_SOCKET_CONNECTING) return;
      if (socket.readyState !== WebSocket.OPEN) {
        reconnectByReload();
        return;
      }
      if (resumeProbe) clearTimeout(resumeProbe.timer);
      const nonce = randomId();
      if (!sendClientCommand(socket, { type: "bridge/ping", nonce }, reconnectByReload)) return;
      resumeProbe = {
        nonce,
        timer: setTimeout(reconnectByReload, RESUME_PROBE_TIMEOUT_MS),
      };
    };

    const handleVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        hiddenAt = Date.now();
        return;
      }
      if (shouldReconnectAfterResume(hiddenAt, Date.now(), socket.readyState)) {
        reconnectByReload();
      } else {
        hiddenAt = undefined;
        probeConnection();
      }
    };
    const handlePageShow = (event: PageTransitionEvent) => {
      if (event.persisted || shouldReconnectAfterResume(hiddenAt, Date.now(), socket.readyState)) {
        reconnectByReload();
      } else {
        probeConnection();
      }
    };
    const handlePageHide = () => { hiddenAt = Date.now(); };
    const handleOnline = () => probeConnection();
    // Some mobile browsers resume with neither a fresh pageshow nor a reliable
    // visibility transition. A focus-time round trip catches sockets that are
    // still reported OPEN locally but no longer reach the server.
    const handleFocus = () => probeConnection();

    document.addEventListener("visibilitychange", handleVisibilityChange);
    window.addEventListener("pageshow", handlePageShow);
    window.addEventListener("pagehide", handlePageHide);
    window.addEventListener("online", handleOnline);
    window.addEventListener("focus", handleFocus);

    socket.addEventListener("open", () => dispatch({ type: "socket/open" }));
    socket.addEventListener("close", () => {
      rejectPendingContext("ACP WebSocket closed while preparing workspace context");
      dispatch({ type: "socket/closed" });
    });
    socket.addEventListener("message", ({ data }) => {
      let event: ServerEvent;
      try {
        event = parseServerEvent(String(data));
      } catch (error) {
        dispatch({
          type: "client/error",
          message: `Invalid server event: ${error instanceof Error ? error.message : String(error)}`,
        });
        return;
      }
      if (event.type === "bridge/pong") {
        if (resumeProbe?.nonce === event.nonce) {
          clearTimeout(resumeProbe.timer);
          resumeProbe = undefined;
        }
        return;
      }
      if (
        event.type === "bridge/context_search_result" ||
        event.type === "bridge/context_attached"
      ) {
        const pending = pendingContextRequests.current.get(event.requestId);
        if (
          (event.type === "bridge/context_search_result" && pending?.kind === "search") ||
          (event.type === "bridge/context_attached" && pending?.kind === "read")
        ) {
          clearTimeout(pending.timer);
          pendingContextRequests.current.delete(event.requestId);
          if (event.type === "bridge/context_search_result" && pending.kind === "search") {
            pending.resolve(event.matches);
          } else if (event.type === "bridge/context_attached" && pending.kind === "read") {
            pending.resolve(event.attachment);
          }
        }
        return;
      }
      if (event.type === "bridge/error" && event.requestId) {
        const pending = pendingContextRequests.current.get(event.requestId);
        if (pending) {
          clearTimeout(pending.timer);
          pendingContextRequests.current.delete(event.requestId);
          pending.reject(new Error(event.message));
          return;
        }
      }
      dispatch({ type: "server/event", event });
      if (event.type === "acp/permission_request") {
        storeSessionId(event.request.sessionId);
      } else if (
        event.type === "acp/elicitation_request" &&
        "sessionId" in event.request &&
        typeof event.request.sessionId === "string"
      ) {
        storeSessionId(event.request.sessionId);
      }
      const pendingSession = "requestId" in event && typeof event.requestId === "string"
        ? pendingSessionRequests.current.get(event.requestId)
        : undefined;
      if (event.type === "acp/session_created" && pendingSession?.kind === "new") {
        storeSessionId(event.response.sessionId);
        pendingSessionRequests.current.delete(event.requestId);
      } else if (event.type === "acp/session_attached" && pendingSession?.kind === "attach") {
        storeSessionId(event.sessionId);
        pendingSessionRequests.current.delete(event.requestId);
      } else if (event.type === "acp/session_forked" && pendingSession?.kind === "fork") {
        storeSessionId(event.response.sessionId);
        pendingSessionRequests.current.delete(event.requestId);
      } else if (
        (event.type === "acp/session_closed" || event.type === "acp/session_deleted") &&
        (pendingSession?.kind === "close" || pendingSession?.kind === "delete")
      ) {
        if (readStoredSessionId() === event.sessionId) storeSessionId(undefined);
        pendingSessionRequests.current.delete(event.requestId);
      } else if (event.type === "bridge/error" && event.requestId) {
        pendingSessionRequests.current.delete(event.requestId);
      }
      if (
        event.type === "bridge/auth_terminal_exited" &&
        event.status === "succeeded" &&
        event.requestId === startup.current.authRequestId
      ) {
        startup.current = {
          ...startup.current,
          authRequestId: undefined,
          retryAfterAuth: false,
        };
        terminalReloadTimer.current = setTimeout(() => window.location.reload(), 350);
        return;
      }
      if (event.type === "acp/initialized" && !startup.current.started) {
        const capabilities = event.response.agentCapabilities;
        startSessionDiscovery(capabilities);
        return;
      }
      if (
        event.type === "acp/sessions_listed" &&
        event.requestId === startup.current.listRequestId
      ) {
        const discovered = mergeDiscoveredSessions(
          startup.current.discoveredSessions ?? [],
          event.response.sessions,
        );
        startup.current = { ...startup.current, discoveredSessions: discovered };
        const method = startup.current.attachMethod;
        const preferred = preferredSessionId == null
          ? undefined
          : discovered.find(({ sessionId }) => sessionId === preferredSessionId);
        if (preferred && method) {
          attachStartupSession(preferred, method);
          return;
        }
        if (preferredSessionId && event.response.nextCursor) {
          const requestId = randomId();
          startup.current = { ...startup.current, listRequestId: requestId };
          if (!sendClientCommand(socket, {
            type: "session/list",
            requestId,
            cursor: event.response.nextCursor,
          }, (message) => dispatch({ type: "client/error", message }))) {
            finishSessionDiscovery();
          }
          return;
        }
        const session = mostRecentSession(discovered);
        if (session && method) attachStartupSession(session, method);
        else finishSessionDiscovery();
        return;
      }
      if (
        event.type === "bridge/error" &&
        (event.requestId === startup.current.listRequestId ||
          event.requestId === startup.current.attachRequestId ||
          event.requestId === startup.current.sessionRequestId)
      ) {
        if (event.code === -32_000) {
          startup.current = {
            ...startup.current,
            listRequestId: undefined,
            attachRequestId: undefined,
            sessionRequestId: undefined,
            retryAfterAuth: true,
          };
          return;
        }
        if (event.requestId === startup.current.sessionRequestId) return;
        finishSessionDiscovery();
        return;
      }
      if (
        event.type === "acp/authenticated" &&
        event.requestId === startup.current.authRequestId
      ) {
        const retry = startup.current.retryAfterAuth === true;
        const capabilities = startup.current.capabilities;
        startup.current = {
          ...startup.current,
          authRequestId: undefined,
          retryAfterAuth: false,
        };
        if (retry) startSessionDiscovery(capabilities);
        return;
      }
      if (
        event.type === "bridge/error" &&
        event.requestId === startup.current.authRequestId
      ) {
        if (
          event.operation === "auth/terminal_input" ||
          event.operation === "auth/terminal_resize" ||
          event.operation === "auth/terminal_cancel"
        ) return;
        startup.current = {
          ...startup.current,
          authRequestId: undefined,
          retryAfterAuth: false,
        };
        return;
      }
      if (
        (event.type === "acp/session_attached" || event.type === "acp/session_created") &&
        (event.requestId === startup.current.attachRequestId ||
          event.requestId === startup.current.sessionRequestId)
      ) {
        startup.current = {
          ...startup.current,
          started: true,
          listRequestId: undefined,
          attachRequestId: undefined,
          sessionRequestId: undefined,
          retryAfterAuth: false,
        };
        return;
      }
    });

    return () => {
      if (terminalReloadTimer.current != null) clearTimeout(terminalReloadTimer.current);
      document.removeEventListener("visibilitychange", handleVisibilityChange);
      window.removeEventListener("pageshow", handlePageShow);
      window.removeEventListener("pagehide", handlePageHide);
      window.removeEventListener("online", handleOnline);
      window.removeEventListener("focus", handleFocus);
      if (resumeProbe) clearTimeout(resumeProbe.timer);
      reconnectRef.current = () => window.location.reload();
      rejectPendingContext("Workspace context request was cancelled");
      pendingSessionRequests.current.clear();
      socketRef.current = undefined;
      socket.close();
    };
  }, []);

  const searchWorkspaceContext = useCallback((query: string) =>
    new Promise<WorkspaceContextMatch[]>((resolve, reject) => {
      const requestId = randomId();
      const timer = setTimeout(() => {
        pendingContextRequests.current.delete(requestId);
        reject(new Error("Workspace context search timed out"));
      }, CONTEXT_REQUEST_TIMEOUT_MS);
      pendingContextRequests.current.set(requestId, {
        kind: "search",
        timer,
        resolve,
        reject,
      });
      if (!sendClientCommand(socketRef.current, {
        type: "context/search",
        requestId,
        query,
      }, (message) => {
        clearTimeout(timer);
        pendingContextRequests.current.delete(requestId);
        reject(new Error(message));
      })) {
        clearTimeout(timer);
        pendingContextRequests.current.delete(requestId);
      }
    }), []);

  const readWorkspaceContext = useCallback((path: string) =>
    new Promise<WorkspaceContextAttachment>((resolve, reject) => {
      const sessionId = state.session?.sessionId;
      if (!sessionId) {
        reject(new Error("Wait for an active ACP session before adding workspace context"));
        return;
      }
      const requestId = randomId();
      const timer = setTimeout(() => {
        pendingContextRequests.current.delete(requestId);
        reject(new Error("Workspace context read timed out"));
      }, CONTEXT_REQUEST_TIMEOUT_MS);
      pendingContextRequests.current.set(requestId, {
        kind: "read",
        timer,
        resolve,
        reject,
      });
      if (!sendClientCommand(socketRef.current, {
        type: "context/read",
        requestId,
        sessionId,
        path,
      }, (message) => {
        clearTimeout(timer);
        pendingContextRequests.current.delete(requestId);
        reject(new Error(message));
      })) {
        clearTimeout(timer);
        pendingContextRequests.current.delete(requestId);
      }
    }), [state.session?.sessionId]);

  const prompt = useCallback(
    (blocks: ContentBlock[]) => {
      if (!state.session) return false;
      const requestId = randomId();
      const sessionId = state.session.sessionId;
      if (!transmit({
        type: "session/prompt",
        requestId,
        sessionId,
        prompt: blocks,
      })) return false;
      dispatch({ type: "user/prompt", requestId, sessionId, blocks });
      return true;
    },
    [state.session, transmit],
  );

  const cancel = useCallback(() => {
    if (!state.session) return;
    transmit({
      type: "session/cancel",
      sessionId: state.session.sessionId,
    });
  }, [state.session, transmit]);

  const setMode = useCallback(
    (modeId: string) => {
      if (!state.session) return;
      const requestId = randomId();
      if (!transmit({
        type: "session/set_mode",
        requestId,
        sessionId: state.session.sessionId,
        modeId,
      })) return;
      dispatch({
        type: "session/control_start",
        kind: "mode",
        requestId,
        sessionId: state.session.sessionId,
      });
    },
    [state.session, transmit],
  );

  const setConfig = useCallback(
    (configId: string, value: string | boolean) => {
      if (!state.session) return;
      const requestId = randomId();
      if (!transmit({
        type: "session/set_config_option",
        requestId,
        sessionId: state.session.sessionId,
        configId,
        value,
      })) return;
      dispatch({
        type: "session/control_start",
        kind: "config",
        requestId,
        sessionId: state.session.sessionId,
      });
    },
    [state.session, transmit],
  );

  const respondPermission = useCallback(
    (permissionId: string, outcome: RequestPermissionResponse["outcome"]) => {
      const requestId = randomId();
      if (!transmit({
        type: "permission/respond",
        requestId,
        permissionId,
        outcome,
      })) return;
      dispatch({ type: "permission/respond_start", permissionId, requestId });
    },
    [transmit],
  );

  const authenticate = useCallback((methodId: string) => {
    const requestId = randomId();
    const method = state.initialized?.authMethods?.find((candidate) => candidate.id === methodId);
    const terminal = method != null && "type" in method && method.type === "terminal";
    if (!transmit(terminal
      ? { type: "auth/terminal_start", requestId, methodId, cols: 80, rows: 24 }
      : { type: "auth/authenticate", requestId, methodId })) return;
    startup.current = {
      ...startup.current,
      authRequestId: requestId,
      retryAfterAuth: state.session == null,
    };
    dispatch({
      type: "auth/start",
      kind: terminal ? "terminal" : "authenticate",
      requestId,
      methodId,
    });
  }, [state.initialized?.authMethods, state.session, transmit]);

  const writeAuthTerminal = useCallback((requestId: string, data: string) => {
    transmit({ type: "auth/terminal_input", requestId, data });
  }, [transmit]);

  const resizeAuthTerminal = useCallback((requestId: string, cols: number, rows: number) => {
    transmit({ type: "auth/terminal_resize", requestId, cols, rows });
  }, [transmit]);

  const cancelAuthTerminal = useCallback((requestId: string) => {
    transmit({ type: "auth/terminal_cancel", requestId });
  }, [transmit]);

  const dismissAuthTerminal = useCallback(() => {
    dispatch({ type: "auth/dismiss_terminal" });
  }, []);

  const logout = useCallback(() => {
    const requestId = randomId();
    if (!transmit({ type: "auth/logout", requestId })) return;
    startup.current = { ...startup.current, authRequestId: requestId };
    dispatch({ type: "auth/start", kind: "logout", requestId });
  }, [transmit]);

  const newSession = useCallback((cwd: string): boolean => {
    const requestId = randomId();
    if (!transmit({
      type: "session/new",
      requestId,
      cwd,
    })) return false;
    pendingSessionRequests.current.set(requestId, { kind: "new" });
    dispatch({ type: "session/transition_start", kind: "new", requestId, cwd });
    return true;
  }, [transmit]);

  const listSessions = useCallback((cursor?: string) => {
    transmit({
      type: "session/list",
      requestId: randomId(),
      cursor,
    });
  }, [transmit]);

  const attachSession = useCallback(
    (session: SessionInfo) => {
      if (state.session?.sessionId === session.sessionId) return;
      if (state.cachedSessions.has(session.sessionId)) {
        dispatch({ type: "session/activate_cached", sessionId: session.sessionId });
        storeSessionId(session.sessionId);
        return;
      }
      const capabilities = state.initialized?.agentCapabilities;
      const method = capabilities?.loadSession
        ? "session/load"
        : capabilities?.sessionCapabilities?.resume != null
          ? "session/resume"
          : undefined;
      if (!method) return;
      const requestId = randomId();
      if (transmit({
        type: method,
        requestId,
        sessionId: session.sessionId,
      })) {
        pendingSessionRequests.current.set(requestId, {
          kind: "attach",
          sessionId: session.sessionId,
        });
        dispatch({
          type: "session/transition_start",
          kind: "attach",
          requestId,
          sessionId: session.sessionId,
          cwd: session.cwd,
          title: session.title,
        });
      }
    },
    [state.cachedSessions, state.initialized, state.session?.sessionId, transmit],
  );

  const closeSession = useCallback(() => {
    if (!state.session) return;
    const requestId = randomId();
    if (!transmit({
      type: "session/close",
      requestId,
      sessionId: state.session.sessionId,
    })) return;
    pendingSessionRequests.current.set(requestId, {
      kind: "close",
      sessionId: state.session.sessionId,
    });
    dispatch({
      type: "session/transition_start",
      kind: "close",
      requestId,
      sessionId: state.session.sessionId,
    });
  }, [state.session, transmit]);

  const forkSession = useCallback(() => {
    if (!state.session) return;
    const requestId = randomId();
    if (!transmit({
      type: "session/fork",
      requestId,
      sessionId: state.session.sessionId,
    })) return;
    pendingSessionRequests.current.set(requestId, {
      kind: "fork",
      sessionId: state.session.sessionId,
    });
    dispatch({
      type: "session/transition_start",
      kind: "fork",
      requestId,
      sessionId: state.session.sessionId,
    });
  }, [state.session, transmit]);

  const deleteSession = useCallback((sessionId: string) => {
    const requestId = randomId();
    if (!transmit({
      type: "session/delete",
      requestId,
      sessionId,
    })) return;
    pendingSessionRequests.current.set(requestId, { kind: "delete", sessionId });
    dispatch({ type: "session/delete_start", requestId, sessionId });
  }, [transmit]);

  const respondElicitation = useCallback(
    (elicitationId: string, response: CreateElicitationResponse) => {
      const requestId = randomId();
      if (!transmit({
        type: "elicitation/respond",
        requestId,
        elicitationId,
        response,
      })) return;
      dispatch({ type: "elicitation/respond_start", elicitationId, requestId });
    },
    [transmit],
  );

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

export function startupAttachMethod(
  capabilities: AgentCapabilities | null | undefined,
): SessionAttachCommand | undefined {
  if (capabilities?.loadSession) return "session/load";
  if (capabilities?.sessionCapabilities?.resume != null) return "session/resume";
  return undefined;
}

export function mostRecentSession(sessions: SessionInfo[]): SessionInfo | undefined {
  let best: SessionInfo | undefined;
  let bestTimestamp = Number.NEGATIVE_INFINITY;
  for (const session of sessions) {
    const timestamp = typeof session.updatedAt === "string"
      ? Date.parse(session.updatedAt)
      : Number.NaN;
    const comparable = Number.isFinite(timestamp) ? timestamp : Number.NEGATIVE_INFINITY;
    if (!best || comparable > bestTimestamp) {
      best = session;
      bestTimestamp = comparable;
    }
  }
  return best;
}

function mergeDiscoveredSessions(
  current: SessionInfo[],
  incoming: SessionInfo[],
): SessionInfo[] {
  const sessions = new Map(current.map((session) => [session.sessionId, session]));
  for (const session of incoming) sessions.set(session.sessionId, session);
  return [...sessions.values()];
}

function readStoredSessionId(): string | undefined {
  try {
    const value = window.localStorage.getItem(LAST_SESSION_STORAGE_KEY);
    return value || undefined;
  } catch {
    return undefined;
  }
}

function storeSessionId(sessionId: string | undefined): void {
  try {
    if (sessionId) window.localStorage.setItem(LAST_SESSION_STORAGE_KEY, sessionId);
    else window.localStorage.removeItem(LAST_SESSION_STORAGE_KEY);
  } catch {
    // Storage can be unavailable in privacy modes; session discovery still
    // falls back to the Agent's most recently updated thread.
  }
}

export function sendClientCommand(
  socket: Pick<WebSocket, "readyState" | "send"> | undefined,
  command: ClientCommand,
  onError: (message: string) => void,
): boolean {
  if (!socket || socket.readyState !== 1) {
    onError(`Cannot send ${command.type}: ACP WebSocket is not open`);
    return false;
  }
  try {
    socket.send(JSON.stringify(command));
    return true;
  } catch (error) {
    onError(
      `Cannot send ${command.type}: ${error instanceof Error ? error.message : String(error)}`,
    );
    return false;
  }
}
