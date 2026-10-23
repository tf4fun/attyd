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
} from "@agentclientprotocol/sdk";

export type SessionControls = { modes: SessionModes | null, 
/**
 * `null`: never provided; `[]`: provided empty. Preferred over `modes`
 * whenever non-null.
 */
configOptions: SessionConfigOption[] | null, commands: AvailableCommand[], info: SessionInfo, usage: SessionUsage | null, 
/**
 * The source metadata of the latest source that set each control (D12).
 */
meta: ControlMeta, };

export type SessionModes = { currentModeId: string, availableModes: SessionMode[], };

export type SessionInfo = { title: string | null, updatedAt: string | null, meta: unknown | null, };

export type SessionUsage = { used: number, size: number, cost: Cost | null, };

export type ControlMeta = { modes?: ControlSource, configOptions?: ControlSource, commands?: ControlSource, info?: ControlSource, usage?: ControlSource, };

export type ControlSource = { 
/**
 * The update's own `_meta` (for example `CurrentModeUpdate._meta`).
 */
update?: unknown, 
/**
 * `SessionNotification._meta` of the notification that carried it.
 */
notification?: unknown, };

export type Op = { "op": "set", path: string, value: unknown, } | { "op": "remove", path: string, } | { "op": "append", path: string, } & ({ values: unknown[], } | { text: string, }) | { "op": "trim", path: string, count: number, };

export type AppendTail = { values: unknown[], } | { text: string, };

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

export type ProtocolError = { code: ErrorCode, message: string, 
/**
 * `agent_error`: the ACP error `{ code, message, data }`.
 */
data?: unknown, };

export type ErrorCode = "invalid_request" | "not_found" | "stale" | "not_allowed" | "catalog_changed" | "unsupported" | "unavailable" | "agent_error";

export type OperationKind = "prompt" | "set_mode" | "set_config" | "fork" | "create" | "delete" | "authenticate" | "logout" | "terminal_login" | "list" | "load" | "open" | "none";

export type OperationStatus = "running" | "completed" | "failed" | "cancelled";

export type Operation = { 
/**
 * Random, never reused (protocol §6.5).
 */
id: string, 
/**
 * The scope it was written to: its issuing scope.
 */
scope: string, 
/**
 * The operation it replaced in its issuing scope: the `expect` it was admitted with.
 */
after: string | null, kind: OperationKind, status: OperationStatus, 
/**
 * The write's `args` (a prompt as normalized `Block[]`), or the load's cwd.
 */
input: unknown, cancelRequested: boolean, result?: unknown, error?: ProtocolError, };

export type Interaction = { "kind": "permission", interactionId: string, request: RequestPermissionRequest, } | { "kind": "elicitation", interactionId: string, request: CreateElicitationRequest, };

export type UrlFlow = { 
/**
 * The accepted elicitation; identifies the flow.
 */
interactionId: string, 
/**
 * Agent-chosen; may be reused after the flow ends.
 */
elicitationId: string, request: CreateElicitationRequest, status: UrlFlowState, };

export type UrlFlowState = "waiting" | "completed" | "ended";

export type Terminal = { 
/**
 * Session: the Agent's ID; global: the operation ID.
 */
terminalId: string, 
/**
 * A bounded tail (`trim`, protocol §5.3).
 */
output: string, truncated: boolean, exitStatus: ExitStatus | null, 
/**
 * Its handle is gone and the output frozen.
 */
released: boolean, };

export type ExitStatus = { exitCode: number | null, signal: string | null, };

export type LoginTerminal = { terminalId: string, output: string, truncated: boolean, exitStatus: ExitStatus | null, 
/**
 * The authentication method it runs.
 */
methodId: string, 
/**
 * Always `false`: a login's handle lasts until it is dismissed.
 */
released: boolean, };

export type SessionLive = { op: Operation, 
/**
 * Pending, by interaction ID (protocol §4.4): beside the operation, not part of it.
 */
interactions: { [key in string]: Interaction }, 
/**
 * By interaction ID.
 */
urlFlows: { [key in string]: UrlFlow }, sessionId: string, cwd: string, controls: SessionControls, 
/**
 * Agent terminals, those with a handle and those released whose output an entry embeds.
 */
terminals: { [key in string]: Terminal }, 
/**
 * A close request is outstanding.
 */
closing: boolean, };

export type GlobalLive = { op: Operation, interactions: { [key in string]: Interaction }, urlFlows: { [key in string]: UrlFlow }, agent: AgentInfo, 
/**
 * Sessions the bridge holds.
 */
sessions: { [key in string]: SessionSummary }, 
/**
 * Terminal logins, by operation ID.
 */
terminals: { [key in string]: LoginTerminal }, };

export type CatalogLive = { op: Operation, interactions: { [key in string]: Interaction }, urlFlows: { [key in string]: UrlFlow }, 
/**
 * Changes when the Agent's list may have changed.
 */
revision: string, 
/**
 * The pages listed since `revision` last changed, in order.
 */
pages: Array<{ sessions: Array<import("@agentclientprotocol/sdk").SessionInfo>; nextCursor: string | null; _meta?: unknown }>, };

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
title: string | null, op: OperationSummary, pending: PendingCounts, 
/**
 * The `create` or `fork` that installed the session, while it is held.
 */
origin?: string, };

export type OperationSummary = { kind: OperationKind, status: OperationStatus, };

export type PendingCounts = { interactions: number, urlFlows: number, };

export type End = { reason: EndReason, error?: ProtocolError, };

export type EndReason = "deleted" | "closed" | "load_failed";

export type OpRequest = { 
/**
 * The `op.id` the write expects the scope to show.
 */
error: test failed, to rerun pass `--bin attyd`
expect: string, } & ({ "kind": "prompt", "args": PromptArgs } | { "kind": "set_mode", "args": SetModeArgs } | { "kind": "set_config", "args": SetConfigArgs } | { "kind": "fork", "args": ForkArgs } | { "kind": "create", "args": CreateArgs } | { "kind": "delete", "args": DeleteArgs } | { "kind": "authenticate", "args": MethodArgs } | { "kind": "logout", "args": Empty } | { "kind": "terminal_login", "args": TerminalLoginArgs } | { "kind": "list", "args": ListArgs } | { "kind": "cancel", "args": Empty });

export type OpArgs = { "kind": "prompt", "args": PromptArgs } | { "kind": "set_mode", "args": SetModeArgs } | { "kind": "set_config", "args": SetConfigArgs } | { "kind": "fork", "args": ForkArgs } | { "kind": "create", "args": CreateArgs } | { "kind": "delete", "args": DeleteArgs } | { "kind": "authenticate", "args": MethodArgs } | { "kind": "logout", "args": Empty } | { "kind": "terminal_login", "args": TerminalLoginArgs } | { "kind": "list", "args": ListArgs } | { "kind": "cancel", "args": Empty };

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

export type Answer = { outcome: RequestPermissionOutcome, } | { response: CreateElicitationResponse, };

export type OpAccepted = { op: string, };

export type SessionCreated = { sessionId: string, };

export type CatalogPage = { revision: string, sessions: Array<import("@agentclientprotocol/sdk").SessionInfo>, nextCursor: string | null, 
/**
 * The Agent's `_meta` of this page.
 */
_meta?: unknown, };

export type PromptResult = { stopReason: PromptResponse["stopReason"], usage?: PromptResponse["usage"], };

export type TerminalInput = { data: string, };

export type TerminalResize = { cols: number, rows: number, };

export type ContextMatches = { matches: Array<ContextMatch>, };

export type ContextMatch = { 
/**
 * Absolute.
 */
path: string, name: string, relativePath: string, rootName: string, size: number, };

export type ContextRead = { attachment: ContextAttachment, };

export type ContextAttachment = { name: string, size: number, block: Extract<ContentBlock, { type: "resource" }>, };

export type ErrorBody = { error: ProtocolError, };

export type HostError = { error: string, };

export type Health = { ok: boolean, protocol: string, backend: string, };
