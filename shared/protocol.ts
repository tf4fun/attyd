// Generated from src/protocol by `npm run protocol:types`. Do not edit.
// Protocol v2: docs/protocol.md.

import type {
  AvailableCommand,
  ContentBlock,
  Cost,
  CreateElicitationRequest,
  CreateElicitationResponse,
  InitializeResponse,
  PromptResponse,
  RequestPermissionOutcome,
  RequestPermissionRequest,
  SessionConfigOption,
  SessionMode,
  SessionUpdate,
} from "@agentclientprotocol/sdk";

export type Position = { epoch: string, seq: number, };

export type GlobalState = { agent: AgentState, catalog: CatalogState, sessions: { [key in string]: SessionSummary }, requests: { [key in string]: Request }, interactions: { [key in string]: Interaction }, urlFlows: { [key in string]: UrlFlow }, };

export type AgentState = { phase: AgentPhase, error: ProtocolError | null, launch: AgentLaunch, initialize: InitializeResponse | null, authenticatedMethodId: string | null, predecessor: EpochPredecessor | null, };

export type AgentPhase = "starting" | "initializing" | "ready" | "stopped" | "error";

export type AgentLaunch = { transport: AgentTransport, command: Array<string>, cwd: string, readOnly: boolean, additionalDirectories: Array<string>, mcpServers: Array<McpServerSummary>, };

export type AgentTransport = "stdio" | "http" | "ws";

export type McpServerSummary = { name: string, type: McpServerKind, };

export type McpServerKind = "stdio" | "http" | "sse" | "acp";

export type EpochPredecessor = { epoch: string, reason: EpochEndReason, };

export type EpochEndReason = "agent_exit" | "agent_error" | "auth_restart" | "shutdown";

export type CatalogState = { revision: string, };

export type SessionSummary = { sessionId: string, incarnation: number, cwd: string, title: string | null, lifecycle: SessionLifecycle, phase: SessionPhase, work: SessionWork, pending: PendingCounts, };

export type SessionLifecycle = "attaching" | "active" | "closing" | "closing_for_delete" | "deleting" | "closed" | "uncertain";

export type SessionWork = "idle" | "running" | "awaiting_input";

export type PendingCounts = { permissions: number, elicitations: number, urlFlows: number, };

export type Request = { requestId: string, operation: RequestOperation, 
/**
 * The ACP method, or `attyd/auth_terminal`.
 */
method: string, clientIntentId: string | null, target: RequestTarget | null, interactionId: string | null, methodId: string | null, 
/**
 * Non-secret parameters that tell two submissions apart (protocol §3.1).
 */
params: Record<string, unknown>, status: RequestStatus, result?: RequestResult, error?: ProtocolError, terminal?: AuthTerminalState, };

export type RequestOperation = "open" | "create" | "fork" | "set_mode" | "set_config" | "close" | "delete" | "authenticate" | "logout" | "auth_terminal" | "interaction_response" | "agent_request";

export type RequestTarget = { sessionId: string, incarnation: number | null, };

export type RequestStatus = "running" | "succeeded" | "failed" | "cancelled";

export type RequestResult = { "kind": "session", sessionId: string, incarnation: number, } | { "kind": "done" };

export type AuthTerminalState = { output: string, truncated: boolean, exitCode: number | null, signal: number | null, message: string | null, };

export type SessionState = { sessionId: string, incarnation: number, cwd: string, controls: SessionControls, history: SessionHistory, turn: ActiveTurn | null, outcomes: { [key in string]: TurnOutcome }, operation: SessionOperation | null, interactions: { [key in string]: Interaction }, urlFlows: { [key in string]: UrlFlow }, terminals: { [key in string]: TerminalState }, };

export type SessionPhase = "cold" | "loading" | "ready" | "running" | "reconciling" | "blocked";

export type SessionHistory = { revision: string | null, phase: SessionPhase, error: string | null, notice: string | null, 
/**
 * Completed history with process bodies collapsed (protocol §3.2).
 */
timeline: SessionUpdate[], collapsedTurns: Array<CollapsedTurn>, };

export type CollapsedTurn = { turnId: string, operationId: string | null, beforeUpdate: number, afterUpdate: number, processCount: number, visibleRanges: Array<TimelineRange>, };

export type TimelineRange = { start: number, end: number, };

export type ActiveTurn = { operationId: string, clientIntentId: string, prompt: ContentBlock[], 
/**
 * Folded overlay; entries may change in place.
 */
updates: SessionUpdate[], cancelRequested: boolean, };

export type TurnOutcome = { operationId: string, clientIntentId: string, status: TurnOutcomeStatus, response?: PromptResponse, error?: ProtocolError, 
/**
 * Failed turns keep the exact prompt for a retry.
 */
prompt?: ContentBlock[], 
/**
 * The `timeline` index this outcome follows.
 */
afterUpdate: number, };

export type TurnOutcomeStatus = "completed" | "failed";

export type SessionOperation = { operationId: string, kind: OperationKind, stage: string, 
/**
 * The request record, when a command started the operation.
 */
requestId: string | null, };

export type OperationKind = "load" | "resume" | "fork" | "close" | "delete" | "set_mode" | "set_config";

export type Interaction = { "kind": "permission", interactionId: string, request: RequestPermissionRequest, operationId: string | null, } | { "kind": "elicitation", interactionId: string, request: CreateElicitationRequest, operationId: string | null, 
/**
 * The parent request; global scope only.
 */
requestId?: string, };

export type UrlFlow = { interactionId: string, 
/**
 * Agent-chosen; a later flow may reuse it.
 */
elicitationId: string, 
/**
 * The parent request; global scope only.
 */
requestId?: string, request: CreateElicitationRequest, status: UrlFlowStatus, };

export type UrlFlowStatus = "waiting" | "completed" | "cancelled";

export type TerminalState = { terminalId: string, 
/**
 * Bounded tail; moves forward with `trim` plus `append`.
 */
output: string, truncated: boolean, exitStatus: TerminalExitStatus | null, released: boolean, };

export type TerminalExitStatus = { exitCode: number | null, signal: string | null, };

export type SessionControls = { modes: SessionModes | null, 
/**
 * `null`: never provided; `[]`: provided empty. Preferred over `modes`
 * whenever non-null.
 */
configOptions: SessionConfigOption[] | null, commands: AvailableCommand[], info: SessionInfo, usage: SessionUsage | null, };

export type SessionModes = { currentModeId: string, availableModes: SessionMode[], };

export type SessionInfo = { title: string | null, updatedAt: string | null, meta: unknown | null, };

export type SessionUsage = { used: number, size: number, cost: Cost | null, };

export type Entry = { "type": "snapshot", scope: string, at: Position, state: ScopeState, } | { "type": "reset", scope: string, at: Position, } | { "type": "ops", scope: string, at: Position, 
/**
 * The `seq` of the previous entry delivered for this scope.
 */
prev: number, ops: Array<Op>, } | { "type": "end", scope: string, at: Position, reason: EndReason, } | { "type": "position", at: Position, };

export type ScopeState = GlobalState | SessionState;

export type EndReason = "closed" | "deleted" | "unobserved" | "replaced" | "gone";

export type Op = { "op": "set", path: string, value: unknown, } | { "op": "remove", path: string, } | { "op": "append", path: string, } & ({ values: unknown[], } | { text: string, }) | { "op": "trim", path: string, count: number, };

export type AppendTail = { values: unknown[], } | { text: string, };

export type CommandResult<T> = { 
/**
 * The effect is committed at or before this position.
 */
at: Position, scopes: Array<string>, result: T, };

export type Disposition = "accepted" | "duplicate";

export type RequestAccepted = { requestId: string, disposition: Disposition, 
/**
 * A duplicate whose record is no longer public; `status` is final.
 */
resultExpired?: boolean, status?: RequestStatus, };

export type TurnAccepted = { operationId: string, disposition: Disposition, resultExpired?: boolean, status?: TurnOutcomeStatus, };

export type EmptyBody = Record<symbol, never>;

export type AuthenticateBody = { methodId: string, };

export type AuthTerminalStartBody = { methodId: string, cols: number, rows: number, };

export type AuthTerminalInputBody = { data: string, };

export type TerminalSizeBody = { cols: number, rows: number, };

export type SessionCwdBody = { cwd: string, };

export type TurnBody = { prompt: ContentBlock[], };

export type SetModeBody = { modeId: string, };

export type SetConfigBody = { value: unknown, };

export type InteractionResponseBody = { outcome: RequestPermissionOutcome, } | { response: CreateElicitationResponse, };

export type Snapshot<T> = { at: Position, state: T, };

export type ProtocolError = { code: ErrorCode, message: string, 
/**
 * `agent_error`: the ACP error `{ code, message, data }`.
 */
data?: unknown, };

export type ErrorCode = "invalid_request" | "not_found" | "owner_mismatch" | "history_mismatch" | "precondition_required" | "conflict" | "intent_conflict" | "catalog_changed" | "unsupported" | "gone" | "agent_error" | "unavailable" | "intent_capacity" | "interaction_ended";

export type ErrorBody = { error: ProtocolError, };
