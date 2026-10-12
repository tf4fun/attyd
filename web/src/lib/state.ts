import type {
  AuthenticateResponse,
  AvailableCommand,
  CompactionStatus,
  ContentBlock,
  CreateElicitationRequest,
  InitializeResponse,
  NewSessionResponse,
  LogoutResponse,
  PromptResponse,
  RequestPermissionRequest,
  SessionConfigOption,
  SessionInfo,
  SessionNotification,
  SessionUpdate,
  ToolCall,
  ToolCallUpdate,
} from "@agentclientprotocol/sdk";
import type {
  ConnectionPhase,
  ElicitationAbortReason,
  SessionRuntimeOperation,
  TerminalSnapshot,
} from "../../../shared/bridge";
import { assertNever } from "../../../shared/exhaustive";
import type {
  BridgeSessionView,
  BridgeTurnProcessPage,
  GlobalBusinessEvent,
  RuntimeView,
  SessionBusinessEvent,
  SessionListResult,
  SessionOwner,
  SessionSyncPhase,
} from "../../../shared/business-api";
import { sameSessionOwner } from "../../../shared/business-api";
import type { SessionControlKind } from "./session-deltas";
import { randomId } from "./id";
import { timelineTurnStarts } from "./timeline-turns";
import { releaseTimelineProcess, releaseUnreferencedTerminals } from "./process-retention";

export interface AssistantMessageChunk {
  id: string;
  role: "agent" | "thought";
  blocks: ContentBlock[];
  messageId?: string | null;
  raw: unknown[];
}

export interface DeferredTurnProcess {
  turnId: string;
  operationId?: string;
  historyRevision: string;
  processCount: number;
  owner: SessionOwner;
}

export type TimelineItem = (
  | {
      id: string;
      type: "message";
      role: "user" | "protocol-user";
      blocks: ContentBlock[];
      messageId?: string | null;
      turnOperationId?: string;
      echoBlocks?: ContentBlock[];
      raw: unknown[];
    }
  | {
      id: string;
      type: "assistant";
      chunks: AssistantMessageChunk[];
    }
  | {
      id: string;
      type: "tool";
      call: ToolCall;
      cancelled?: boolean;
      raw: unknown[];
    }
  | { id: string; type: "plan"; update: SessionUpdate; raw: SessionNotification[] }
  | {
      id: string;
      type: "compaction";
      compactionId: string;
      status: CompactionStatus;
      blocks: ContentBlock[];
      error?: string;
      raw: SessionNotification[];
    }
  | { id: string; type: "protocol"; notification: SessionNotification }
  | { id: string; type: "stop"; response: PromptResponse }
  | {
      id: string;
      type: "error";
      message: string;
      requestId?: string;
      operation?: string;
      code?: number;
      data?: unknown;
      dataTruncated?: boolean;
      dataBytes?: number;
      retryBlocks?: ContentBlock[];
    }
) & {
  historyTurnId?: string;
  deferredProcess?: DeferredTurnProcess;
  retainedProcess?: DeferredTurnProcess;
};

export interface PendingPermission {
  permissionId: string;
  request: RequestPermissionRequest;
  responseRequestId?: string;
  responseError?: string;
}

export interface PendingElicitation {
  elicitationId: string;
  request: CreateElicitationRequest;
  responseRequestId?: string;
  responseError?: string;
}

export interface ExternalElicitationFlow {
  elicitationId: string;
  sessionId?: string;
  url?: string;
  message: string;
  status: "waiting" | "completed" | "cancelled";
  abortReason?: ElicitationAbortReason;
}

/** A failed request or connection, as reported by the business API. */
export interface BridgeError {
  message: string;
  requestId?: string;
  operation?: string;
  code?: number;
  data?: unknown;
  dataTruncated?: boolean;
  dataBytes?: number;
}

type RuntimeHello = NonNullable<RuntimeView["hello"]>;

export type AgentActivity =
  | { kind: "waiting" }
  | { kind: "thinking"; timelineId: string }
  | { kind: "tool"; toolCallId: string; title: string }
  | { kind: "planning" }
  | { kind: "compacting" }
  | { kind: "responding" };

export interface ActiveSessionSnapshot {
  cwd: string;
  session?: NewSessionResponse;
  sessionOwner?: SessionOwner;
  availableCommands: AvailableCommand[];
  modeId?: string;
  configOptions: SessionConfigOption[] | null;
  timeline: TimelineItem[];
  permissions: PendingPermission[];
  elicitations: PendingElicitation[];
  running: boolean;
  agentActivity?: AgentActivity;
  pendingPrompt?: PendingPrompt;
  runtimeOperation?: { requestId: string; operation: SessionRuntimeOperation };
  title?: string;
  usage?: AppState["usage"];
  activePlan?: AppState["activePlan"];
  terminalSnapshots: TerminalSnapshot[];
  historyStatus?: HistoryStatus;
  historyNotice?: string;
}

export type HistoryStatus =
  | {
      state: "loading";
      sessionId: string;
      requestId: string;
    }
  | {
      state: "available";
      sessionId: string;
    }
  | {
      state: "unavailable";
      sessionId?: string;
      reason: "load_not_supported";
      message: string;
    };

export interface SessionTransition {
  kind: "new" | "attach" | "fork" | "close";
  requestId: string;
  targetSessionId?: string;
  targetCwd?: string;
  backup: ActiveSessionSnapshot;
}

export interface PendingSessionDeletion {
  requestId: string;
  sessionId: string;
  stage: "closing" | "deleting";
}

export interface PendingSessionControl {
  requestId: string;
  sessionId: string;
  kind: "mode" | "config";
}

export interface PendingPrompt {
  requestId: string;
  sessionId: string;
  blocks: ContentBlock[];
  cancelRequested?: boolean;
}

export type AgentAuthStatus =
  | "available"
  | "required"
  | "authenticated"
  | "logged_out";

export interface PendingAuthOperation {
  requestId: string;
  kind: "authenticate" | "terminal" | "logout";
  methodId?: string;
}

export interface AgentAuthResponse {
  kind: "authenticate" | "logout";
  response: AuthenticateResponse | LogoutResponse;
}

export interface AuthTerminalState {
  requestId: string;
  methodId: string;
  status: "starting" | "running" | "succeeded" | "failed" | "cancelled";
  output: string;
  truncated: boolean;
  exitCode?: number | null;
  signal?: number;
  message?: string;
}

export interface AppState {
  sessionOwner?: SessionOwner;
  phase: ConnectionPhase;
  socketOpen: boolean;
  transport: RuntimeHello["transport"];
  command: string[];
  defaultCwd: string;
  cwd: string;
  additionalDirectories: string[];
  mcpServers: RuntimeHello["mcpServers"];
  readOnly: boolean;
  initialized?: InitializeResponse;
  authStatus?: AgentAuthStatus;
  pendingAuth?: PendingAuthOperation;
  authTerminal?: AuthTerminalState;
  authError?: string;
  lastAuthResponse?: AgentAuthResponse;
  session?: NewSessionResponse;
  historyStatus?: HistoryStatus;
  historyNotice?: string;
  cachedSessions: Map<string, ActiveSessionSnapshot>;
  attentionSessionIds: string[];
  pendingSessionId?: string;
  sessionTransition?: SessionTransition;
  pendingSessionDeletions: PendingSessionDeletion[];
  pendingSessionControl?: PendingSessionControl;
  pendingPrompt?: PendingPrompt;
  runtimeOperation?: { requestId: string; operation: SessionRuntimeOperation };
  sessions: SessionInfo[];
  nextSessionCursor?: string | null;
  availableCommands: AvailableCommand[];
  modeId?: string;
  configOptions: SessionConfigOption[] | null;
  timeline: TimelineItem[];
  terminalSnapshots: TerminalSnapshot[];
  permissions: PendingPermission[];
  elicitations: PendingElicitation[];
  externalFlows: ExternalElicitationFlow[];
  backgroundEvents: unknown[];
  running: boolean;
  sessionSyncPhase?: SessionSyncPhase;
  agentActivity?: AgentActivity;
  title?: string;
  usage?: { used: number; size: number; cost?: { amount: number; currency: string } | null };
  activePlan?: Extract<TimelineItem, { type: "plan" }>;
}

export type AppAction =
  | { type: "bridge/connection_replaced" }
  | { type: "session/retired"; owner: SessionOwner; deleted: boolean }
  | { type: "session/management_complete"; sessionId: string; requestId: string; owner?: SessionOwner; deleted: boolean }
  | { type: "socket/open" }
  | { type: "socket/closed" }
  | { type: "bridge/session_hydrate"; view: BridgeSessionView; failedPrompt?: { requestId: string; message: string } }
  | { type: "bridge/session_live_update"; view: BridgeSessionView; settledControl?: SessionControlKind }
  | { type: "session/control_settled"; requestId: string }
  | { type: "session/release_turn_process"; process: DeferredTurnProcess }
  | {
      type: "bridge/turn_complete";
      event: Extract<SessionBusinessEvent, { type: "bridge/session_turn_complete" }>;
    }
  | {
      type: "bridge/turn_failed";
      event: Extract<SessionBusinessEvent, { type: "bridge/session_turn_failed" }>;
    }
  | { type: "runtime/loaded"; runtime: RuntimeView }
  | { type: "global/event"; event: GlobalBusinessEvent }
  | { type: "request/failed"; error: BridgeError }
  | { type: "sessions/listed"; cursor?: string; response: SessionListResult }
  | { type: "session/turn_update"; notification: SessionNotification }
  | { type: "session/terminal_update"; terminal: TerminalSnapshot }
  | { type: "client/error"; message: string }
  | {
      type: "history/unavailable";
      sessionId?: string;
      reason: Extract<HistoryStatus, { state: "unavailable" }>["reason"];
    }
  | { type: "permission/respond_start"; permissionId: string; requestId: string }
  | { type: "elicitation/respond_start"; elicitationId: string; requestId: string }
  | {
      type: "auth/start";
      requestId: string;
      kind: PendingAuthOperation["kind"];
      methodId?: string;
    }
  | { type: "auth/dismiss_terminal" }
  | {
      type: "user/prompt";
      requestId: string;
      sessionId: string;
      blocks: ContentBlock[];
    }
  | { type: "elicitation/dismiss_flow"; elicitationId: string }
  | {
      type: "session/transition_start";
      kind: SessionTransition["kind"];
      requestId: string;
      sessionId?: string;
      cwd?: string;
      title?: string | null;
    }
  | {
      type: "session/delete_start";
      requestId: string;
      sessionId: string;
      stage: PendingSessionDeletion["stage"];
    }
  | {
      type: "session/delete_continue";
      closeRequestId: string;
      requestId: string;
      sessionId: string;
    }
  | { type: "session/delete_cancel"; requestId: string; sessionId: string }
  | {
      type: "session/control_start";
      requestId: string;
      sessionId: string;
      kind: PendingSessionControl["kind"];
    }
  | { type: "session/reset" }
  | { type: "session/deselect" }
  | { type: "session/activate_cached"; sessionId: string };

export const initialState: AppState = {
  phase: "starting",
  socketOpen: false,
  transport: "stdio",
  command: [],
  defaultCwd: "",
  cwd: "",
  additionalDirectories: [],
  mcpServers: [],
  readOnly: false,
  cachedSessions: new Map(),
  attentionSessionIds: [],
  sessions: [],
  pendingSessionDeletions: [],
  availableCommands: [],
  configOptions: null,
  timeline: [],
  terminalSnapshots: [],
  permissions: [],
  elicitations: [],
  externalFlows: [],
  backgroundEvents: [],
  running: false,
};

function historyUnavailableMessage(
  reason: Extract<HistoryStatus, { state: "unavailable" }>["reason"],
): string {
  switch (reason) {
    case "load_not_supported":
      return "History unavailable: this Agent does not support loading saved session history.";
    default:
      return assertNever(reason, "history unavailable reason");
  }
}

export function appReducer(state: AppState, action: AppAction): AppState {
  switch (action.type) {
    case "bridge/connection_replaced":
      return { ...terminateBridgeState(state, "stopped", false), sessionSyncPhase: "loading" };
    case "session/retired":
      return removeAuthoritativeSessionRuntime(state, action.owner.sessionId, action.deleted, action.owner);
    case "session/management_complete": {
      const next = action.owner == null ? state : removeAuthoritativeSessionRuntime(
        state, action.sessionId, action.deleted, action.owner,
      );
      return {
        ...next,
        sessions: action.deleted && action.owner == null
          ? next.sessions.filter(({ sessionId }) => sessionId !== action.sessionId)
          : next.sessions,
        pendingSessionDeletions: next.pendingSessionDeletions.filter(({ requestId }) => requestId !== action.requestId),
      };
    }
    case "socket/open":
      return { ...state, socketOpen: true };
    case "socket/closed":
      return terminateBridgeState(state, "stopped", false);
    case "bridge/session_live_update":
      return applySessionLiveView(state, action.view, action.settledControl);
    case "session/control_settled":
      return state.pendingSessionControl?.requestId === action.requestId
        ? { ...state, pendingSessionControl: undefined }
        : state;
    case "bridge/session_hydrate": {
      const next = hydrateBridgeSession(state, action.view);
      const failure = action.failedPrompt;
      if (failure == null || action.view.phase !== "ready" || action.view.activeTurn != null) return next;
      const snapshot = sameSessionOwner(state.sessionOwner, action.view)
        ? state : state.cachedSessions.get(action.view.sessionId);
      if (!sameSessionOwner(snapshot?.sessionOwner, action.view)) return next;
      const pending = snapshot?.pendingPrompt;
      const previousFailure = snapshot?.timeline.find((item) =>
        item.type === "error" && item.requestId === failure.requestId && item.retryBlocks != null,
      );
      const blocks = pending?.requestId === failure.requestId ? pending.blocks
        : previousFailure?.type === "error" ? previousFailure.retryBlocks : undefined;
      if (blocks == null) return next;
      // The authoritative read confirmed no admission. Reuse the existing input
      // for its visible question and explicit retry, without another body copy.
      return {
        ...next,
        timeline: [...next.timeline, {
          id: randomId(), type: "message", role: "user", blocks, raw: [],
        }, errorTimelineItem({
          operation: "session/prompt", requestId: failure.requestId, message: failure.message,
        }, blocks)],
      };
    }
    case "session/release_turn_process": {
      if (!sameSessionOwner(state.sessionOwner, action.process.owner)) return state;
      const timeline = releaseTimelineProcess(state.timeline, action.process);
      if (timeline === state.timeline) return state;
      return { ...state, timeline, terminalSnapshots: releaseUnreferencedTerminals(state.terminalSnapshots,
        state.timeline, [timeline, state.permissions, state.elicitations, state.activePlan]),
        sessionTransition: state.sessionTransition == null ? undefined : {
          ...state.sessionTransition, backup: compactSessionSnapshot(state.sessionTransition.backup),
        },
      };
    }
    case "bridge/turn_complete":
      return applyBridgeTurnComplete(state, action.event);
    case "bridge/turn_failed":
      return applyBridgeTurnFailure(state, action.event);
    case "runtime/loaded":
      return applyRuntime(state, action.runtime);
    case "global/event":
      return applyGlobalEvent(state, action.event);
    case "request/failed":
      return applyBridgeError(state, action.error);
    case "sessions/listed":
      return applySessionsListed(state, action.cursor, action.response);
    case "session/turn_update":
      return isCurrentSession(state, action.notification.sessionId)
        ? reduceSessionUpdate(state, action.notification)
        : updateCachedSession(
            state,
            action.notification.sessionId,
            (cached) => reduceSessionUpdate(cached, action.notification),
            action,
          );
    case "session/terminal_update":
      return isCurrentSession(state, action.terminal.sessionId)
        ? { ...state, terminalSnapshots: upsertTerminalSnapshot(state.terminalSnapshots, action.terminal) }
        : updateCachedSession(
            state,
            action.terminal.sessionId,
            (cached) => ({
              ...cached,
              terminalSnapshots: upsertTerminalSnapshot(cached.terminalSnapshots, action.terminal),
            }),
            action,
          );
    case "client/error":
      return {
        ...state,
        timeline: [
          ...state.timeline,
          { id: randomId(), type: "error", message: action.message },
        ],
      };
    case "history/unavailable": {
      const message = historyUnavailableMessage(action.reason);
      const status: HistoryStatus = {
        state: "unavailable",
        sessionId: action.sessionId,
        reason: action.reason,
        message,
      };
      return {
        ...state,
        historyStatus: status,
        timeline: [
          ...state.timeline.filter((item) =>
            item.type !== "error" || item.message !== message
          ),
          {
            id: randomId(),
            type: "error",
            message,
            operation: "session/load",
          },
        ],
      };
    }
    case "permission/respond_start":
      return {
        ...state,
        permissions: state.permissions.map((pending) =>
          pending.permissionId === action.permissionId && pending.responseRequestId == null
            ? { ...pending, responseRequestId: action.requestId, responseError: undefined }
            : pending
        ),
      };
    case "elicitation/respond_start":
      return {
        ...state,
        elicitations: state.elicitations.map((pending) =>
          pending.elicitationId === action.elicitationId && pending.responseRequestId == null
            ? { ...pending, responseRequestId: action.requestId, responseError: undefined }
            : pending
        ),
      };
    case "auth/start":
      if (state.pendingAuth != null) return state;
      return {
        ...state,
        pendingAuth: {
          requestId: action.requestId,
          kind: action.kind,
          methodId: action.methodId,
        },
        authTerminal: action.kind === "terminal" && action.methodId != null
          ? {
              requestId: action.requestId,
              methodId: action.methodId,
              status: "starting",
              output: "",
              truncated: false,
            }
          : state.authTerminal,
        authError: undefined,
      };
    case "auth/dismiss_terminal":
      if (
        state.authTerminal?.status === "starting" ||
        state.authTerminal?.status === "running"
      ) return state;
      return { ...state, authTerminal: undefined };
    case "user/prompt":
      return startPrompt(state, action.requestId, action.sessionId, action.blocks);
    case "elicitation/dismiss_flow":
      return {
        ...state,
        externalFlows: state.externalFlows.filter(
          ({ elicitationId }) => elicitationId !== action.elicitationId,
        ),
      };
    case "session/transition_start":
      if (
        state.sessionTransition ||
        state.pendingSessionControl ||
        state.runtimeOperation
      ) return state;
      if (
        (action.kind === "fork" || action.kind === "close") &&
        (
          state.session == null ||
          action.sessionId !== state.session.sessionId ||
          (action.kind === "fork" && (state.running || state.pendingPrompt != null)) ||
          state.runtimeOperation != null
        )
      ) return state;
      const transition: SessionTransition = {
        kind: action.kind,
        requestId: action.requestId,
        targetSessionId: action.sessionId,
        targetCwd: action.cwd,
        backup: captureActiveSession(
          state.session == null ? resetActiveSession(state) : state,
        ),
      };
      const stateWithCachedCurrent = cacheCurrentSession(state);
      if (action.kind === "fork" || action.kind === "close") {
        return { ...stateWithCachedCurrent, sessionTransition: transition };
      }
      if (action.kind === "attach" && action.sessionId != null) {
        return {
          ...stateWithCachedCurrent,
          pendingSessionId: action.sessionId,
          sessionTransition: transition,
          historyStatus: { state: "loading", sessionId: action.sessionId, requestId: action.requestId },
        };
      }
      return {
        ...resetActiveSession(stateWithCachedCurrent, action.title ?? undefined),
        cwd: action.cwd ?? state.defaultCwd,
        pendingSessionId: action.sessionId,
        sessionTransition: transition,
      };
    case "session/delete_start":
      if (state.pendingSessionDeletions.some(
        ({ requestId, sessionId }) =>
          requestId === action.requestId || sessionId === action.sessionId,
      )) return state;
      {
        return {
          ...state,
          pendingSessionDeletions: [
            ...state.pendingSessionDeletions,
            {
              requestId: action.requestId,
              sessionId: action.sessionId,
              stage: action.stage,
            },
          ],
        };
      }
    case "session/delete_continue":
      if (!state.pendingSessionDeletions.some(
        ({ requestId, sessionId, stage }) =>
          requestId === action.closeRequestId &&
          sessionId === action.sessionId &&
          stage === "closing",
      )) return state;
      return {
        ...state,
        pendingSessionDeletions: state.pendingSessionDeletions.map((pending) =>
          pending.requestId === action.closeRequestId &&
            pending.sessionId === action.sessionId
            ? { ...pending, requestId: action.requestId, stage: "deleting" }
            : pending
        ),
      };
    case "session/delete_cancel":
      return {
        ...state,
        pendingSessionDeletions: state.pendingSessionDeletions.filter(
          ({ requestId, sessionId }) =>
            requestId !== action.requestId || sessionId !== action.sessionId,
        ),
      };
    case "session/control_start":
      if (
        state.pendingSessionControl != null ||
        state.sessionTransition != null ||
        state.runtimeOperation != null ||
        state.session?.sessionId !== action.sessionId
      ) return state;
      return {
        ...state,
        pendingSessionControl: {
          requestId: action.requestId,
          sessionId: action.sessionId,
          kind: action.kind,
        },
      };
    case "session/reset":
      return resetActiveSession(state);
    case "session/deselect":
      return resetActiveSession(cacheCurrentSession(state));
    case "session/activate_cached":
      return activateCachedSession(state, action.sessionId);
  }
}

function applyRuntime(state: AppState, runtime: RuntimeView): AppState {
  let next = state;
  if (runtime.hello != null) next = applyHello(next, runtime.hello);
  if (runtime.initialized != null) next = applyInitialized(next, runtime.initialized.response);
  if (runtime.error != null) next = applyBridgeError(next, runtime.error);
  if (runtime.phase != null) next = applyConnectionPhase(next, runtime.phase.phase);
  return next;
}

function applyHello(state: AppState, hello: RuntimeHello): AppState {
  return {
    ...state,
    transport: hello.transport,
    command: hello.command,
    defaultCwd: hello.cwd,
    cwd: state.session ? state.cwd : hello.cwd,
    readOnly: hello.readOnly,
    additionalDirectories: hello.additionalDirectories,
    mcpServers: hello.mcpServers,
  };
}

function applyConnectionPhase(state: AppState, phase: ConnectionPhase): AppState {
  return phase === "error" || phase === "stopped"
    ? terminateBridgeState(state, phase, state.socketOpen)
    : { ...state, phase };
}

function applyInitialized(state: AppState, response: InitializeResponse): AppState {
  const preserveRuntimeAuth = state.pendingAuth != null ||
    state.authTerminal != null ||
    state.authStatus === "authenticated" ||
    state.authStatus === "logged_out";
  return {
    ...state,
    initialized: response,
    authStatus: preserveRuntimeAuth
      ? state.authStatus
      : (response.authMethods?.length ?? 0) > 0 || response.agentCapabilities?.auth?.logout != null
        ? "available"
        : undefined,
    pendingAuth: preserveRuntimeAuth ? state.pendingAuth : undefined,
    authTerminal: preserveRuntimeAuth ? state.authTerminal : undefined,
    authError: preserveRuntimeAuth ? state.authError : undefined,
    lastAuthResponse: preserveRuntimeAuth ? state.lastAuthResponse : undefined,
  };
}

function applyGlobalEvent(state: AppState, event: GlobalBusinessEvent): AppState {
  switch (event.type) {
    case "bridge/connection":
      return applyConnectionPhase(state, event.phase);
    case "bridge/connection_error":
      return applyBridgeError(state, { message: event.message, code: event.code, data: event.data });
    case "bridge/catalog_changed":
      // useAcp refreshes the Agent-owned catalog; nothing is projected here.
      return state;
    case "acp/authenticated":
      if (
        state.pendingAuth != null &&
        (
          state.pendingAuth.kind !== "authenticate" ||
          state.pendingAuth.requestId !== event.requestId ||
          state.pendingAuth.methodId !== event.methodId
        )
      ) return appendBackgroundEvent(state, event);
      return {
        ...state,
        authStatus: "authenticated",
        pendingAuth: undefined,
        authError: undefined,
        lastAuthResponse: { kind: "authenticate", response: event.response },
      };
    case "bridge/auth_terminal_started":
      if (state.pendingAuth == null) {
        return {
          ...state,
          authStatus: "required",
          pendingAuth: {
            kind: "terminal",
            requestId: event.requestId,
            methodId: event.methodId,
          },
          authTerminal: {
            requestId: event.requestId,
            methodId: event.methodId,
            status: "running",
            output: "",
            truncated: false,
          },
          authError: undefined,
        };
      }
      if (
        state.pendingAuth?.kind !== "terminal" ||
        state.pendingAuth.requestId !== event.requestId ||
        state.pendingAuth.methodId !== event.methodId ||
        state.authTerminal?.requestId !== event.requestId
      ) return appendBackgroundEvent(state, event);
      return {
        ...state,
        authTerminal: { ...state.authTerminal, status: "running" },
      };
    case "bridge/auth_terminal_output":
      if (state.authTerminal?.requestId !== event.requestId) {
        return appendBackgroundEvent(state, event);
      }
      return appendAuthTerminalOutput(state, event.data);
    case "bridge/auth_terminal_exited":
      if (
        state.pendingAuth?.kind !== "terminal" ||
        state.pendingAuth.requestId !== event.requestId ||
        state.pendingAuth.methodId !== event.methodId ||
        state.authTerminal?.requestId !== event.requestId
      ) return appendBackgroundEvent(state, event);
      return {
        ...state,
        authStatus: event.status === "succeeded" ? "authenticated" : "required",
        pendingAuth: undefined,
        authError: event.status === "failed" ? event.message ?? "Terminal sign-in failed" : undefined,
        authTerminal: {
          ...state.authTerminal,
          status: event.status,
          exitCode: event.exitCode,
          signal: event.signal,
          message: event.message,
        },
      };
    case "acp/logged_out":
      if (
        state.pendingAuth != null &&
        (
          state.pendingAuth.kind !== "logout" ||
          state.pendingAuth.requestId !== event.requestId
        )
      ) return appendBackgroundEvent(state, event);
      return {
        ...state,
        authStatus: "logged_out",
        pendingAuth: undefined,
        authError: undefined,
        lastAuthResponse: { kind: "logout", response: event.response },
      };
  }
  return assertNever(event, "global business event");
}

function applySessionsListed(
  state: AppState,
  cursor: string | undefined,
  response: SessionListResult,
): AppState {
  const listedActive = state.session == null
    ? undefined
    : response.sessions.find(
        ({ sessionId }) => sessionId === state.session?.sessionId,
      );
  return {
    ...state,
    sessions: cursor
      ? mergeSessions(state.sessions, response.sessions)
      : mergeSessions(
          state.sessions.filter(({ sessionId }) => state.cachedSessions.has(sessionId)),
          response.sessions,
        ),
    nextSessionCursor: response.nextCursor,
    title: state.title ?? listedActive?.title ?? undefined,
  };
}

function applyBridgeError(state: AppState, event: BridgeError): AppState {
  state = clearRuntimeOperation(state, event.requestId);
  if (
    event.requestId != null &&
    state.pendingAuth?.requestId === event.requestId
  ) {
    if (
      state.pendingAuth.kind === "terminal" &&
      event.operation !== "auth/terminal_start"
    ) {
      return {
        ...state,
        authError: event.message,
        authTerminal: state.authTerminal == null
          ? undefined
          : { ...state.authTerminal, message: event.message },
      };
    }
    const terminal = state.pendingAuth.kind === "terminal" && state.authTerminal
      ? {
          ...state.authTerminal,
          status: "failed" as const,
          message: event.message,
        }
      : state.authTerminal;
    return {
      ...state,
      pendingAuth: undefined,
      authTerminal: terminal,
      authStatus: state.pendingAuth.kind === "authenticate" || state.pendingAuth.kind === "terminal"
        ? "required"
        : state.authStatus,
      authError: event.message,
    };
  }
  if (event.requestId != null && event.operation === "permission/respond") {
    const pending = state.permissions.find(
      ({ responseRequestId }) => responseRequestId === event.requestId,
    );
    if (pending) {
      return failPermissionResponse(state, pending.permissionId, event.message);
    }
    for (const [sessionId, snapshot] of state.cachedSessions) {
      const cached = snapshot.permissions.find(
        ({ responseRequestId }) => responseRequestId === event.requestId,
      );
      if (!cached) continue;
      return updateCachedSession(
        state,
        sessionId,
        (cachedState) => failPermissionResponse(
          cachedState,
          cached.permissionId,
          event.message,
        ),
        event,
      );
    }
    return appendBackgroundEvent(state, event);
  }
  if (event.requestId != null && event.operation === "elicitation/respond") {
    const pending = state.elicitations.find(
      ({ responseRequestId }) => responseRequestId === event.requestId,
    );
    if (pending) {
      return failElicitationResponse(state, pending.elicitationId, event.message);
    }
    for (const [sessionId, snapshot] of state.cachedSessions) {
      const cached = snapshot.elicitations.find(
        ({ responseRequestId }) => responseRequestId === event.requestId,
      );
      if (!cached) continue;
      return updateCachedSession(
        state,
        sessionId,
        (cachedState) => failElicitationResponse(
          cachedState,
          cached.elicitationId,
          event.message,
        ),
        event,
      );
    }
    return appendBackgroundEvent(state, event);
  }
  if (isAuthenticationRequired(state, event)) {
    return settleAuthenticationRequired(state, event.requestId);
  }
  if (
    event.requestId != null &&
    event.requestId === state.sessionTransition?.requestId
  ) {
    const restored = rollbackSessionTransition(state);
    const error = errorTimelineItem(event);
    return {
      ...restored,
      timeline: [
        ...restored.timeline.filter((item) =>
          item.type !== "error" ||
          item.message !== error.message ||
          item.operation !== error.operation ||
          item.code !== error.code
        ),
        error,
      ],
    };
  }
  if (event.requestId != null) {
    if (
      event.operation === "session/prompt" &&
      state.pendingPrompt?.requestId === event.requestId
    ) {
      return failPrompt(state, event);
    }
    if (event.operation === "session/prompt") {
      for (const [sessionId, snapshot] of state.cachedSessions) {
        if (snapshot.pendingPrompt?.requestId !== event.requestId) continue;
        return updateCachedSession(
          state,
          sessionId,
          (cached) => failPrompt(cached, event),
          event,
        );
      }
    }
    if (state.pendingSessionControl?.requestId === event.requestId) {
      return {
        ...state,
        pendingSessionControl: undefined,
        timeline: [
          ...state.timeline,
          errorTimelineItem(event),
        ],
      };
    }
    const pendingDeletion = state.pendingSessionDeletions.find(
      ({ requestId }) => requestId === event.requestId,
    );
    if (pendingDeletion) {
      return {
        ...state,
        pendingSessionDeletions: state.pendingSessionDeletions.filter(
          ({ requestId }) => requestId !== event.requestId,
        ),
        timeline: [
          ...state.timeline,
          errorTimelineItem(event),
        ],
      };
    }
  }
  return {
    ...state,
    timeline: [
      ...state.timeline,
      errorTimelineItem(event),
    ],
  };
}

function startPrompt(
  state: AppState,
  requestId: string,
  sessionId: string,
  blocks: ContentBlock[],
): AppState {
  if (state.pendingPrompt?.requestId === requestId) return state;
  if (
    state.session?.sessionId !== sessionId ||
    state.running ||
    state.pendingPrompt != null ||
    state.sessionTransition != null ||
    state.pendingSessionControl != null ||
    state.runtimeOperation != null
  ) return state;
  state = archiveActivePlan(state);
  return {
    ...state,
    running: true,
    agentActivity: { kind: "waiting" },
    pendingPrompt: { requestId, sessionId, blocks },
    timeline: [
      ...state.timeline,
      {
        id: randomId(),
        type: "message",
        role: "user",
        blocks,
        raw: [],
      },
    ],
  };
}

function failPrompt(
  state: AppState,
  event: BridgeError,
): AppState {
  const pendingPrompt = state.pendingPrompt;
  if (pendingPrompt == null || pendingPrompt.requestId !== event.requestId) return state;
  const retryBlocks = pendingPrompt.blocks;
  state = archiveActivePlan(state);
  return {
    ...state,
    running: false,
    agentActivity: undefined,
    pendingPrompt: undefined,
    timeline: [
      ...state.timeline,
      errorTimelineItem(event, retryBlocks),
    ],
  };
}

function reduceSessionUpdate(
  state: AppState,
  notification: SessionNotification,
): AppState {
  const update = notification.update;

  if (
    update.sessionUpdate === "agent_message_chunk" ||
    update.sessionUpdate === "agent_thought_chunk" ||
    update.sessionUpdate === "user_message_chunk"
  ) {
    const role =
      update.sessionUpdate === "agent_message_chunk"
        ? "agent"
        : update.sessionUpdate === "agent_thought_chunk"
          ? "thought"
          : "protocol-user";
    const messageId =
      "messageId" in update
        ? update.messageId
        : undefined;
    const turnOperationId = role === "protocol-user"
      ? bridgeTurnOperationId(update)
      : undefined;
    const timeline = appendContent(
      state.timeline,
      role,
      update.content,
      messageId,
      turnOperationId,
      notification,
    );
    const activeChunk = role === "thought"
      ? lastAssistantChunk(timeline, "thought")
      : undefined;
    return {
      ...state,
      timeline,
      agentActivity: !state.running
        ? state.agentActivity
        : role === "thought" && activeChunk
          ? { kind: "thinking", timelineId: activeChunk.id }
          : role === "agent"
            ? { kind: "responding" }
            : state.agentActivity,
    };
  }

  if (update.sessionUpdate === "tool_call") {
    const { timeline, call } = upsertToolCall(
      state.timeline,
      update,
      notification,
      "create",
    );
    return {
      ...state,
      timeline: state.pendingPrompt?.cancelRequested ? cancelTurnTools(timeline) : timeline,
      agentActivity: state.running
        ? toolActivity(call)
        : state.agentActivity,
    };
  }

  if (update.sessionUpdate === "tool_call_update") {
    const { timeline, call } = upsertToolCall(
      state.timeline,
      update,
      notification,
      "update",
    );
    return {
      ...state,
      timeline: state.pendingPrompt?.cancelRequested ? cancelTurnTools(timeline) : timeline,
      agentActivity: state.running
        ? toolActivity(call)
        : state.agentActivity,
    };
  }

  if (update.sessionUpdate === "plan") {
    // Legacy plans have no ID: keep one latest snapshot per turn. Historical
    // replay never creates a live composer card, even for unfinished entries.
    const turnStart = timelineTurnStarts(state.timeline).at(-1) ?? 0;
    const index = state.timeline.findIndex((item, index) => index >= turnStart &&
      item.historyTurnId == null && item.type === "plan" && item.update.sessionUpdate === "plan");
    const previous = state.activePlan ?? state.timeline[index];
    const item: Extract<TimelineItem, { type: "plan" }> = {
      id: previous?.id ?? randomId(),
      type: "plan",
      update,
      raw: [notification],
    };
    const active = state.running && state.sessionSyncPhase !== "reconciling" &&
      update.entries.some(({ status }) => status !== "completed");
    const timeline = [...state.timeline];
    if (active || update.entries.length === 0) {
      if (index >= 0) timeline.splice(index, 1);
    } else if (index >= 0) {
      timeline[index] = item;
    } else {
      timeline.push(item);
    }
    return {
      ...state,
      activePlan: active ? item : undefined,
      timeline,
      agentActivity: active
        ? { kind: "planning" }
        : state.agentActivity?.kind === "planning" ? { kind: "waiting" } : state.agentActivity,
    };
  }

  if (update.sessionUpdate === "plan_update") {
    const planId = update.plan.planId;
    const baseId = `plan:${planId}`;
    const index = state.timeline.findIndex((item) => item.historyTurnId == null && item.type === "plan" && (
      item.update.sessionUpdate === "plan_update" ? item.update.plan.planId === planId :
        item.update.sessionUpdate === "plan_removed" && item.update.planId === planId
    ));
    const current = index >= 0 ? state.timeline[index] : undefined;
    const id = current?.id ?? (state.timeline.some((item) => item.id === baseId) ? `${baseId}:${state.timeline.length}` : baseId);
    const item: TimelineItem = {
      id,
      type: "plan",
      update,
      raw: [notification],
    };
    const timeline = [...state.timeline];
    if (index >= 0) timeline[index] = item;
    else timeline.push(item);
    return {
      ...state,
      timeline,
      agentActivity: state.running ? { kind: "planning" } : state.agentActivity,
    };
  }

  if (update.sessionUpdate === "plan_removed") {
    const index = state.timeline.findIndex((item) => item.historyTurnId == null && item.type === "plan" && (
      item.update.sessionUpdate === "plan_update" ? item.update.plan.planId === update.planId :
        item.update.sessionUpdate === "plan_removed" && item.update.planId === update.planId
    ));
    const current = index >= 0 ? state.timeline[index] : undefined;
    if (current?.type !== "plan") {
      return {
        ...state,
        timeline: [
          ...state.timeline,
          { id: randomId(), type: "protocol", notification },
        ],
      };
    }
    const removed: TimelineItem = {
      id: current.id,
      type: "plan",
      update,
      raw: [notification],
    };
    const timeline = [...state.timeline];
    timeline[index] = removed;
    return {
      ...state,
      timeline,
      agentActivity: state.running ? { kind: "planning" } : state.agentActivity,
    };
  }

  if (
    update.sessionUpdate === "compaction_update" ||
    update.sessionUpdate === "compaction_summary_chunk"
  ) {
    const next = reduceCompactionUpdate(state, notification);
    return {
      ...next,
      agentActivity: state.running
        ? update.sessionUpdate === "compaction_update" && update.status !== "in_progress"
          ? { kind: "waiting" }
          : { kind: "compacting" }
        : state.agentActivity,
    };
  }

  if (update.sessionUpdate === "current_mode_update") {
    return { ...state, modeId: update.currentModeId };
  }
  if (update.sessionUpdate === "config_option_update") {
    return { ...state, configOptions: update.configOptions };
  }
  if (update.sessionUpdate === "available_commands_update") {
    return { ...state, availableCommands: update.availableCommands };
  }
  if (update.sessionUpdate === "session_info_update") {
    const title = update.title === undefined
      ? state.title
      : update.title ?? undefined;
    const sessions = state.sessions.map((session) => {
      if (session.sessionId !== notification.sessionId) return session;
      return {
        ...session,
        ...(update.title !== undefined ? { title: update.title } : {}),
        ...(update.updatedAt !== undefined ? { updatedAt: update.updatedAt } : {}),
      };
    });
    return { ...state, title, sessions };
  }
  if (update.sessionUpdate === "usage_update") {
    return {
      ...state,
      usage: {
        used: update.used,
        size: update.size,
        // Cost is cumulative and optional. Match Zed's patch semantics:
        // omission/null updates context usage without erasing known cost.
        cost: update.cost ?? state.usage?.cost,
      },
    };
  }

  const unhandled: never = update;
  return {
    ...state,
    timeline: [
      ...state.timeline,
      {
        id: randomId(),
        type: "protocol",
        notification: {
          ...notification,
          update: assertNever(unhandled, "ACP session update"),
        },
      },
    ],
  };
}

function upsertToolCall(
  timeline: TimelineItem[],
  update: ToolCall | ToolCallUpdate,
  raw: unknown,
  source: "create" | "update" | "permission",
): { timeline: TimelineItem[]; call: ToolCall } {
  const turnStart = source === "create" ? timelineTurnStarts(timeline).at(-1) ?? 0 : 0;
  let index = -1;
  for (let position = timeline.length - 1; position >= turnStart; position--) {
    const item = timeline[position];
    if (item.historyTurnId == null && item.type === "tool" && item.call.toolCallId === update.toolCallId) {
      index = position;
      break;
    }
  }
  const current = index >= 0 ? timeline[index] : undefined;
  const previous = current?.type === "tool" ? current : undefined;
  const id = previous?.id ?? `tool:${update.toolCallId}:${timeline.length}`;

  let call: ToolCall;
  if (!previous && source === "update") {
    // This mirrors Zed's recovery path: keep the stream usable and make the
    // broken reference visible as a failed tool card instead of turning it
    // into a session-wide protocol error.
    call = {
      toolCallId: update.toolCallId,
      title: "Tool call not found",
      kind: "other",
      status: "failed",
      content: [{
        type: "content",
        content: { type: "text", text: "The Agent updated a tool call that was not introduced earlier." },
      }],
    };
  } else {
    const patch = withoutNullish(update);
    call = {
      ...(previous?.call ?? {}),
      ...patch,
      toolCallId: update.toolCallId,
      title: patch.title ?? previous?.call.title ?? (
        source === "permission" ? "Tool awaiting permission" : "Tool"
      ),
      ...(source === "permission" && patch.status == null && previous?.call.status == null
        ? { status: "pending" as const }
        : {}),
    } as ToolCall;
  }

  const item: TimelineItem = {
    id,
    type: "tool",
    call,
    ...(previous?.cancelled && call.status !== "completed" && call.status !== "failed"
      ? { cancelled: true }
      : {}),
    raw: [raw],
  };
  const next = [...timeline];
  if (index >= 0) next[index] = item;
  else next.push(item);
  return { timeline: next, call };
}

function toolActivity(call: ToolCall): AgentActivity {
  return call.status === "pending" || call.status === "in_progress" || call.status == null
    ? { kind: "tool", toolCallId: call.toolCallId, title: call.title }
    : { kind: "waiting" };
}

function reduceCompactionUpdate(
  state: AppState,
  notification: SessionNotification,
): AppState {
  const update = notification.update;
  if (
    update.sessionUpdate !== "compaction_update" &&
    update.sessionUpdate !== "compaction_summary_chunk"
  ) {
    return state;
  }
  const baseId = `compaction:${update.compactionId}`;
  const index = state.timeline.findIndex((item) => item.historyTurnId == null && item.type === "compaction" &&
    item.compactionId === update.compactionId);
  const current = index >= 0 ? state.timeline[index] : undefined;
  const previous = current?.type === "compaction" ? current : undefined;
  const id = previous?.id ?? (state.timeline.some((item) => item.id === baseId) ? `${baseId}:${state.timeline.length}` : baseId);

  let blocks = previous?.blocks ?? [];
  let status = previous?.status ?? "in_progress";
  let error = previous?.error;
  if (update.sessionUpdate === "compaction_summary_chunk") {
    blocks = mergeTextBlock(blocks, update.content);
  } else {
    status = update.status;
    if (update.summary !== undefined) blocks = update.summary ?? [];
    if (update.error !== undefined) error = update.error ?? undefined;
  }

  const item: TimelineItem = {
    id,
    type: "compaction",
    compactionId: update.compactionId,
    status,
    blocks,
    error,
    raw: [notification],
  };
  const timeline = [...state.timeline];
  if (index >= 0) timeline[index] = item;
  else timeline.push(item);
  return { ...state, timeline };
}

function appendContent(
  timeline: TimelineItem[],
  role: "agent" | "thought" | "protocol-user",
  block: ContentBlock,
  messageId: string | null | undefined,
  turnOperationId: string | undefined,
  raw: unknown,
): TimelineItem[] {
  return role === "protocol-user"
    ? appendProtocolUserContent(timeline, block, messageId, turnOperationId, raw)
    : appendAssistantContent(timeline, role, block, messageId, raw);
}

function appendProtocolUserContent(
  timeline: TimelineItem[],
  block: ContentBlock,
  messageId: string | null | undefined,
  turnOperationId: string | undefined,
  raw: unknown,
): TimelineItem[] {
  // Compact history is an immutable presentation slice. An Agent may reuse
  // its protocol IDs in a later turn, including for the current prompt echo.
  const identified = messageId == null ? -1 : timeline.findIndex((item) =>
    item.historyTurnId == null && item.type === "message" && item.messageId === messageId &&
    canMergeTurnOperationIds(item.turnOperationId, turnOperationId)
  );
  const index = identified >= 0 ? identified : timeline.length - 1;
  const candidate = timeline[index];
  const last = candidate?.historyTurnId == null ? candidate : undefined;

  // Match Zed's optimistic prompt echo handling: an Agent may replay the
  // prompt it just received. Keep one user entry while retaining the raw ACP
  // notification and the Agent-owned protocol ID for inspection.
  if (
    last?.type === "message" &&
    last.role === "user" &&
    promptEchoRemainder(last.echoBlocks ?? last.blocks, block) != null &&
    canMergeMessageIds(last.messageId, messageId)
  ) {
    const next = [...timeline];
    next[index] = {
      ...last,
      messageId: last.messageId ?? messageId,
      turnOperationId: last.turnOperationId ?? turnOperationId,
      echoBlocks: promptEchoRemainder(last.echoBlocks ?? last.blocks, block)!,
      raw: [raw],
    };
    return next;
  }

  if (
    last?.type === "message" &&
    (last.role === "protocol-user" || identified >= 0) &&
    canMergeMessageIds(last.messageId, messageId) &&
    canMergeTurnOperationIds(last.turnOperationId, turnOperationId)
  ) {
    const next = [...timeline];
    next[index] = {
      ...last,
      messageId: last.messageId ?? messageId,
      turnOperationId: last.turnOperationId ?? turnOperationId,
      blocks: mergeTextBlock(last.blocks, block),
      raw: [raw],
    };
    return next;
  }

  return [
    ...timeline,
    {
      id: randomId(),
      type: "message",
      role: "protocol-user",
      blocks: [block],
      messageId,
      turnOperationId,
      raw: [raw],
    },
  ];
}

function appendAssistantContent(
  timeline: TimelineItem[],
  role: AssistantMessageChunk["role"],
  block: ContentBlock,
  messageId: string | null | undefined,
  raw: unknown,
): TimelineItem[] {
  if (messageId != null) {
    const index = timeline.findIndex((item) => item.historyTurnId == null && item.type === "assistant" &&
      item.chunks.some((chunk) => chunk.messageId === messageId && chunk.role === role)
    );
    const existing = timeline[index];
    if (existing?.type === "assistant") {
      const next = [...timeline];
      next[index] = {
        ...existing,
        chunks: existing.chunks.map((chunk) => chunk.messageId === messageId && chunk.role === role
          ? { ...chunk, blocks: mergeTextBlock(chunk.blocks, block), raw: [raw] }
          : chunk),
      };
      return next;
    }
  }
  const last = timeline.at(-1);
  if (last?.type !== "assistant" || last.historyTurnId != null) {
    return [
      ...timeline,
      {
        id: randomId(),
        type: "assistant",
        chunks: [{
          id: randomId(),
          role,
          blocks: [block],
          messageId,
          raw: [raw],
        }],
      },
    ];
  }

  const chunks = [...last.chunks];
  const previous = chunks.at(-1);
  if (
    previous?.role === role &&
    canMergeMessageIds(previous.messageId, messageId)
  ) {
    chunks[chunks.length - 1] = {
      ...previous,
      messageId: previous.messageId ?? messageId,
      blocks: mergeTextBlock(previous.blocks, block),
      raw: [raw],
    };
  } else {
    chunks.push({
      id: randomId(),
      role,
      blocks: [block],
      messageId,
      raw: [raw],
    });
  }

  const next = [...timeline];
  next[next.length - 1] = { ...last, chunks };
  return next;
}

function canMergeMessageIds(
  existing: string | null | undefined,
  incoming: string | null | undefined,
): boolean {
  return existing == null || incoming == null || existing === incoming;
}

function canMergeTurnOperationIds(
  existing: string | undefined,
  incoming: string | undefined,
): boolean {
  return existing == null && incoming == null || existing === incoming;
}

function bridgeTurnOperationId(update: SessionUpdate): string | undefined {
  const metadata = "_meta" in update ? update._meta : undefined;
  if (!isStateRecord(metadata) || !isStateRecord(metadata.attyd)) return undefined;
  return typeof metadata.attyd.turnOperationId === "string"
    ? metadata.attyd.turnOperationId
    : undefined;
}

function isStateRecord(value: unknown): value is Record<string, unknown> {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function lastAssistantChunk(
  timeline: TimelineItem[],
  role: AssistantMessageChunk["role"],
): AssistantMessageChunk | undefined {
  const last = timeline.at(-1);
  if (last?.type !== "assistant") return undefined;
  const chunk = last.chunks.at(-1);
  return chunk?.role === role ? chunk : undefined;
}

function archiveActivePlan(state: AppState): AppState {
  if (state.activePlan == null) return state;
  return {
    ...state,
    activePlan: undefined,
    timeline: [...state.timeline, state.activePlan],
  };
}

function contentBlockEqual(left: ContentBlock, right: ContentBlock): boolean {
  try {
    return JSON.stringify(left) === JSON.stringify(right);
  } catch {
    return left === right;
  }
}

function mergeTextBlock(blocks: ContentBlock[], incoming: ContentBlock): ContentBlock[] {
  const last = blocks.at(-1);
  if (last?.type === "text" && incoming.type === "text" &&
    contentBlockEqual({ ...last, text: "" }, { ...incoming, text: "" })) {
    return [...blocks.slice(0, -1), { ...last, text: last.text + incoming.text }];
  }
  return [...blocks, incoming];
}

function promptEchoRemainder(blocks: ContentBlock[], incoming: ContentBlock): ContentBlock[] | undefined {
  const first = blocks[0];
  if (first == null) return undefined;
  if (contentBlockEqual(first, incoming)) return blocks.slice(1);
  if (first.type === "text" && incoming.type === "text" && incoming.text.length > 0 &&
    first.text.startsWith(incoming.text) &&
    contentBlockEqual({ ...first, text: "" }, { ...incoming, text: "" })) {
    return [{ ...first, text: first.text.slice(incoming.text.length) }, ...blocks.slice(1)];
  }
  return undefined;
}

function finishTurnTools(timeline: TimelineItem[], response: PromptResponse): TimelineItem[] {
  return response.stopReason === "cancelled" ? cancelTurnTools(timeline) : timeline;
}

function cancelTurnTools(timeline: TimelineItem[]): TimelineItem[] {
  const start = timelineTurnStarts(timeline).at(-1) ?? 0;
  return timeline.map((item, index) => index >= start && item.type === "tool" &&
    (item.call.status == null || item.call.status === "pending" || item.call.status === "in_progress")
    ? { ...item, cancelled: true }
    : item);
}

function withoutNullish<T extends object>(value: T): Partial<T> {
  return Object.fromEntries(
    Object.entries(value).filter(([, item]) => item != null),
  ) as Partial<T>;
}

function terminateBridgeState(
  state: AppState,
  phase: Extract<ConnectionPhase, "error" | "stopped">,
  socketOpen: boolean,
): AppState {
  const current = archiveActivePlan(rollbackSessionTransition(state));
  return {
    ...current,
    phase,
    socketOpen,
    cachedSessions: new Map(),
    attentionSessionIds: [],
    running: false,
    agentActivity: undefined,
    permissions: [],
    elicitations: [],
    externalFlows: [],
    pendingSessionDeletions: [],
    pendingSessionControl: undefined,
    pendingAuth: undefined,
    authTerminal: current.authTerminal == null
      ? undefined
      : {
          ...current.authTerminal,
          status: current.authTerminal.status === "starting" || current.authTerminal.status === "running"
            ? "cancelled"
            : current.authTerminal.status,
        },
    pendingPrompt: undefined,
    runtimeOperation: undefined,
    terminalSnapshots: current.terminalSnapshots.map((terminal) => ({
      ...terminal,
      released: true,
    })),
  };
}

function appendAuthTerminalOutput(state: AppState, data: string): AppState {
  if (!state.authTerminal) return state;
  const output = state.authTerminal.output + data;
  const truncated = state.authTerminal.truncated;
  return {
    ...state,
    authTerminal: {
      ...state.authTerminal,
      output,
      truncated,
    },
  };
}

function isAuthenticationRequired(
  state: AppState,
  event: BridgeError,
): boolean {
  return event.code === -32_000 && (state.initialized?.authMethods?.length ?? 0) > 0;
}

function settleAuthenticationRequired(
  state: AppState,
  requestId: string | undefined,
): AppState {
  let next = state;
  if (requestId != null && requestId === next.sessionTransition?.requestId) {
    next = rollbackSessionTransition(next);
  }
  if (requestId != null && requestId === next.pendingPrompt?.requestId) {
    next = archiveActivePlan(next);
    next = {
      ...next,
      running: false,
      agentActivity: undefined,
      pendingPrompt: undefined,
    };
  }
  if (requestId != null) {
    for (const [sessionId, snapshot] of next.cachedSessions) {
      if (snapshot.pendingPrompt?.requestId !== requestId) continue;
      next = updateCachedSession(
        next,
        sessionId,
        (cached) => ({
          ...archiveActivePlan(cached),
          running: false,
          agentActivity: undefined,
          pendingPrompt: undefined,
        }),
        { type: "auth_required", requestId },
      );
      break;
    }
  }
  if (requestId != null && requestId === next.pendingSessionControl?.requestId) {
    next = { ...next, pendingSessionControl: undefined };
  }
  if (requestId != null) {
    next = {
      ...next,
      pendingSessionDeletions: next.pendingSessionDeletions.filter(
        (pending) => pending.requestId !== requestId,
      ),
    };
  }
  return {
    ...next,
    authStatus: "required",
    pendingAuth: undefined,
    authError: undefined,
  };
}

function resetActiveSession(state: AppState, title?: string): AppState {
  return {
    ...state,
    cwd: state.defaultCwd,
    session: undefined,
    sessionOwner: undefined,
    historyStatus: undefined,
    historyNotice: undefined,
    pendingSessionId: undefined,
    sessionTransition: undefined,
    modeId: undefined,
    configOptions: null,
    availableCommands: [],
    timeline: [],
    terminalSnapshots: [],
    permissions: [],
    elicitations: requestScopedElicitations(state.elicitations),
    externalFlows: state.externalFlows,
    running: false,
    sessionSyncPhase: undefined,
    agentActivity: undefined,
    pendingSessionControl: undefined,
    pendingPrompt: undefined,
    runtimeOperation: undefined,
    title,
    usage: undefined,
    activePlan: undefined,
  };
}

/**
 * Project the live parts of an incrementally updated view: pending
 * interactions, and the session control a delta settled. Unlike a hydrate it
 * never rebuilds the timeline, and it keeps the response state of
 * interactions that are still pending.
 */
function applySessionLiveView(
  state: AppState,
  view: BridgeSessionView,
  settledControl: SessionControlKind | undefined,
): AppState {
  if (state.session?.sessionId !== view.sessionId || !sameSessionOwner(state.sessionOwner, view)) {
    return state;
  }
  const permissions = Object.values(view.interactions.permissions).map((pending) => {
    const current = state.permissions.find(({ permissionId }) => permissionId === pending.interactionId);
    return current != null && current.request === pending.request
      ? current
      : { ...current, permissionId: pending.interactionId, request: pending.request };
  });
  const elicitations = Object.values(view.interactions.elicitations).map((pending) => {
    const current = state.elicitations.find(({ elicitationId }) => elicitationId === pending.interactionId);
    return current != null && current.request === pending.request
      ? current
      : { ...current, elicitationId: pending.interactionId, request: pending.request };
  });
  let next: AppState = { ...state, permissions, elicitations };
  if (settledControl != null) {
    const session: NewSessionResponse = { ...(view.workspace.session ?? {}), sessionId: view.sessionId };
    next = {
      ...next,
      session,
      ...(settledControl === "mode"
        ? { modeId: session.modes?.currentModeId }
        : { configOptions: session.configOptions ?? null }),
      pendingSessionControl: state.pendingSessionControl?.kind === settledControl &&
          state.pendingSessionControl.sessionId === view.sessionId
        ? undefined
        : state.pendingSessionControl,
      runtimeOperation: state.runtimeOperation?.operation === settledControl
        ? undefined
        : state.runtimeOperation,
    };
  }
  return next;
}

function hydrateBridgeSession(state: AppState, view: BridgeSessionView): AppState {
  const priorTimeline = state.session?.sessionId === view.sessionId
    ? state.timeline
    : state.cachedSessions.get(view.sessionId)?.timeline ?? [];
  const priorTurnFailures = priorTimeline.filter((item) =>
    item.type === "error" && item.id.startsWith("bridge-turn-outcome:")
  );
  const cached = cacheCurrentSession(state);
  const listed = cached.sessions.find(({ sessionId }) => sessionId === view.sessionId);
  const session: NewSessionResponse = {
    ...(view.workspace.session ?? {}),
    sessionId: view.sessionId,
  };
  let next: AppState = {
    ...resetActiveSession(cached, listed?.title ?? undefined),
    cachedSessions: withoutCachedSession(cached.cachedSessions, view.sessionId),
    cwd: view.workspace.cwd ?? listed?.cwd ?? cached.defaultCwd,
    session,
    sessionOwner: { bridgeEpoch: view.bridgeEpoch, sessionId: view.sessionId, sessionIncarnation: view.sessionIncarnation },
    historyStatus: view.historyRevision == null
      ? {
          state: "loading",
          sessionId: view.sessionId,
          requestId: `bridge:${view.sessionIncarnation}:${view.viewRevision}`,
        }
      : { state: "available", sessionId: view.sessionId },
    historyNotice: view.historyNotice ?? undefined,
    sessionSyncPhase: view.phase,
    modeId: session.modes?.currentModeId,
    configOptions: session.configOptions ?? null,
    permissions: Object.values(view.interactions.permissions).map((pending) => ({
      permissionId: pending.interactionId,
      request: pending.request,
    })),
    elicitations: Object.values(view.interactions.elicitations).map((pending) => ({
      elicitationId: pending.interactionId,
      request: pending.request,
    })),
    externalFlows: Object.values(view.interactions.urlFlows).map((flow) => ({
      elicitationId: flow.elicitationId,
      sessionId: elicitationSessionId(flow.request),
      url: flow.request.mode === "url" && "url" in flow.request &&
          typeof flow.request.url === "string"
        ? flow.request.url
        : undefined,
      message: flow.request.message,
      status: flow.status,
    })),
    terminalSnapshots: Object.values(view.terminals),
    runtimeOperation: view.operation == null
      ? undefined
      : {
          requestId: view.operation.operationId,
          operation: bridgeOperationKind(view.operation.kind),
        },
  };

  const outcomesByOffset = new Map<number, NonNullable<BridgeSessionView["turnOutcomes"]>>();
  for (const outcome of view.turnOutcomes ?? []) {
    if (!Number.isSafeInteger(outcome.afterUpdate) || outcome.afterUpdate <= 0) continue;
    const outcomes = outcomesByOffset.get(outcome.afterUpdate) ?? [];
    outcomes.push(outcome);
    outcomesByOffset.set(outcome.afterUpdate, outcomes);
  }
  if (view.collapsedTurns != null) {
    const owner: SessionOwner = {
      bridgeEpoch: view.bridgeEpoch, sessionId: view.sessionId, sessionIncarnation: view.sessionIncarnation,
    };
    const observedTurns = new Map<string, TimelineItem[]>();
    if (state.session?.sessionId === view.sessionId && sameSessionOwner(state.sessionOwner, owner)) {
      const starts = timelineTurnStarts(priorTimeline);
      for (const [index, start] of starts.entries()) {
        const items = priorTimeline.slice(start, starts[index + 1]);
        if (items.length === 0 || items.some((item) => item.deferredProcess != null)) continue;
        for (const item of items) {
          if (item.type === "stop" && item.id.startsWith("bridge-turn-outcome:")) {
            observedTurns.set(item.id.slice("bridge-turn-outcome:".length), items);
          } else if (item.type === "message" && item.turnOperationId != null) {
            observedTurns.set(item.turnOperationId, items);
          }
        }
        if (start + items.length === priorTimeline.length && state.pendingPrompt != null) {
          observedTurns.set(state.pendingPrompt.requestId, items);
        }
      }
    }
    for (const turn of view.collapsedTurns) {
      // Reduce each compact turn independently: missing process rows must not
      // make distinct final answers or reused Agent IDs merge across turns.
      let projected = { ...next, timeline: [] as TimelineItem[] };
      for (const update of view.timeline.slice(turn.beforeUpdate, turn.afterUpdate)) {
        projected = reduceSessionUpdate(projected, { sessionId: view.sessionId, update });
      }
      for (const outcome of turn.outcomes) {
        projected.timeline = [...finishTurnTools(projected.timeline, outcome.response), {
          id: `bridge-turn-outcome:${outcome.operationId}`, type: "stop", response: outcome.response,
        }];
      }
      const entries = projected.timeline.length > 0 ? projected.timeline : [{
        id: `history:${turn.turnId}:empty`, type: "assistant" as const, chunks: [],
      }];
      let identified = entries.map((item, index): TimelineItem => ({
        ...item, id: item.type === "stop" ? item.id : `history:${turn.turnId}:${index}`,
      }));
      if (turn.processIncluded) {
        const previous = turn.outcomes.map((outcome) => observedTurns.get(outcome.operationId))
          .find((items) => items != null) ?? projected.timeline.flatMap((item) =>
            item.type === "message" && item.turnOperationId != null
              ? [observedTurns.get(item.turnOperationId)] : []
          ).find((items) => items != null);
        if (previous != null) identified = preserveIncludedTurnIdentity(identified, previous);
      }
      next = {
        ...next,
        timeline: [...next.timeline, ...identified.map((item, index): TimelineItem => ({
          ...item,
          historyTurnId: turn.turnId,
          ...(index === 0 ? { [turn.processIncluded ? "retainedProcess" : "deferredProcess"]: {
            turnId: turn.turnId, operationId: turn.operationId ?? turn.outcomes.at(-1)?.operationId,
            historyRevision: turn.historyRevision, processCount: turn.processCount, owner,
          } } : {}),
        }))],
      };
    }
  } else {
    for (const [index, update] of view.timeline.entries()) {
      next = reduceSessionUpdate(next, { sessionId: view.sessionId, update });
      for (const outcome of outcomesByOffset.get(index + 1) ?? []) {
        const id = `bridge-turn-outcome:${outcome.operationId}`;
        if (!next.timeline.some((item) => item.id === id)) {
          next = {
            ...next,
            timeline: [...finishTurnTools(next.timeline, outcome.response), { id, type: "stop", response: outcome.response }],
          };
        }
      }
    }
  }
  for (const update of Object.values(view.controls)) {
    next = reduceSessionUpdate(next, { sessionId: view.sessionId, update });
  }
  if (view.activeTurn != null) {
    next = startPrompt(
      next,
      view.activeTurn.operationId,
      view.sessionId,
      view.activeTurn.prompt,
    );
    for (const update of view.activeTurn.updates) {
      next = reduceSessionUpdate(next, { sessionId: view.sessionId, update });
    }
    if (view.activeTurn.cancelRequested && view.phase === "running" && next.pendingPrompt != null) {
      next = {
        ...next,
        pendingPrompt: { ...next.pendingPrompt, cancelRequested: true },
        timeline: cancelTurnTools(next.timeline),
      };
    }
    if (isStateRecord(view.activeTurn.terminal) && view.activeTurn.terminal.stopReason === "cancelled") {
      next = { ...next, timeline: finishTurnTools(next.timeline, { stopReason: "cancelled" }) };
    }
    if (view.phase !== "running") {
      next = archiveActivePlan(next);
      next = {
        ...next,
        running: view.phase === "reconciling",
        agentActivity: undefined,
        pendingPrompt: view.phase === "reconciling" ? next.pendingPrompt : undefined,
      };
    }
  }
  if (view.syncError != null && view.phase === "blocked") {
    next = {
      ...next,
      timeline: [
        ...next.timeline,
        {
          id: randomId(),
          type: "error",
          message: `Session synchronization failed: ${view.syncError}`,
        },
      ],
    };
  }
  if (priorTurnFailures.length > 0) {
    const rebuiltIds = new Set(next.timeline.map(({ id }) => id));
    next = {
      ...next,
      timeline: [
        ...next.timeline,
        ...priorTurnFailures.filter(({ id }) => !rebuiltIds.has(id)),
      ],
    };
  }
  return next;
}

/** Keep mounted reading/disclosure state, while every body comes from the new view. */
function preserveIncludedTurnIdentity(entries: TimelineItem[], previous: TimelineItem[]): TimelineItem[] {
  const remaining = [...previous];
  return entries.map((item) => {
    let index = remaining.findIndex((prior) => {
      if (prior.type !== item.type) return false;
      if (item.type === "tool" && prior.type === "tool") return item.call.toolCallId === prior.call.toolCallId;
      if (item.type === "compaction" && prior.type === "compaction") return item.compactionId === prior.compactionId;
      if (item.type === "message" && prior.type === "message") return item.messageId != null && item.messageId === prior.messageId;
      if (item.type === "assistant" && prior.type === "assistant") return item.chunks.some((chunk) =>
        chunk.messageId != null && prior.chunks.some((old) => old.role === chunk.role && old.messageId === chunk.messageId));
      return prior.id === item.id;
    });
    if (index < 0 && (item.type === "message" || item.type === "assistant" || item.type === "plan" || item.type === "protocol")) {
      index = remaining.findIndex((prior) => prior.type === item.type);
    }
    if (index < 0) return item;
    const [prior] = remaining.splice(index, 1);
    if (item.type !== "assistant" || prior.type !== "assistant") return { ...item, id: prior.id };
    const chunks = [...prior.chunks];
    return { ...item, id: prior.id, chunks: item.chunks.map((chunk) => {
      let match = chunks.findIndex((old) => old.role === chunk.role && old.messageId === chunk.messageId);
      if (match < 0) match = chunks.findIndex((old) => old.role === chunk.role);
      return match < 0 ? chunk : { ...chunk, id: chunks.splice(match, 1)[0].id };
    }) };
  });
}

/** Decode presentation-only pages without changing the live reducer's cut. */
export function processPageTimeline(page: BridgeTurnProcessPage): TimelineItem[] {
  return page.items.flatMap((updates, index) => {
    let projected: AppState = { ...initialState, session: { sessionId: page.sessionId } };
    for (const update of updates) {
      projected = reduceSessionUpdate(projected, { sessionId: page.sessionId, update });
    }
    if (page.response != null) projected.timeline = finishTurnTools(projected.timeline, page.response);
    return projected.timeline.map((item, itemIndex): TimelineItem => {
      const id = `process:${page.turnId}:${page.offset + index}:${itemIndex}`;
      return item.type === "assistant"
        ? { ...item, id, chunks: item.chunks.map((chunk, chunkIndex) => ({ ...chunk, id: `${id}:chunk:${chunkIndex}` })) }
        : { ...item, id };
    });
  });
}

function applyBridgeTurnComplete(
  state: AppState,
  event: Extract<SessionBusinessEvent, { type: "bridge/session_turn_complete" }>,
): AppState {
  const outcomeId = `bridge-turn-outcome:${event.operationId}`;
  if (
    state.session?.sessionId !== event.sessionId ||
    state.timeline.some(({ id }) => id === outcomeId)
  ) return state;
  const ownsActivePrompt = state.pendingPrompt == null ||
    state.pendingPrompt.requestId === event.clientIntentId ||
    state.pendingPrompt.requestId === event.operationId;
  if (!ownsActivePrompt) {
    return {
      ...state,
      timeline: [
        ...state.timeline,
        { id: outcomeId, type: "stop", response: event.response },
      ],
    };
  }
  state = archiveActivePlan(state);
  return {
    ...state,
    running: false,
    sessionSyncPhase: event.phase,
    agentActivity: undefined,
    pendingPrompt: undefined,
    timeline: [
      ...finishTurnTools(state.timeline, event.response),
      { id: outcomeId, type: "stop", response: event.response },
    ],
  };
}

function applyBridgeTurnFailure(
  state: AppState,
  event: Extract<SessionBusinessEvent, { type: "bridge/session_turn_failed" }>,
): AppState {
  const outcomeId = `bridge-turn-outcome:${event.operationId}`;
  if (
    state.session?.sessionId !== event.sessionId ||
    state.timeline.some(({ id }) => id === outcomeId)
  ) return state;
  const error = {
    id: outcomeId,
    type: "error" as const,
    message: event.error.message,
    requestId: event.operationId,
    operation: "session/prompt" as const,
    code: event.error.code,
    data: event.error.data,
    retryBlocks: event.prompt,
  };
  const ownsActivePrompt = state.pendingPrompt == null ||
    state.pendingPrompt.requestId === event.clientIntentId ||
    state.pendingPrompt.requestId === event.operationId;
  if (ownsActivePrompt) state = archiveActivePlan(state);
  const next = !ownsActivePrompt
    ? { ...state, timeline: [...state.timeline, error] }
    : {
        ...state,
        running: false,
        sessionSyncPhase: event.phase,
        agentActivity: undefined,
        pendingPrompt: undefined,
        timeline: [...state.timeline, error],
      };
  return event.error.code === -32_000 && (state.initialized?.authMethods?.length ?? 0) > 0
    ? settleAuthenticationRequired(next, event.clientIntentId)
    : next;
}

function bridgeOperationKind(kind: string): SessionRuntimeOperation {
  switch (kind) {
    case "fork":
    case "close":
    case "delete":
    case "mode":
    case "config":
      return kind;
    case "set_mode":
      return "mode";
    case "set_config":
      return "config";
    default:
      return "close";
  }
}

function cacheCurrentSession(state: AppState): AppState {
  const sessionId = state.session?.sessionId;
  if (!sessionId) return state;
  const cachedSessions = new Map(state.cachedSessions);
  cachedSessions.set(sessionId, captureActiveSession(state));
  const existing = state.sessions.find((session) => session.sessionId === sessionId);
  const openSession: SessionInfo = {
    ...existing,
    sessionId,
    cwd: state.cwd,
    ...(state.title !== undefined ? { title: state.title } : {}),
  };
  return {
    ...state,
    cachedSessions,
    sessions: mergeSessions(state.sessions, [openSession]),
  };
}

function activateCachedSession(state: AppState, sessionId: string): AppState {
  if (
    state.session?.sessionId === sessionId ||
    state.sessionTransition != null ||
    state.pendingSessionControl != null
  ) return state;
  const snapshot = state.cachedSessions.get(sessionId);
  if (!snapshot?.session) return state;
  const listed = state.sessions.find((session) => session.sessionId === sessionId);

  const withCurrentCached = cacheCurrentSession(state);
  const cachedSessions = new Map(withCurrentCached.cachedSessions);
  cachedSessions.delete(sessionId);
  return {
    ...withCurrentCached,
    ...snapshot,
    cachedSessions,
    attentionSessionIds: state.attentionSessionIds.filter(
      (candidate) => candidate !== sessionId,
    ),
    pendingSessionId: undefined,
    sessionTransition: undefined,
    pendingSessionControl: undefined,
    title: snapshot.title ?? listed?.title ?? undefined,
    elicitations: [
      ...snapshot.elicitations,
      ...requestScopedElicitations(state.elicitations),
    ],
  };
}

function updateCachedSession(
  state: AppState,
  sessionId: string,
  update: (cached: AppState) => AppState,
  backgroundEvent: unknown,
): AppState {
  const snapshot = state.cachedSessions.get(sessionId);
  if (!snapshot) return appendBackgroundEvent(state, backgroundEvent);
  const updated = update({ ...state, ...snapshot });
  const cachedSessions = new Map(state.cachedSessions);
  cachedSessions.set(sessionId, captureActiveSession(updated));
  return {
    ...state,
    // session_info_update also updates the Agent-owned history row.
    sessions: updated.sessions,
    // URL elicitations are connection-level UI state even when their request
    // and response are replayed through a cached session snapshot.
    externalFlows: updated.externalFlows,
    cachedSessions,
  };
}

function removeCachedSession(state: AppState, sessionId: string): AppState {
  return {
    ...state,
    cachedSessions: withoutCachedSession(state.cachedSessions, sessionId),
    attentionSessionIds: state.attentionSessionIds.filter(
      (candidate) => candidate !== sessionId,
    ),
  };
}

function clearRuntimeOperation(
  state: AppState,
  requestId: string | undefined,
): AppState {
  if (requestId == null) return state;
  let changed = false;
  let next = state;
  if (state.runtimeOperation?.requestId === requestId) {
    next = { ...next, runtimeOperation: undefined };
    changed = true;
  }
  const cachedSessions = new Map(next.cachedSessions);
  for (const [sessionId, snapshot] of cachedSessions) {
    if (snapshot.runtimeOperation?.requestId !== requestId) continue;
    cachedSessions.set(sessionId, { ...snapshot, runtimeOperation: undefined });
    changed = true;
  }
  return changed ? { ...next, cachedSessions } : state;
}

function removeAuthoritativeSessionRuntime(
  state: AppState,
  sessionId: string,
  deleted: boolean,
  owner?: SessionOwner,
): AppState {
  const matches = (candidate?: SessionOwner) => owner == null || sameSessionOwner(candidate, owner);
  let next = state;
  if (next.sessionTransition?.targetSessionId === sessionId && matches(next.sessionTransition.backup.sessionOwner)) {
    next = rollbackSessionTransition(next);
  }
  if (next.session?.sessionId === sessionId && matches(next.sessionOwner)) {
    next = resetActiveSession(next);
  } else if (next.sessionTransition?.backup.session?.sessionId === sessionId && matches(next.sessionTransition.backup.sessionOwner)) {
    next = {
      ...next,
      sessionTransition: {
        ...next.sessionTransition,
        backup: captureActiveSession(resetActiveSession(next)),
      },
    };
  }
  if (matches(next.cachedSessions.get(sessionId)?.sessionOwner)) next = removeCachedSession(next, sessionId);
  const replacement = state.session?.sessionId === sessionId && !matches(state.sessionOwner) ||
    state.cachedSessions.has(sessionId) && !matches(state.cachedSessions.get(sessionId)?.sessionOwner);
  return {
    ...next,
    sessions: deleted && !replacement
      ? next.sessions.filter((session) => session.sessionId !== sessionId)
      : next.sessions,
    pendingSessionDeletions: deleted && !replacement
      ? next.pendingSessionDeletions.filter((pending) => pending.sessionId !== sessionId)
      : next.pendingSessionDeletions,
  };
}

function withoutCachedSession(
  cachedSessions: ReadonlyMap<string, ActiveSessionSnapshot>,
  sessionId: string,
): Map<string, ActiveSessionSnapshot> {
  const next = new Map(cachedSessions);
  next.delete(sessionId);
  return next;
}

function captureActiveSession(state: AppState): ActiveSessionSnapshot {
  return compactSessionSnapshot({
    cwd: state.cwd,
    session: state.session,
    sessionOwner: state.sessionOwner,
    availableCommands: state.availableCommands,
    modeId: state.modeId,
    configOptions: state.configOptions,
    timeline: state.timeline,
    permissions: state.permissions,
    elicitations: state.elicitations.filter(({ request }) =>
      elicitationSessionId(request) != null
    ),
    running: state.running,
    agentActivity: state.agentActivity,
    pendingPrompt: state.pendingPrompt,
    runtimeOperation: state.runtimeOperation,
    title: state.title,
    usage: state.usage,
    activePlan: state.activePlan,
    terminalSnapshots: state.terminalSnapshots,
    historyStatus: state.historyStatus,
    historyNotice: state.historyNotice,
  });
}

function compactSessionSnapshot(snapshot: ActiveSessionSnapshot): ActiveSessionSnapshot {
  let timeline = snapshot.timeline;
  for (const item of snapshot.timeline) {
    if (item.retainedProcess != null) timeline = releaseTimelineProcess(timeline, item.retainedProcess);
  }
  if (timeline === snapshot.timeline) return snapshot;
  return { ...snapshot, timeline, terminalSnapshots: releaseUnreferencedTerminals(snapshot.terminalSnapshots,
    snapshot.timeline, [timeline, snapshot.permissions, snapshot.elicitations, snapshot.activePlan]),
  };
}

function rollbackSessionTransition(state: AppState): AppState {
  const transition = state.sessionTransition;
  if (!transition) return state;
  const cachedSessions = transition.backup.session?.sessionId
    ? withoutCachedSession(state.cachedSessions, transition.backup.session.sessionId)
    : state.cachedSessions;
  return {
    ...state,
    ...transition.backup,
    cachedSessions,
    elicitations: [
      ...transition.backup.elicitations,
      ...requestScopedElicitations(state.elicitations),
    ],
    pendingSessionId: undefined,
    sessionTransition: undefined,
  };
}

function elicitationSessionId(
  request: CreateElicitationRequest,
): string | undefined {
  return "sessionId" in request && typeof request.sessionId === "string"
    ? request.sessionId
    : undefined;
}

function requestScopedElicitations(
  elicitations: PendingElicitation[],
): PendingElicitation[] {
  return elicitations.filter(({ request }) => elicitationSessionId(request) == null);
}

function failPermissionResponse(
  state: AppState,
  permissionId: string,
  message: string,
): AppState {
  return {
    ...state,
    permissions: state.permissions.map((permission) =>
      permission.permissionId === permissionId
        ? { ...permission, responseRequestId: undefined, responseError: message }
        : permission
    ),
  };
}

function failElicitationResponse(
  state: AppState,
  elicitationId: string,
  message: string,
): AppState {
  return {
    ...state,
    elicitations: state.elicitations.map((elicitation) =>
      elicitation.elicitationId === elicitationId
        ? { ...elicitation, responseRequestId: undefined, responseError: message }
        : elicitation
    ),
  };
}

function isCurrentSession(state: AppState, sessionId: string): boolean {
  return (state.session?.sessionId ?? state.pendingSessionId) === sessionId;
}

function errorTimelineItem(
  event: BridgeError,
  retryBlocks?: ContentBlock[],
): Extract<TimelineItem, { type: "error" }> {
  return {
    id: randomId(),
    type: "error",
    message: event.message,
    requestId: event.requestId,
    operation: event.operation,
    code: event.code,
    data: event.data,
    dataTruncated: event.dataTruncated,
    dataBytes: event.dataBytes,
    retryBlocks,
  };
}

function appendBackgroundEvent(state: AppState, event: unknown): AppState {
  return {
    ...state,
    backgroundEvents: [...state.backgroundEvents, event],
  };
}

function upsertTerminalSnapshot(
  snapshots: TerminalSnapshot[],
  incoming: TerminalSnapshot,
): TerminalSnapshot[] {
  const previous = snapshots.find(
    ({ terminalId }) => terminalId === incoming.terminalId,
  );
  const output = incoming.outputAppend && previous != null
    ? previous.output + incoming.output
    : incoming.output;
  const retainedOutput = retainTerminalBytes(output, incoming.retainedBytes);
  const merged = { ...incoming, output: retainedOutput };
  const next = [
    ...snapshots.filter(({ terminalId }) => terminalId !== incoming.terminalId),
    merged,
  ];
  // A materialized business view owns its history retention. Live updates must
  // not evict terminal results that are still referenced by that history.
  return next;
}

function mergeSessions(current: SessionInfo[], incoming: SessionInfo[]): SessionInfo[] {
  const sessions = new Map(current.map((session) => [session.sessionId, session]));
  for (const session of incoming) sessions.set(session.sessionId, session);
  return [...sessions.values()];
}

function retainTerminalBytes(output: string, retainedBytes?: number): string {
  if (retainedBytes == null) return output;
  const bytes = new TextEncoder().encode(output);
  if (bytes.length <= retainedBytes) return output;
  let start = bytes.length - retainedBytes;
  while (start < bytes.length && (bytes[start] & 0xc0) === 0x80) start += 1;
  return new TextDecoder().decode(bytes.subarray(start));
}
