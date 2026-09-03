import type {
  AuthenticateResponse,
  AvailableCommand,
  CompactionStatus,
  ContentBlock,
  CreateElicitationRequest,
  CreateElicitationResponse,
  ForkSessionResponse,
  InitializeResponse,
  NewSessionResponse,
  LogoutResponse,
  NesSuggestion,
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
  AgentTransport,
  ConnectionPhase,
  NesDocumentState,
  ServerEvent,
  SessionRuntimeOperation,
  TerminalSnapshot,
} from "../../../shared/bridge";
import { assertNever } from "../../../shared/exhaustive";
import type {
  BridgeSessionView,
  SessionBusinessEvent,
  SessionSyncPhase,
} from "./business-api";
import { randomId } from "./id";

export interface AssistantMessageChunk {
  id: string;
  role: "agent" | "thought";
  blocks: ContentBlock[];
  messageId?: string | null;
  raw: unknown[];
}

export type TimelineItem =
  | {
      id: string;
      type: "message";
      role: "user" | "protocol-user";
      blocks: ContentBlock[];
      messageId?: string | null;
      turnOperationId?: string;
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
      operation?: Extract<ServerEvent, { type: "bridge/error" }>["operation"];
      code?: number;
      data?: unknown;
      dataTruncated?: boolean;
      dataBytes?: number;
      retryBlocks?: ContentBlock[];
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
  abortReason?: Extract<ServerEvent, { type: "acp/elicitation_aborted" }>["reason"];
}

export type McpActivity = Extract<ServerEvent, { type: "acp/mcp_message" }>;

export type AgentActivity =
  | { kind: "waiting" }
  | { kind: "thinking"; timelineId: string }
  | { kind: "tool"; toolCallId: string; title: string }
  | { kind: "planning" }
  | { kind: "compacting" }
  | { kind: "responding" };

export interface ActiveMcpConnection {
  serverId: string;
  connectionId: string;
  name: string;
}

export interface ActiveSessionSnapshot {
  cwd: string;
  session?: NewSessionResponse;
  availableCommands: AvailableCommand[];
  modeId?: string;
  configOptions: SessionConfigOption[];
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

export interface PendingNesAccept {
  requestId: string;
  sessionId: string;
  suggestionId: string;
  uri?: string;
  previousDraft?: string;
  optimisticText?: string;
  documentAcknowledged: boolean;
}

export interface PendingNesSuggestion {
  requestId: string;
  sessionId: string;
  uri: string;
  triggerKind: "automatic" | "diagnostic" | "manual";
}

export interface AppState {
  phase: ConnectionPhase;
  socketOpen: boolean;
  runtimeReplaying: boolean;
  runtimeReplacement?: AppState;
  sessionLoadReplacement?: AppState;
  transport: AgentTransport;
  command: string[];
  defaultCwd: string;
  cwd: string;
  additionalDirectories: string[];
  mcpServers: Array<{ name: string; type: "stdio" | "http" | "sse" | "acp" }>;
  mcpConnections: ActiveMcpConnection[];
  mcpActivity: McpActivity[];
  readOnly: boolean;
  initialized?: InitializeResponse;
  authStatus?: AgentAuthStatus;
  pendingAuth?: PendingAuthOperation;
  authTerminal?: AuthTerminalState;
  authError?: string;
  lastAuthResponse?: AgentAuthResponse;
  session?: NewSessionResponse;
  historyStatus?: HistoryStatus;
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
  configOptions: SessionConfigOption[];
  timeline: TimelineItem[];
  terminalSnapshots: TerminalSnapshot[];
  permissions: PendingPermission[];
  elicitations: PendingElicitation[];
  externalFlows: ExternalElicitationFlow[];
  backgroundEvents: unknown[];
  stderr: string;
  running: boolean;
  sessionSyncPhase?: SessionSyncPhase;
  agentActivity?: AgentActivity;
  title?: string;
  usage?: { used: number; size: number; cost?: { amount: number; currency: string } | null };
  activePlan?: Extract<TimelineItem, { type: "plan" }>;
  nesSessionId?: string;
  nesDocuments: NesDocumentState[];
  nesDrafts: Record<string, string>;
  pendingNesSuggestion?: PendingNesSuggestion;
  pendingNesAccept?: PendingNesAccept;
  nesError?: string;
  activeNesUri?: string;
  nesSuggestions: NesSuggestion[];
}

export type AppAction =
  | { type: "socket/open" }
  | { type: "socket/closed" }
  | { type: "bridge/session_hydrate"; view: BridgeSessionView }
  | {
      type: "bridge/turn_complete";
      event: Extract<SessionBusinessEvent, { type: "bridge/session_turn_complete" }>;
    }
  | {
      type: "bridge/turn_failed";
      event: Extract<SessionBusinessEvent, { type: "bridge/session_turn_failed" }>;
    }
  | {
      type: "runtime/replay_complete";
      preferredSessionId?: string;
      fallbackSessionId?: string;
    }
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
  | { type: "server/event"; event: ServerEvent }
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
  | { type: "nes/local_change"; uri: string; text: string }
  | {
      type: "nes/accept_start";
      requestId: string;
      sessionId: string;
      suggestionId: string;
      text?: string;
    }
  | { type: "nes/select_document"; uri: string }
  | {
      type: "nes/suggest_start";
      requestId: string;
      sessionId: string;
      uri: string;
      triggerKind: PendingNesSuggestion["triggerKind"];
    }
  | { type: "session/reset" }
  | { type: "session/activate_cached"; sessionId: string };

export const initialState: AppState = {
  phase: "starting",
  socketOpen: false,
  runtimeReplaying: false,
  transport: "stdio",
  command: [],
  defaultCwd: "",
  cwd: "",
  additionalDirectories: [],
  mcpServers: [],
  mcpConnections: [],
  mcpActivity: [],
  readOnly: false,
  cachedSessions: new Map(),
  attentionSessionIds: [],
  sessions: [],
  pendingSessionDeletions: [],
  availableCommands: [],
  configOptions: [],
  timeline: [],
  terminalSnapshots: [],
  permissions: [],
  elicitations: [],
  externalFlows: [],
  backgroundEvents: [],
  stderr: "",
  running: false,
  nesDocuments: [],
  nesDrafts: {},
  nesSuggestions: [],
};

function withoutRuntimeReplacement(state: AppState): AppState {
  return state.runtimeReplacement == null
    ? state
    : { ...state, runtimeReplacement: undefined };
}

function withoutSessionLoadReplacement(state: AppState): AppState {
  return state.sessionLoadReplacement == null
    ? state
    : { ...state, sessionLoadReplacement: undefined };
}

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

function isMatchingSessionLoadUpdate(
  state: AppState,
  event: ServerEvent,
): boolean {
  const transition = state.sessionTransition;
  return transition?.kind === "attach" &&
    event.type === "acp/session_update" &&
    event.notification.sessionId === transition.targetSessionId;
}

function isMatchingSessionLoadCommit(
  state: AppState,
  event: ServerEvent,
): event is Extract<ServerEvent, { type: "acp/session_attached" }> {
  const transition = state.sessionTransition;
  return transition?.kind === "attach" &&
    event.type === "acp/session_attached" &&
    event.requestId === transition.requestId &&
    event.sessionId === transition.targetSessionId;
}

function isMatchingSessionLoadFailure(
  state: AppState,
  event: ServerEvent,
): event is Extract<ServerEvent, { type: "bridge/error" }> {
  return state.sessionTransition?.kind === "attach" &&
    event.type === "bridge/error" &&
    event.requestId === state.sessionTransition.requestId;
}

function isRuntimeReplacementEvent(event: ServerEvent): boolean {
  switch (event.type) {
    case "bridge/phase":
    case "bridge/runtime_session":
    case "bridge/session_operation_started":
    case "bridge/error":
    case "acp/session_created":
    case "acp/session_attached":
    case "acp/session_forked":
    case "acp/session_closed":
    case "acp/session_deleted":
    case "acp/session_update":
    case "acp/terminal_state":
    case "acp/prompt_started":
    case "acp/prompt_complete":
    case "acp/permission_request":
    case "acp/permission_resolved":
    case "acp/elicitation_request":
    case "acp/elicitation_complete":
    case "acp/elicitation_resolved":
    case "acp/elicitation_aborted":
    case "acp/mode_changed":
    case "acp/config_changed":
    case "acp/nes_started":
    case "acp/nes_suggestions":
    case "acp/nes_suggestion_resolved":
    case "acp/nes_closed":
    case "acp/document_opened":
    case "acp/document_changed":
    case "acp/document_saved":
    case "acp/document_focused":
    case "acp/document_closed":
      return true;
    default:
      return false;
  }
}

export function appReducer(state: AppState, action: AppAction): AppState {
  switch (action.type) {
    case "socket/open":
      return state.runtimeReplacement == null
        ? { ...state, socketOpen: true }
        : {
            ...state,
            socketOpen: true,
            runtimeReplacement: { ...state.runtimeReplacement, socketOpen: true },
          };
    case "socket/closed":
      return terminateBridgeState(state, "stopped", false);
    case "bridge/session_hydrate":
      return hydrateBridgeSession(state, action.view);
    case "bridge/turn_complete":
      return applyBridgeTurnComplete(state, action.event);
    case "bridge/turn_failed":
      return applyBridgeTurnFailure(state, action.event);
    case "runtime/replay_complete": {
      const next = {
        ...(state.runtimeReplacement ?? state),
        runtimeReplaying: false,
        runtimeReplacement: undefined,
        sessionLoadReplacement: undefined,
      };
      const sessionId = action.preferredSessionId != null &&
          next.cachedSessions.has(action.preferredSessionId)
        ? action.preferredSessionId
        : action.fallbackSessionId;
      return sessionId == null ? next : activateCachedSession(next, sessionId);
    }
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
          state.running ||
          state.pendingPrompt != null ||
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
        const historyStatus: HistoryStatus = {
          state: "loading",
          sessionId: action.sessionId,
          requestId: action.requestId,
        };
        const sessionLoadReplacement: AppState = {
          ...resetActiveSession(stateWithCachedCurrent, action.title ?? undefined),
          cwd: action.cwd ?? state.defaultCwd,
          pendingSessionId: action.sessionId,
          sessionTransition: transition,
          historyStatus,
          sessionLoadReplacement: undefined,
        };
        return {
          ...stateWithCachedCurrent,
          pendingSessionId: action.sessionId,
          sessionTransition: transition,
          historyStatus,
          sessionLoadReplacement,
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
        const next = state.session?.sessionId === action.sessionId
          ? resetActiveSession(cacheCurrentSession(state))
          : state;
        return {
          ...next,
          pendingSessionDeletions: [
            ...next.pendingSessionDeletions,
            {
              requestId: action.requestId,
              sessionId: action.sessionId,
              stage: action.stage,
            },
          ].slice(-100),
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
        state.running ||
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
    case "nes/local_change":
      return {
        ...state,
        nesDrafts: { ...state.nesDrafts, [action.uri]: action.text },
        nesSuggestions: state.nesSuggestions.filter(
          (suggestion) => suggestion.uri !== action.uri,
        ),
        pendingNesSuggestion: state.pendingNesSuggestion?.uri === action.uri
          ? undefined
          : state.pendingNesSuggestion,
        nesError: undefined,
      };
    case "nes/accept_start": {
      if (
        state.pendingNesAccept != null ||
        state.nesSessionId !== action.sessionId
      ) return state;
      const suggestion = state.nesSuggestions.find(
        ({ id }) => id === action.suggestionId,
      );
      if (!suggestion) return state;
      if (suggestion.kind === "edit" && action.text == null) return state;
      if (suggestion.kind !== "edit" && action.text != null) return state;
      const uri = suggestion.kind === "edit" ? suggestion.uri : undefined;
      return {
        ...state,
        nesDrafts: uri == null
          ? state.nesDrafts
          : { ...state.nesDrafts, [uri]: action.text! },
        pendingNesAccept: {
          requestId: action.requestId,
          sessionId: action.sessionId,
          suggestionId: action.suggestionId,
          uri,
          previousDraft: uri == null ? undefined : state.nesDrafts[uri],
          optimisticText: uri == null ? undefined : action.text,
          documentAcknowledged: false,
        },
      };
    }
    case "nes/select_document":
      return state.nesDocuments.some(({ uri }) => uri === action.uri)
        ? { ...state, activeNesUri: action.uri }
        : state;
    case "nes/suggest_start":
      if (
        state.nesSessionId !== action.sessionId ||
        state.pendingNesAccept != null
      ) return state;
      return {
        ...state,
        nesSuggestions: [],
        pendingNesSuggestion: {
          requestId: action.requestId,
          sessionId: action.sessionId,
          uri: action.uri,
          triggerKind: action.triggerKind,
        },
        nesError: undefined,
      };
    case "session/reset":
      return resetActiveSession(state);
    case "session/activate_cached":
      return activateCachedSession(state, action.sessionId);
    case "server/event": {
      const sessionLoadReplacement = state.sessionLoadReplacement;
      if (sessionLoadReplacement != null) {
        if (isMatchingSessionLoadUpdate(state, action.event)) {
          return {
            ...state,
            sessionLoadReplacement: reduceServerEvent(
              sessionLoadReplacement,
              action.event,
            ),
          };
        }
        if (isMatchingSessionLoadCommit(state, action.event)) {
          const committed = reduceServerEvent(sessionLoadReplacement, action.event);
          return {
            ...committed,
            sessionLoadReplacement: undefined,
            historyStatus: {
              state: "available",
              sessionId: action.event.sessionId,
            },
          };
        }
        if (isMatchingSessionLoadFailure(state, action.event)) {
          return reduceServerEvent(
            withoutSessionLoadReplacement(state),
            action.event,
          );
        }
      }
      const replacement = state.runtimeReplacement;
      if (replacement == null) return reduceServerEvent(state, action.event);
      if (action.event.type === "bridge/runtime_replay_started") {
        return reduceServerEvent(withoutRuntimeReplacement(state), action.event);
      }
      if (
        action.event.type === "bridge/phase" &&
        (action.event.phase === "error" || action.event.phase === "stopped")
      ) {
        return reduceServerEvent(withoutRuntimeReplacement(state), action.event);
      }
      if (isRuntimeReplacementEvent(action.event)) {
        return {
          ...state,
          runtimeReplacement: reduceServerEvent(replacement, action.event),
        };
      }
      return {
        ...reduceServerEvent(withoutRuntimeReplacement(state), action.event),
        runtimeReplaying: true,
        runtimeReplacement: reduceServerEvent(replacement, action.event),
      };
    }
  }
}

function reduceServerEvent(state: AppState, event: ServerEvent): AppState {
  switch (event.type) {
    case "bridge/hello":
      return {
        ...state,
        transport: event.transport,
        command: event.command,
        defaultCwd: event.cwd,
        cwd: event.cwd,
        readOnly: event.readOnly,
        additionalDirectories: event.additionalDirectories,
        mcpServers: event.mcpServers,
      };
    case "bridge/runtime_replay_started": {
      const visible = withoutSessionLoadReplacement(withoutRuntimeReplacement(state));
      const reset = resetActiveSession(visible);
      const runtimeReplacement: AppState = {
        ...reset,
        runtimeReplaying: true,
        cachedSessions: new Map(),
        attentionSessionIds: [],
        sessions: [],
        backgroundEvents: [],
      };
      return {
        ...visible,
        phase: visible.phase === "ready" ? "initializing" : visible.phase,
        runtimeReplaying: true,
        runtimeReplacement,
        sessionLoadReplacement: undefined,
      };
    }
    case "bridge/runtime_session":
      return cacheRuntimeSession(state, event);
    case "bridge/runtime_replay_complete":
      // useAcp supplies the browser's preferred session ID when it handles
      // this marker and dispatches runtime/replay_complete.
      return state;
    case "bridge/runtime_snapshot":
    case "bridge/runtime_delta":
    case "bridge/intent_ack":
      // The canonical stream is running in shadow mode until the backend
      // state-machine and transport gates are complete. Legacy ACP events
      // remain the renderer's source during this migration phase.
      return state;
    case "bridge/session_operation_started":
      if (isCurrentSession(state, event.sessionId)) {
        return {
          ...state,
          runtimeOperation: {
            requestId: event.requestId,
            operation: event.operation,
          },
        };
      }
      return updateCachedSession(
        state,
        event.sessionId,
        (cached) => ({
          ...cached,
          runtimeOperation: {
            requestId: event.requestId,
            operation: event.operation,
          },
        }),
        event,
      );
    case "bridge/pong":
      // Liveness probes are consumed by useAcp before reducer dispatch. Keep
      // this harmless for callers that replay every validated server event.
      return state;
    case "bridge/phase":
      const terminalPhase = event.phase === "error" || event.phase === "stopped";
      return terminalPhase
        ? terminateBridgeState(
            state,
            event.phase === "error" ? "error" : "stopped",
            state.socketOpen,
          )
        : { ...state, phase: event.phase };
    case "bridge/stderr":
      return { ...state, stderr: tail(state.stderr + event.chunk, 80_000) };
    case "bridge/context_search_result":
    case "bridge/context_attached":
      // Request-scoped context responses are consumed by useAcp before dispatch.
      return state;
    case "bridge/error": {
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
          event.operation === "nes/accept" &&
          state.pendingNesAccept?.requestId === event.requestId
        ) {
          const restored = rollbackPendingNesAccept(state);
          return {
            ...restored,
            timeline: [
              ...restored.timeline,
              errorTimelineItem(event),
            ],
          };
        }
        if (
          event.operation === "nes/suggest" &&
          state.pendingNesSuggestion?.requestId === event.requestId
        ) {
          return {
            ...state,
            pendingNesSuggestion: undefined,
            nesError: event.message,
          };
        }
        if (event.operation === "nes/suggest") {
          return appendBackgroundEvent(state, event);
        }
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
    case "acp/initialized":
      const preserveRuntimeAuth = state.pendingAuth != null ||
        state.authTerminal != null ||
        state.authStatus === "authenticated" ||
        state.authStatus === "logged_out";
      return {
        ...state,
        initialized: event.response,
        authStatus: preserveRuntimeAuth
          ? state.authStatus
          : (event.response.authMethods?.length ?? 0) > 0
            ? "available"
            : undefined,
        pendingAuth: preserveRuntimeAuth ? state.pendingAuth : undefined,
        authTerminal: preserveRuntimeAuth ? state.authTerminal : undefined,
        authError: preserveRuntimeAuth ? state.authError : undefined,
        lastAuthResponse: preserveRuntimeAuth ? state.lastAuthResponse : undefined,
      };
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
    case "acp/session_created":
      if (
        state.sessionTransition?.kind !== "new" ||
        state.sessionTransition.requestId !== event.requestId
      ) {
        if (state.sessionTransition?.kind === "new") {
          return appendBackgroundEvent(state, event);
        }
        return openObservedSession(
          state,
          event.response,
          event.cwd,
          event.earlyUpdates,
        );
      }
      return applyEarlySessionUpdates({
        ...state,
        cwd: event.cwd ?? state.sessionTransition.targetCwd ?? state.defaultCwd,
        session: event.response,
        pendingSessionId: undefined,
        sessionTransition: undefined,
        modeId: event.response.modes?.currentModeId,
        configOptions: event.response.configOptions ?? [],
      }, event.response, event.earlyUpdates);
    case "acp/sessions_listed": {
      const listedActive = state.session == null
        ? undefined
        : event.response.sessions.find(
            ({ sessionId }) => sessionId === state.session?.sessionId,
          );
      return {
        ...state,
        sessions: event.cursor
          ? mergeSessions(state.sessions, event.response.sessions)
          : mergeSessions(
              state.sessions.filter(({ sessionId }) => state.cachedSessions.has(sessionId)),
              event.response.sessions,
            ),
        nextSessionCursor: event.response.nextCursor,
        title: state.title ?? listedActive?.title ?? undefined,
      };
    }
    case "acp/session_attached": {
      if (
        state.sessionTransition?.kind !== "attach" ||
        state.sessionTransition.requestId !== event.requestId ||
        state.sessionTransition.targetSessionId !== event.sessionId
      ) {
        if (state.sessionTransition?.kind === "attach") {
          return appendBackgroundEvent(state, event);
        }
        return openObservedSession(state, {
          ...event.response,
          sessionId: event.sessionId,
        }, event.cwd);
      }
      const listed = state.sessions.find(({ sessionId }) => sessionId === event.sessionId);
      const cachedSessions = new Map(state.cachedSessions);
      cachedSessions.delete(event.sessionId);
      return {
        ...state,
        cachedSessions,
        cwd: event.cwd ?? listed?.cwd ?? state.defaultCwd,
        session: { sessionId: event.sessionId, ...event.response },
        historyStatus: {
          state: "available",
          sessionId: event.sessionId,
        },
        pendingSessionId: undefined,
        sessionTransition: undefined,
        modeId: event.response.modes?.currentModeId,
        configOptions: event.response.configOptions ?? [],
        title: state.title ?? listed?.title ?? undefined,
      };
    }
    case "acp/session_forked": {
      const settled = clearRuntimeOperation(state, event.requestId);
      if (
        !isCurrentSession(settled, event.sourceSessionId) ||
        settled.sessionTransition?.kind !== "fork" ||
        settled.sessionTransition.requestId !== event.requestId ||
        settled.sessionTransition.targetSessionId !== event.sourceSessionId
      ) {
        if (settled.sessionTransition?.kind === "fork") {
          return appendBackgroundEvent(settled, event);
        }
        return openObservedSession(
          settled,
          event.response,
          event.cwd,
          event.earlyUpdates,
        );
      }
      return applyEarlySessionUpdates({
        ...settled,
        cwd: event.cwd ?? settled.cwd,
        session: event.response,
        pendingSessionId: undefined,
        sessionTransition: undefined,
        modeId: event.response.modes?.currentModeId,
        configOptions: event.response.configOptions ?? [],
        permissions: [],
        elicitations: requestScopedElicitations(settled.elicitations),
        running: false,
        agentActivity: undefined,
        pendingPrompt: undefined,
      }, event.response, event.earlyUpdates);
    }
    case "acp/session_closed":
      return removeAuthoritativeSessionRuntime(state, event.sessionId, false);
    case "acp/session_deleted":
      return removeAuthoritativeSessionRuntime(state, event.sessionId, true);
    case "acp/session_update":
      if (isCurrentSession(state, event.notification.sessionId)) {
        return reduceSessionUpdate(state, event.notification);
      }
      return updateCachedSession(
        state,
        event.notification.sessionId,
        (cached) => reduceSessionUpdate(cached, event.notification),
        event,
      );
    case "acp/terminal_state":
      if (isCurrentSession(state, event.terminal.sessionId)) {
        return {
          ...state,
          terminalSnapshots: upsertTerminalSnapshot(
            state.terminalSnapshots,
            event.terminal,
          ),
        };
      }
      return updateCachedSession(
        state,
        event.terminal.sessionId,
        (cached) => ({
          ...cached,
          terminalSnapshots: upsertTerminalSnapshot(
            cached.terminalSnapshots,
            event.terminal,
          ),
        }),
        event,
      );
    case "acp/prompt_started":
      if (isCurrentSession(state, event.sessionId)) {
        return startPrompt(state, event.requestId, event.sessionId, event.prompt);
      }
      return updateCachedSession(
        state,
        event.sessionId,
        (cached) => startPrompt(
          cached,
          event.requestId,
          event.sessionId,
          event.prompt,
        ),
        event,
      );
    case "acp/prompt_complete":
      if (isCurrentSession(state, event.sessionId)) {
        return completePrompt(state, event);
      }
      return updateCachedSession(
        state,
        event.sessionId,
        (cached) => completePrompt(cached, event),
        event,
      );
    case "acp/permission_request": {
      if (!isCurrentSession(state, event.request.sessionId)) {
        if (state.cachedSessions.has(event.request.sessionId)) {
          return markSessionAttention(updateCachedSession(
            state,
            event.request.sessionId,
            (cached) => reduceServerEvent(cached, event),
            event,
          ), event.request.sessionId);
        }
        return appendBackgroundEvent(state, event);
      }
      const { timeline } = upsertToolCall(
        state.timeline,
        event.request.toolCall,
        event,
        "permission",
      );
      return {
        ...state,
        timeline,
        permissions: [
          ...state.permissions.filter(
            ({ permissionId }) => permissionId !== event.permissionId,
          ),
          { permissionId: event.permissionId, request: event.request },
        ],
      };
    }
    case "acp/permission_resolved": {
      const pending = state.permissions.find(
        ({ permissionId }) => permissionId === event.permissionId,
      );
      if (pending) return removePermission(state, event.permissionId);
      for (const [sessionId, snapshot] of state.cachedSessions) {
        const cachedPending = snapshot.permissions.find(
          ({ permissionId }) => permissionId === event.permissionId,
        );
        if (!cachedPending) continue;
        return clearSessionAttentionIfSettled(updateCachedSession(
          state,
          sessionId,
          (cached) => removePermission(cached, event.permissionId),
          event,
        ), sessionId);
      }
      return appendBackgroundEvent(state, event);
    }
    case "acp/elicitation_request": {
      const scopedSessionId = elicitationSessionId(event.request);
      if (scopedSessionId != null && !isCurrentSession(state, scopedSessionId)) {
        if (state.cachedSessions.has(scopedSessionId)) {
          return markSessionAttention(updateCachedSession(
            state,
            scopedSessionId,
            (cached) => reduceServerEvent(cached, event),
            event,
          ), scopedSessionId);
        }
        return appendBackgroundEvent(state, event);
      }
      return {
        ...state,
        elicitations: [
          ...state.elicitations.filter(
            ({ elicitationId }) => elicitationId !== event.elicitationId,
          ),
          { elicitationId: event.elicitationId, request: event.request },
        ],
      };
    }
    case "acp/elicitation_complete":
      return completeExternalFlow(state, event.notification.elicitationId);
    case "acp/elicitation_resolved": {
      const pending = state.elicitations.find(
        ({ elicitationId }) => elicitationId === event.elicitationId,
      );
      if (pending) return resolveElicitation(state, event.elicitationId, event.response);
      for (const [sessionId, snapshot] of state.cachedSessions) {
        const cachedPending = snapshot.elicitations.find(
          ({ elicitationId }) => elicitationId === event.elicitationId,
        );
        if (!cachedPending) continue;
        return clearSessionAttentionIfSettled(updateCachedSession(
          state,
          sessionId,
          (cached) => resolveElicitation(
            cached,
            event.elicitationId,
            event.response,
          ),
          event,
        ), sessionId);
      }
      return appendBackgroundEvent(state, event);
    }
    case "acp/elicitation_aborted": {
      const flow = state.externalFlows.find(
        ({ elicitationId }) => elicitationId === event.elicitationId,
      );
      if (flow?.sessionId !== event.sessionId) {
        return appendBackgroundEvent(state, event);
      }
      return abortExternalFlow(
        state,
        event.elicitationId,
        event.sessionId,
        event.reason,
      );
    }
    case "acp/mode_changed":
      if (!isCurrentSession(state, event.sessionId)) {
        return updateCachedSession(
          state,
          event.sessionId,
          (cached) => ({
            ...cached,
            modeId: event.modeId,
            runtimeOperation: undefined,
          }),
          event,
        );
      }
      if (
        !state.runtimeReplaying &&
        state.pendingSessionControl != null &&
        (
        state.pendingSessionControl?.kind !== "mode" ||
        state.pendingSessionControl.requestId !== event.requestId ||
        state.pendingSessionControl.sessionId !== event.sessionId
        )
      ) return appendBackgroundEvent(state, event);
      return {
        ...state,
        modeId: event.modeId,
        pendingSessionControl: undefined,
        runtimeOperation: undefined,
      };
    case "acp/config_changed":
      if (!isCurrentSession(state, event.sessionId)) {
        return updateCachedSession(
          state,
          event.sessionId,
          (cached) => ({
            ...cached,
            configOptions: event.response.configOptions,
            runtimeOperation: undefined,
          }),
          event,
        );
      }
      if (
        !state.runtimeReplaying &&
        state.pendingSessionControl != null &&
        (
        state.pendingSessionControl?.kind !== "config" ||
        state.pendingSessionControl.requestId !== event.requestId ||
        state.pendingSessionControl.sessionId !== event.sessionId
        )
      ) {
        return appendBackgroundEvent(state, event);
      }
      return {
        ...state,
        configOptions: event.response.configOptions,
        pendingSessionControl: undefined,
        runtimeOperation: undefined,
      };
    case "acp/mcp_connection":
      return {
        ...state,
        mcpConnections: event.action === "connected"
          ? [
              ...state.mcpConnections.filter(
                ({ connectionId }) => connectionId !== event.connectionId,
              ),
              {
                serverId: event.serverId,
                connectionId: event.connectionId,
                name: event.name,
              },
            ]
          : state.mcpConnections.filter(
              ({ connectionId }) => connectionId !== event.connectionId,
            ),
      };
    case "acp/mcp_message":
      return {
        ...state,
        mcpActivity: [...state.mcpActivity, event].slice(-100),
      };
    case "acp/nes_started":
      return {
        ...state,
        nesSessionId: event.response.sessionId,
        nesDocuments: [],
        nesDrafts: {},
        pendingNesSuggestion: undefined,
        pendingNesAccept: undefined,
        nesError: undefined,
        activeNesUri: undefined,
        nesSuggestions: [],
      };
    case "acp/nes_suggestions":
      if (
        state.nesSessionId !== event.sessionId ||
        state.pendingNesSuggestion?.requestId !== event.requestId ||
        state.pendingNesSuggestion.uri !== event.uri
      ) return appendBackgroundEvent(state, event);
      return {
        ...state,
        nesSuggestions: event.response.suggestions,
        pendingNesSuggestion: undefined,
        nesError: undefined,
      };
    case "acp/nes_suggestion_resolved": {
      if (state.nesSessionId !== event.sessionId) {
        return appendBackgroundEvent(state, event);
      }
      const next = {
        ...state,
        nesSuggestions: state.nesSuggestions.filter(
          ({ id }) => id !== event.suggestionId,
        ),
      };
      const pending = state.pendingNesAccept;
      if (
        pending?.requestId !== event.requestId ||
        pending.sessionId !== event.sessionId ||
        pending.suggestionId !== event.suggestionId
      ) return next;
      if (event.outcome === "rejected") return rollbackPendingNesAccept(next);
      if (pending.optimisticText != null && !pending.documentAcknowledged) {
        const restored = rollbackPendingNesAccept(next);
        return {
          ...restored,
          timeline: [
            ...restored.timeline,
            {
              id: randomId(),
              type: "error",
              message: "NES edit was accepted without a matching document acknowledgement",
            },
          ],
        };
      }
      return { ...next, pendingNesAccept: undefined };
    }
    case "acp/nes_closed":
      return state.nesSessionId === event.sessionId
        ? {
            ...state,
            nesSessionId: undefined,
            nesDocuments: [],
            nesDrafts: {},
            pendingNesSuggestion: undefined,
            pendingNesAccept: undefined,
            nesError: undefined,
            activeNesUri: undefined,
            nesSuggestions: [],
          }
        : appendBackgroundEvent(state, event);
    case "acp/document_opened":
      return state.nesSessionId === event.document.sessionId
        ? {
            ...state,
            nesDocuments: upsertNesDocument(state.nesDocuments, event.document),
            activeNesUri: event.document.uri,
          }
        : appendBackgroundEvent(state, event);
    case "acp/document_changed":
      if (state.nesSessionId !== event.document.sessionId) {
        return appendBackgroundEvent(state, event);
      }
      const pendingAccept = state.pendingNesAccept;
      const acknowledgesPendingAccept =
        pendingAccept?.requestId === event.requestId &&
        pendingAccept.sessionId === event.document.sessionId &&
        pendingAccept.uri === event.document.uri &&
        pendingAccept.optimisticText === event.document.text;
      return {
        ...state,
        nesDocuments: upsertNesDocument(state.nesDocuments, event.document),
        nesDrafts: clearAcknowledgedDraft(
          state.nesDrafts,
          event.document.uri,
          event.document.text,
        ),
        pendingNesAccept: acknowledgesPendingAccept
          ? { ...pendingAccept, documentAcknowledged: true }
          : pendingAccept,
        activeNesUri: event.document.uri,
      };
    case "acp/document_saved":
    case "acp/document_focused": {
      const sessionId = event.type === "acp/document_saved"
        ? event.sessionId
        : event.notification.sessionId;
      return state.nesSessionId === sessionId
        ? state
        : appendBackgroundEvent(state, event);
    }
    case "acp/document_closed":
      if (state.nesSessionId !== event.sessionId) {
        return appendBackgroundEvent(state, event);
      }
      return removeNesDocument(state, event.uri);
  }

  return assertNever(event, "attyd server event reducer");
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
  return {
    ...state,
    running: true,
    agentActivity: { kind: "waiting" },
    pendingPrompt: { requestId, sessionId, blocks },
    activePlan: planWithoutCompletedEntries(state.activePlan),
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

function completePrompt(
  state: AppState,
  event: Extract<ServerEvent, { type: "acp/prompt_complete" }>,
): AppState {
  if (
    state.session?.sessionId !== event.sessionId ||
    state.pendingPrompt?.requestId !== event.requestId ||
    state.pendingPrompt.sessionId !== event.sessionId
  ) return appendBackgroundEvent(state, event);
  const completedPlan = event.response.stopReason !== "cancelled" &&
    isCompletedLegacyPlan(state.activePlan)
    ? state.activePlan
    : undefined;
  return {
    ...state,
    running: false,
    agentActivity: undefined,
    pendingPrompt: undefined,
    activePlan: completedPlan ? undefined : state.activePlan,
    timeline: [
      ...state.timeline,
      ...(completedPlan ? [completedPlan] : []),
      { id: randomId(), type: "stop", response: event.response },
    ],
  };
}

function failPrompt(
  state: AppState,
  event: Extract<ServerEvent, { type: "bridge/error" }>,
): AppState {
  const pendingPrompt = state.pendingPrompt;
  if (pendingPrompt == null || pendingPrompt.requestId !== event.requestId) return state;
  const retryBlocks = pendingPrompt.blocks;
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
      timeline,
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
      timeline,
      agentActivity: state.running
        ? toolActivity(call)
        : state.agentActivity,
    };
  }

  if (update.sessionUpdate === "plan") {
    return {
      ...state,
      activePlan: {
        id: state.activePlan?.id ?? randomId(),
        type: "plan",
        update,
        raw: [...(state.activePlan?.raw ?? []), notification],
      },
      agentActivity: state.running ? { kind: "planning" } : state.agentActivity,
    };
  }

  if (update.sessionUpdate === "plan_update") {
    const planId = update.plan.planId;
    const id = `plan:${planId}`;
    const index = state.timeline.findIndex((item) => item.id === id);
    const current = index >= 0 ? state.timeline[index] : undefined;
    const item: TimelineItem = {
      id,
      type: "plan",
      update,
      raw: [...(current?.type === "plan" ? current.raw : []), notification],
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
    const id = `plan:${update.planId}`;
    const index = state.timeline.findIndex((item) => item.id === id);
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
      id,
      type: "plan",
      update,
      raw: [...current.raw, notification],
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

function applyEarlySessionUpdates(
  state: AppState,
  response: NewSessionResponse | ForkSessionResponse,
  updates: SessionNotification[] | undefined,
): AppState {
  let next = state;
  for (const notification of updates ?? []) {
    next = reduceSessionUpdate(next, notification);
  }
  return {
    ...next,
    session: response,
    modeId: response.modes?.currentModeId,
    configOptions: response.configOptions ?? [],
  };
}

function upsertToolCall(
  timeline: TimelineItem[],
  update: ToolCall | ToolCallUpdate,
  raw: unknown,
  source: "create" | "update" | "permission",
): { timeline: TimelineItem[]; call: ToolCall } {
  const id = `tool:${update.toolCallId}`;
  const index = timeline.findIndex(
    (item) => item.type === "tool" && item.call.toolCallId === update.toolCallId,
  );
  const current = index >= 0 ? timeline[index] : undefined;
  const previous = current?.type === "tool" ? current : undefined;

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
    raw: [...(previous?.raw ?? []), raw],
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
  const id = `compaction:${update.compactionId}`;
  const index = state.timeline.findIndex((item) => item.id === id);
  const current = index >= 0 ? state.timeline[index] : undefined;
  const previous = current?.type === "compaction" ? current : undefined;

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
    raw: [...(previous?.raw ?? []), notification],
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
  const last = timeline.at(-1);

  // Match Zed's optimistic prompt echo handling: an Agent may replay the
  // prompt it just received. Keep one user entry while retaining the raw ACP
  // notification and the Agent-owned protocol ID for inspection.
  if (
    last?.type === "message" &&
    last.role === "user" &&
    last.raw.length === 0 &&
    last.blocks.some((candidate) => contentBlockEqual(candidate, block)) &&
    canMergeMessageIds(last.messageId, messageId)
  ) {
    const next = [...timeline];
    next[next.length - 1] = {
      ...last,
      messageId: last.messageId ?? messageId,
      turnOperationId: last.turnOperationId ?? turnOperationId,
      raw: [raw],
    };
    return next;
  }

  if (
    last?.type === "message" &&
    last.role === "protocol-user" &&
    canMergeMessageIds(last.messageId, messageId) &&
    canMergeTurnOperationIds(last.turnOperationId, turnOperationId)
  ) {
    const next = [...timeline];
    next[next.length - 1] = {
      ...last,
      messageId: last.messageId ?? messageId,
      turnOperationId: last.turnOperationId ?? turnOperationId,
      blocks: mergeTextBlock(last.blocks, block),
      raw: [...last.raw, raw],
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
  const last = timeline.at(-1);
  if (last?.type !== "assistant") {
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
      raw: [...previous.raw, raw],
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

function planWithoutCompletedEntries(
  plan: AppState["activePlan"],
): AppState["activePlan"] {
  if (!plan || plan.update.sessionUpdate !== "plan") return plan;
  const entries = plan.update.entries.filter(({ status }) => status !== "completed");
  if (entries.length === plan.update.entries.length) return plan;
  if (entries.length === 0) return undefined;
  return { ...plan, update: { ...plan.update, entries } };
}

function isCompletedLegacyPlan(
  plan: AppState["activePlan"],
): plan is Extract<TimelineItem, { type: "plan" }> {
  return plan?.update.sessionUpdate === "plan" &&
    plan.update.entries.length > 0 &&
    plan.update.entries.every(({ status }) => status === "completed");
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
  if (last?.type === "text" && incoming.type === "text") {
    return [...blocks.slice(0, -1), { ...last, text: last.text + incoming.text }];
  }
  return [...blocks, incoming];
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
  const current = rollbackSessionTransition(state);
  return {
    ...current,
    phase,
    socketOpen,
    runtimeReplaying: false,
    runtimeReplacement: undefined,
    sessionLoadReplacement: undefined,
    cachedSessions: new Map(),
    attentionSessionIds: [],
    running: false,
    agentActivity: undefined,
    mcpConnections: [],
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
    nesSessionId: undefined,
    nesDocuments: [],
    nesDrafts: {},
    pendingNesSuggestion: undefined,
    pendingNesAccept: undefined,
    nesError: undefined,
    activeNesUri: undefined,
    nesSuggestions: [],
    terminalSnapshots: current.terminalSnapshots.map((terminal) => ({
      ...terminal,
      released: true,
    })),
  };
}

function appendAuthTerminalOutput(state: AppState, data: string): AppState {
  if (!state.authTerminal) return state;
  const maxCharacters = 2_000_000;
  const output = state.authTerminal.output + data;
  const truncated = state.authTerminal.truncated || output.length > maxCharacters;
  return {
    ...state,
    authTerminal: {
      ...state.authTerminal,
      output: output.length > maxCharacters ? output.slice(-maxCharacters) : output,
      truncated,
    },
  };
}

function isAuthenticationRequired(
  state: AppState,
  event: Extract<ServerEvent, { type: "bridge/error" }>,
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
          ...cached,
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
      pendingNesAccept: next.pendingNesAccept?.requestId === requestId
        ? undefined
        : next.pendingNesAccept,
      pendingNesSuggestion: next.pendingNesSuggestion?.requestId === requestId
        ? undefined
        : next.pendingNesSuggestion,
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
    historyStatus: undefined,
    pendingSessionId: undefined,
    sessionTransition: undefined,
    modeId: undefined,
    configOptions: [],
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
    historyStatus: view.historyRevision == null
      ? {
          state: "loading",
          sessionId: view.sessionId,
          requestId: `bridge:${view.sessionIncarnation}:${view.viewRevision}`,
        }
      : { state: "available", sessionId: view.sessionId },
    sessionSyncPhase: view.phase,
    modeId: session.modes?.currentModeId,
    configOptions: session.configOptions ?? [],
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
  for (const [index, update] of view.timeline.entries()) {
    next = reduceSessionUpdate(next, { sessionId: view.sessionId, update });
    for (const outcome of outcomesByOffset.get(index + 1) ?? []) {
      const id = `bridge-turn-outcome:${outcome.operationId}`;
      if (!next.timeline.some((item) => item.id === id)) {
        next = {
          ...next,
          timeline: [...next.timeline, { id, type: "stop", response: outcome.response }],
        };
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
    if (view.phase !== "running") {
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
  return {
    ...state,
    running: false,
    sessionSyncPhase: event.phase,
    agentActivity: undefined,
    pendingPrompt: undefined,
    timeline: [
      ...state.timeline,
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

function cacheRuntimeSession(
  state: AppState,
  event: Extract<ServerEvent, { type: "bridge/runtime_session" }>,
): AppState {
  const snapshot = runtimeSessionSnapshot(state, event);
  const sessions = mergeSessions(state.sessions, [{
    sessionId: event.sessionId,
    cwd: snapshot.cwd,
  }]);
  if (state.session?.sessionId === event.sessionId) {
    const title = state.title;
    const reset = resetActiveSession(state);
    return {
      ...reset,
      ...snapshot,
      title: snapshot.title ?? title,
      cachedSessions: withoutCachedSession(
        state.cachedSessions,
        event.sessionId,
      ),
      sessions,
      elicitations: [
        ...snapshot.elicitations,
        ...requestScopedElicitations(state.elicitations),
      ],
    };
  }
  const cachedSessions = new Map(state.cachedSessions);
  cachedSessions.set(event.sessionId, snapshot);
  return {
    ...state,
    cachedSessions,
    sessions,
  };
}

function runtimeSessionSnapshot(
  state: AppState,
  event: Extract<ServerEvent, { type: "bridge/runtime_session" }>,
): ActiveSessionSnapshot {
  return {
    cwd: event.cwd || state.defaultCwd,
    session: event.session,
    availableCommands: [],
    modeId: event.session.modes?.currentModeId,
    configOptions: event.session.configOptions ?? [],
    timeline: event.truncated
      ? [{
          id: randomId(),
          type: "error",
          message: "Earlier in-memory runtime events were omitted because the replay limit was reached",
        }]
      : [],
    permissions: [],
    elicitations: [],
    running: false,
    terminalSnapshots: [],
  };
}

function openObservedSession(
  state: AppState,
  response: NewSessionResponse | ForkSessionResponse,
  cwd?: string,
  earlyUpdates?: SessionNotification[],
): AppState {
  const sessionId = response.sessionId;
  let next = cacheRuntimeSession(state, {
    type: "bridge/runtime_session",
    sessionId,
    cwd: cwd ?? state.defaultCwd,
    session: response,
    truncated: false,
  });
  for (const notification of earlyUpdates ?? []) {
    next = reduceServerEvent(next, {
      type: "acp/session_update",
      notification,
    });
  }
  return state.session == null && state.sessionTransition == null
    ? activateCachedSession(next, sessionId)
    : next;
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
): AppState {
  let next = state;
  if (next.sessionTransition?.targetSessionId === sessionId) {
    next = rollbackSessionTransition(next);
  }
  if (next.session?.sessionId === sessionId) {
    next = resetActiveSession(next);
  } else if (next.sessionTransition?.backup.session?.sessionId === sessionId) {
    next = {
      ...next,
      sessionTransition: {
        ...next.sessionTransition,
        backup: captureActiveSession(resetActiveSession(next)),
      },
    };
  }
  next = removeCachedSession(next, sessionId);
  return {
    ...next,
    sessions: deleted
      ? next.sessions.filter((session) => session.sessionId !== sessionId)
      : next.sessions,
    pendingSessionDeletions: deleted
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
  return {
    cwd: state.cwd,
    session: state.session,
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
    sessionLoadReplacement: undefined,
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

function removePermission(state: AppState, permissionId: string): AppState {
  return {
    ...state,
    permissions: state.permissions.filter(
      (permission) => permission.permissionId !== permissionId,
    ),
  };
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

function markSessionAttention(state: AppState, sessionId: string): AppState {
  return state.attentionSessionIds.includes(sessionId)
    ? state
    : {
        ...state,
        attentionSessionIds: [...state.attentionSessionIds, sessionId].slice(-32),
      };
}

function clearSessionAttentionIfSettled(state: AppState, sessionId: string): AppState {
  const session = state.cachedSessions.get(sessionId);
  if ((session?.permissions.length ?? 0) > 0 || (session?.elicitations.length ?? 0) > 0) {
    return state;
  }
  return {
    ...state,
    attentionSessionIds: state.attentionSessionIds.filter(
      (candidate) => candidate !== sessionId,
    ),
  };
}

function isCurrentSession(state: AppState, sessionId: string): boolean {
  return (state.session?.sessionId ?? state.pendingSessionId) === sessionId;
}

function errorTimelineItem(
  event: Extract<ServerEvent, { type: "bridge/error" }>,
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
    backgroundEvents: [...state.backgroundEvents, event].slice(-100),
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
  const retainedOutput = incoming.retainedBytes != null && output.length > incoming.retainedBytes
    ? output.slice(-incoming.retainedBytes)
    : output;
  const merged = { ...incoming, output: retainedOutput };
  return [
    ...snapshots.filter(({ terminalId }) => terminalId !== incoming.terminalId),
    merged,
  ].slice(-64);
}

function resolveElicitation(
  state: AppState,
  bridgeElicitationId: string,
  response: CreateElicitationResponse,
): AppState {
  const pending = state.elicitations.find(
    ({ elicitationId }) => elicitationId === bridgeElicitationId,
  );
  let externalFlows = state.externalFlows;
  if (
    response.action === "accept" &&
    pending?.request.mode === "url" &&
    "elicitationId" in pending.request &&
    typeof pending.request.elicitationId === "string" &&
    "url" in pending.request &&
    typeof pending.request.url === "string"
  ) {
    const agentElicitationId = pending.request.elicitationId;
    const flow: ExternalElicitationFlow = {
      elicitationId: agentElicitationId,
      sessionId: elicitationSessionId(pending.request),
      url: pending.request.url,
      message: pending.request.message,
      status: externalFlows.some(
        ({ elicitationId, status }) =>
          elicitationId === agentElicitationId && status === "completed",
      )
        ? "completed"
        : "waiting",
    };
    externalFlows = upsertExternalFlow(externalFlows, flow);
  }
  return {
    ...state,
    externalFlows,
    elicitations: state.elicitations.filter(
      ({ elicitationId }) => elicitationId !== bridgeElicitationId,
    ),
  };
}

function completeExternalFlow(state: AppState, elicitationId: string): AppState {
  const existing = state.externalFlows.find(
    (flow) => flow.elicitationId === elicitationId,
  );
  if (existing?.status === "cancelled") return state;
  return {
    ...state,
    externalFlows: upsertExternalFlow(state.externalFlows, {
      elicitationId,
      sessionId: existing?.sessionId,
      url: existing?.url,
      message: existing?.message ?? "External flow",
      status: "completed",
    }),
  };
}

function abortExternalFlow(
  state: AppState,
  elicitationId: string,
  sessionId: string,
  reason: Extract<ServerEvent, { type: "acp/elicitation_aborted" }>["reason"],
): AppState {
  const existing = state.externalFlows.find(
    (flow) => flow.elicitationId === elicitationId,
  );
  if (existing?.status === "completed") return state;
  return {
    ...state,
    externalFlows: upsertExternalFlow(state.externalFlows, {
      elicitationId,
      sessionId,
      url: existing?.url,
      message: existing?.message ?? "External flow",
      status: "cancelled",
      abortReason: reason,
    }),
  };
}

function upsertExternalFlow(
  flows: ExternalElicitationFlow[],
  next: ExternalElicitationFlow,
): ExternalElicitationFlow[] {
  const index = flows.findIndex(
    ({ elicitationId }) => elicitationId === next.elicitationId,
  );
  if (index < 0) return [...flows, next].slice(-100);
  const result = [...flows];
  result[index] = next;
  return result;
}

function mergeSessions(current: SessionInfo[], incoming: SessionInfo[]): SessionInfo[] {
  const sessions = new Map(current.map((session) => [session.sessionId, session]));
  for (const session of incoming) sessions.set(session.sessionId, session);
  return [...sessions.values()];
}

function upsertNesDocument(
  documents: NesDocumentState[],
  next: NesDocumentState,
): NesDocumentState[] {
  const index = documents.findIndex(({ uri }) => uri === next.uri);
  if (index < 0) return [...documents, next];
  const result = [...documents];
  result[index] = next;
  return result;
}

function removeNesDocument(state: AppState, uri: string): AppState {
  const documents = state.nesDocuments.filter((document) => document.uri !== uri);
  return {
    ...state,
    nesDocuments: documents,
    nesDrafts: omitKey(state.nesDrafts, uri),
    pendingNesSuggestion: state.pendingNesSuggestion?.uri === uri
      ? undefined
      : state.pendingNesSuggestion,
    pendingNesAccept: state.pendingNesAccept?.uri === uri
      ? undefined
      : state.pendingNesAccept,
    activeNesUri: state.activeNesUri === uri ? documents.at(-1)?.uri : state.activeNesUri,
    nesSuggestions: state.nesSuggestions.filter((suggestion) => suggestion.uri !== uri),
  };
}

function rollbackPendingNesAccept(state: AppState): AppState {
  const pending = state.pendingNesAccept;
  if (!pending) return state;
  let nesDrafts = state.nesDrafts;
  if (
    pending.uri != null &&
    pending.optimisticText != null &&
    nesDrafts[pending.uri] === pending.optimisticText
  ) {
    nesDrafts = pending.previousDraft == null
      ? omitKey(nesDrafts, pending.uri)
      : { ...nesDrafts, [pending.uri]: pending.previousDraft };
  }
  return { ...state, nesDrafts, pendingNesAccept: undefined };
}

function clearAcknowledgedDraft(
  drafts: Record<string, string>,
  uri: string,
  acknowledgedText: string,
): Record<string, string> {
  return drafts[uri] === acknowledgedText ? omitKey(drafts, uri) : drafts;
}

function omitKey<T>(record: Record<string, T>, key: string): Record<string, T> {
  if (!(key in record)) return record;
  const next = { ...record };
  delete next[key];
  return next;
}

function tail(value: string, max: number): string {
  return value.length > max ? value.slice(value.length - max) : value;
}
