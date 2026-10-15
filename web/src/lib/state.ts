import type {
  AuthenticateResponse,
  AvailableCommand,
  CompactionStatus,
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
  ToolCallContent,
} from "@agentclientprotocol/sdk";
import type { AgentLaunch, PromptBlock } from "../../../shared/protocol";
import type {
  ConnectionPhase,
  ElicitationAbortReason,
  SessionRuntimeOperation,
  TerminalSnapshot,
} from "../../../shared/bridge";

/**
 * Tool call content as the thread shows it. A running prompt's process carries
 * content blocks normalized like the log's (protocol §4.4), so a block may be
 * an `AttachmentRef` rather than an ACP `ContentBlock`.
 */
export type ViewToolCallContent =
  | Exclude<ToolCallContent, { type: "content" }>
  | (Omit<Extract<ToolCallContent, { type: "content" }>, "content"> & { content: PromptBlock });

export type ViewToolCall = Omit<ToolCall, "content"> & { content?: ViewToolCallContent[] };

export interface AssistantMessageChunk {
  id: string;
  role: "agent" | "thought";
  blocks: PromptBlock[];
  messageId?: string | null;
  raw: unknown[];
}

export type TimelineItem = (
  | {
      id: string;
      type: "message";
      role: "user" | "protocol-user";
      blocks: PromptBlock[];
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
      call: ViewToolCall;
      raw: unknown[];
    }
  | { id: string; type: "plan"; update: SessionUpdate; raw: unknown[] }
  | {
      id: string;
      type: "compaction";
      compactionId: string;
      status: CompactionStatus;
      blocks: PromptBlock[];
      error?: string;
      raw: unknown[];
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
      retryBlocks?: PromptBlock[];
    }
) & {
  /** The log entry a completed turn's items come from. */
  historyTurnId?: string;
};

export interface PendingPermission {
  permissionId: string;
  request: RequestPermissionRequest;
  responseRequestId?: string;
  /** What the card says while its answer is held or sent, instead of "Sending". */
  responseStatus?: string;
  responseError?: string;
}

export interface PendingElicitation {
  elicitationId: string;
  request: CreateElicitationRequest;
  responseRequestId?: string;
  /** What the card says while its answer is held or sent, instead of "Sending". */
  responseStatus?: string;
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

/** A failed operation or request, as the page reports it. */
export interface BridgeError {
  message: string;
  requestId?: string;
  operation?: string;
  code?: number;
  data?: unknown;
}

export type AgentActivity =
  | { kind: "waiting" }
  | { kind: "thinking"; timelineId: string }
  | { kind: "tool"; toolCallId: string; title: string }
  | { kind: "planning" }
  | { kind: "compacting" }
  | { kind: "responding" };

export interface SessionTransition {
  kind: "new" | "attach" | "fork" | "close";
  requestId: string;
}

export interface PendingSessionDeletion {
  requestId: string;
  sessionId: string;
}

export interface PendingSessionControl {
  requestId: string;
  sessionId: string;
  kind: "mode" | "config";
}

export interface PendingPrompt {
  requestId: string;
  sessionId: string;
  blocks: PromptBlock[];
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

/** Where the shown session's operation stands (`op` of its `live`). */
export type SessionSyncPhase = "ready" | "running" | "reconciling";

/** What the page shows, projected from the replicas and its own UI state. */
export interface AppState {
  phase: ConnectionPhase;
  transport: AgentLaunch["transport"];
  command: string[];
  defaultCwd: string;
  cwd: string;
  additionalDirectories: string[];
  mcpServers: AgentLaunch["mcpServers"];
  readOnly: boolean;
  initialized?: InitializeResponse;
  authStatus?: AgentAuthStatus;
  pendingAuth?: PendingAuthOperation;
  authError?: string;
  lastAuthResponse?: AgentAuthResponse;
  session?: NewSessionResponse;
  attentionSessionIds: string[];
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
  running: boolean;
  sessionSyncPhase?: SessionSyncPhase;
  agentActivity?: AgentActivity;
  title?: string;
  usage?: { used: number; size: number; cost?: { amount: number; currency: string } | null };
  activePlan?: Extract<TimelineItem, { type: "plan" }>;
}


export const initialState: AppState = {
  phase: "starting",
  transport: "stdio",
  command: [],
  defaultCwd: "",
  cwd: "",
  additionalDirectories: [],
  mcpServers: [],
  readOnly: false,
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
  running: false,
};
