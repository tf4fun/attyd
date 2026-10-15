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

export type SessionControls = { modes: SessionModes | null, 
/**
 * `null`: never provided; `[]`: provided empty. Preferred over `modes`
 * whenever non-null.
 */
configOptions: SessionConfigOption[] | null, commands: AvailableCommand[], info: SessionInfo, usage: SessionUsage | null, };

export type SessionModes = { currentModeId: string, availableModes: SessionMode[], };

export type SessionInfo = { title: string | null, updatedAt: string | null, meta: unknown | null, };

export type SessionUsage = { used: number, size: number, cost: Cost | null, };

export type Op = { "op": "set", path: string, value: unknown, } | { "op": "remove", path: string, } | { "op": "append", path: string, } & ({ values: unknown[], } | { text: string, }) | { "op": "trim", path: string, count: number, };

export type AppendTail = { values: unknown[], } | { text: string, };

export type Turn = { 
/**
 * The user's message, normalized.
 */
prompt: Array<Block>, 
/**
 * The Agent's message content, in order, normalized.
 */
reply: Array<Block>, };

export type Block = AttachmentRef | Extract<ContentBlock, { type: "text" | "resource_link" }>;

export type PromptBlock = AttachmentRef | ContentBlock;

export type AttachmentRef = { type: "attachment", 
/**
 * Lowercase hex SHA-256 of the block's canonical JSON.
 */
id: string, block: AttachmentKind, mimeType?: string, 
/**
 * A resource's URI.
 */
uri?: string, };

export type AttachmentKind = "image" | "audio" | "resource";

export type LogEntry = { turn: Turn, 
/**
 * `h_i` in its wire form (protocol §3.3).
 */
hash: string, };

export type ProtocolError = { code: ErrorCode, message: string, 
/**
 * `agent_error`: the ACP error `{ code, message, data }`.
 */
data?: unknown, };

export type ErrorCode = "invalid_request" | "not_found" | "stale" | "not_allowed" | "catalog_changed" | "unsupported" | "unavailable" | "agent_error";

export type OperationKind = "prompt" | "set_mode" | "set_config" | "fork" | "create" | "delete" | "authenticate" | "logout" | "terminal_login" | "list" | "load" | "open" | "none" | "agent_request";

export type OperationStatus = "running" | "completed" | "failed" | "cancelled";

export type Operation = { 
/**
 * Random, never reused (protocol §6.5).
 */
id: string, kind: OperationKind, status: OperationStatus, 
/**
 * The write's `args` (a prompt as normalized `Block[]`), or the load's cwd.
 */
input: unknown, cancelRequested: boolean, 
/**
 * Pending, by interaction ID.
 */
interactions: { [key in string]: Interaction }, 
/**
 * A running prompt's updates, folded, with content blocks normalized.
 */
process?: ProcessUpdate[], result?: unknown, error?: ProtocolError, };

export type Interaction = { "kind": "permission", interactionId: string, request: RequestPermissionRequest, } | { "kind": "elicitation", interactionId: string, request: CreateElicitationRequest, };

export type UrlFlow = { 
/**
 * The accepted elicitation; identifies the flow.
 */
interactionId: string, 
/**
 * Agent-chosen; may be reused after the flow ends.
 */
elicitationId: string, request: CreateElicitationRequest, };

export type Terminal = { 
/**
 * Session: the Agent's ID; global: the operation ID.
 */
terminalId: string, 
/**
 * A bounded tail (`trim`, protocol §5.3).
 */
output: string, truncated: boolean, exitStatus: ExitStatus | null, };

export type ExitStatus = { exitCode: number | null, signal: string | null, };

export type LoginTerminal = { terminalId: string, output: string, truncated: boolean, exitStatus: ExitStatus | null, 
/**
 * The authentication method it runs.
 */
methodId: string, };

export type SessionLive = { op: Operation, 
/**
 * By interaction ID.
 */
urlFlows: { [key in string]: UrlFlow }, sessionId: string, cwd: string, controls: SessionControls, 
/**
 * Agent terminals not yet released.
 */
terminals: { [key in string]: Terminal }, };

export type GlobalLive = { op: Operation, urlFlows: { [key in string]: UrlFlow }, agent: AgentInfo, 
/**
 * Sessions the bridge holds.
 */
sessions: { [key in string]: SessionSummary }, 
/**
 * Terminal logins, by operation ID.
 */
terminals: { [key in string]: LoginTerminal }, };

export type CatalogLive = { op: Operation, urlFlows: { [key in string]: UrlFlow }, 
/**
 * Changes when the Agent's list may have changed.
 */
revision: string, };

export type AgentInfo = { phase: AgentPhase, error: ProtocolError | null, launch: AgentLaunch, initialize: InitializeResponse | null, authenticatedMethodId: string | null, };

export type AgentPhase = "starting" | "initializing" | "ready" | "stopped" | "error";

export type AgentLaunch = { transport: AgentTransport, command: Array<string>, cwd: string, readOnly: boolean, additionalDirectories: Array<string>, mcpServers: Array<McpServerSummary>, };

export type AgentTransport = "stdio" | "http" | "ws";

export type McpServerSummary = { name: string, type: McpServerKind, };

export type McpServerKind = "stdio" | "http" | "sse" | "acp";

export type SessionSummary = { sessionId: string, cwd: string, 
/**
 * `controls.info.title`.
 */
title: string | null, op: OperationSummary, pending: PendingCounts, };

export type OperationSummary = { kind: OperationKind, status: OperationStatus, };

export type PendingCounts = { interactions: number, urlFlows: number, };

export type Event = { scope: string, log: LogChange, live?: LiveChange, } | { scope: string, live: LiveChange, } | { scope: string, end: End, };

export type LogChange = { 
/**
 * Truncate the log to this length, then append.
 */
from: number, 
/**
 * `h_from`, as the client must hold it.
 */
prev: string, entries: Array<LogEntry>, };

export type LiveChange = { replace: unknown, } | { patch: Array<Op>, };

export type End = { reason: EndReason, error?: ProtocolError, };

export type EndReason = "deleted" | "load_failed";

export type OpRequest = { 
/**
 * The `op.id` the write expects the scope to show.
 */
expect: string, } & ({ "kind": "prompt", "args": PromptArgs } | { "kind": "set_mode", "args": SetModeArgs } | { "kind": "set_config", "args": SetConfigArgs } | { "kind": "fork", "args": ForkArgs } | { "kind": "create", "args": CreateArgs } | { "kind": "delete", "args": DeleteArgs } | { "kind": "authenticate", "args": MethodArgs } | { "kind": "logout", "args": Empty } | { "kind": "terminal_login", "args": TerminalLoginArgs } | { "kind": "list", "args": ListArgs } | { "kind": "cancel", "args": Empty } | { "kind": "answer", "args": AnswerArgs });

export type OpArgs = { "kind": "prompt", "args": PromptArgs } | { "kind": "set_mode", "args": SetModeArgs } | { "kind": "set_config", "args": SetConfigArgs } | { "kind": "fork", "args": ForkArgs } | { "kind": "create", "args": CreateArgs } | { "kind": "delete", "args": DeleteArgs } | { "kind": "authenticate", "args": MethodArgs } | { "kind": "logout", "args": Empty } | { "kind": "terminal_login", "args": TerminalLoginArgs } | { "kind": "list", "args": ListArgs } | { "kind": "cancel", "args": Empty } | { "kind": "answer", "args": AnswerArgs };

export type Empty = Record<symbol, never>;

export type PromptArgs = { prompt: Array<PromptBlock>, };

export type SetModeArgs = { modeId: string, };

export type SetConfigArgs = { configId: string, value: ConfigValue, };

export type ConfigValue = string | boolean;

export type ForkArgs = { cwd?: string, };

export type CreateArgs = { cwd: string, };

export type DeleteArgs = { sessionId: string, };

export type MethodArgs = { methodId: string, };

export type TerminalLoginArgs = { methodId: string, cols: number, rows: number, };

export type ListArgs = { cursor?: string, 
/**
 * Refused with `catalog_changed` unless it is `catalog.live.revision`.
 */
revision?: string, };

export type AnswerArgs = { answers: { [key in string]: Answer }, };

export type Answer = { outcome: RequestPermissionOutcome, } | { response: CreateElicitationResponse, };

export type OpAccepted = { op: string, };

export type OpEnded = { op: string, } & ({ result: unknown, } | { error: ProtocolError, });

export type OpOutcome = { result: unknown, } | { error: ProtocolError, };

export type SessionCreated = { sessionId: string, };

export type CatalogPage = { revision: string, sessions: Array<import("@agentclientprotocol/sdk").SessionInfo>, nextCursor: string | null, };

export type PromptResult = { stopReason: PromptResponse["stopReason"], usage?: PromptResponse["usage"], };

export type TerminalInput = { data: string, };

export type TerminalResize = { cols: number, rows: number, };

export type ScopeRead = { live: SessionLive | GlobalLive | CatalogLive, log?: Array<LogEntry>, };

export type ContextMatches = { matches: Array<ContextMatch>, };

export type ContextMatch = { 
/**
 * Absolute.
 */
path: string, name: string, relativePath: string, rootName: string, size: number, };

export type ContextRead = { attachment: ContextAttachment, };

export type ContextAttachment = { name: string, size: number, block: Extract<ContentBlock, { type: "resource" }>, };

export type ErrorBody = { error: ProtocolError, };

export type ProcessUpdate = ProjectedFields<Extract<SessionUpdate, { sessionUpdate: "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" | "compaction_summary_chunk" }>, "content"> | ProjectedFields<Extract<SessionUpdate, { sessionUpdate: "compaction_update" }>, "summary"> | ProjectedToolCall<Extract<SessionUpdate, { sessionUpdate: "tool_call" | "tool_call_update" }>> | Exclude<SessionUpdate, { sessionUpdate: "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" | "compaction_summary_chunk" | "compaction_update" | "tool_call" | "tool_call_update" }>;

export type ProjectedFields<T, K extends PropertyKey> = T extends unknown ? { [P in keyof T]: P extends K ? ProjectedBlocks<T[P]> : T[P] } : never;

export type ProjectedBlocks<V> = V extends ContentBlock ? Block : V extends Array<infer B> ? Array<ProjectedBlocks<B>> : V;

export type ProjectedToolCall<T> = T extends unknown ? { [P in keyof T]: P extends "content" ? ProjectedToolCallContents<T[P]> : T[P] } : never;

export type ProjectedToolCallContents<V> = V extends Array<infer C> ? Array<ProjectedToolCallContent<C>> : V;

export type ProjectedToolCallContent<C> = C extends { type: "content" } ? ProjectedFields<C, "content"> : C;
