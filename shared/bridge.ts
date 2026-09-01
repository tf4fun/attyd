import type {
  AuthenticateResponse,
  ContentBlock,
  CompleteElicitationNotification,
  CreateElicitationRequest,
  CreateElicitationResponse,
  DidChangeDocumentNotification,
  DidCloseDocumentNotification,
  DidFocusDocumentNotification,
  DidOpenDocumentNotification,
  DidSaveDocumentNotification,
  ForkSessionResponse,
  InitializeResponse,
  ListSessionsResponse,
  LoadSessionResponse,
  LogoutResponse,
  NewSessionResponse,
  NesRejectReason,
  NesSuggestion,
  Position,
  PromptResponse,
  RequestPermissionRequest,
  RequestPermissionResponse,
  Range,
  ResumeSessionResponse,
  SessionNotification,
  SetSessionConfigOptionResponse,
  StartNesResponse,
  SuggestNesResponse,
  TerminalExitStatus,
} from "@agentclientprotocol/sdk";
import { validateContentBlockSemantics } from "./content-validation.js";

export const MAX_BRIDGE_MESSAGE_BYTES = 5 * 1024 * 1024;
export const MAX_BRIDGE_ERROR_DATA_BYTES = 256 * 1024;
const MAX_BRIDGE_TYPE_LENGTH = 128;
const MAX_BRIDGE_IDENTIFIER_LENGTH = 1_024;
const MAX_BRIDGE_CURSOR_LENGTH = 4_096;
const MAX_BRIDGE_PATH_LENGTH = 16_384;
const MAX_BRIDGE_LANGUAGE_ID_LENGTH = 256;
const MAX_NES_SUGGESTIONS = 100;
const MAX_NES_EDITS = 10_000;

export interface NesDocumentState {
  sessionId: string;
  path: string;
  uri: string;
  languageId: string;
  version: number;
  text: string;
}

export interface TerminalSnapshot {
  sessionId: string;
  terminalId: string;
  output: string;
  truncated: boolean;
  exitStatus?: TerminalExitStatus | null;
  released: boolean;
}

export interface WorkspaceContextMatch {
  path: string;
  name: string;
  relativePath: string;
  rootName: string;
  size: number;
}

export interface WorkspaceContextAttachment {
  name: string;
  size: number;
  block: Extract<ContentBlock, { type: "resource" }>;
}

export type ConnectionPhase =
  | "starting"
  | "initializing"
  | "ready"
  | "stopped"
  | "error";

export type ElicitationAbortReason =
  | "session_cancelled"
  | "session_closed"
  | "nes_closed";

export type ClientCommand =
  | { type: "bridge/ping"; nonce: string }
  | { type: "auth/authenticate"; requestId: string; methodId: string }
  | {
      type: "auth/terminal_start";
      requestId: string;
      methodId: string;
      cols: number;
      rows: number;
    }
  | { type: "auth/terminal_input"; requestId: string; data: string }
  | {
      type: "auth/terminal_resize";
      requestId: string;
      cols: number;
      rows: number;
    }
  | { type: "auth/terminal_cancel"; requestId: string }
  | { type: "auth/logout"; requestId: string }
  | { type: "context/search"; requestId: string; query: string }
  | {
      type: "context/read";
      requestId: string;
      sessionId: string;
      path: string;
    }
  | { type: "session/new"; requestId: string; cwd?: string }
  | { type: "session/list"; requestId: string; cursor?: string }
  | {
      type: "session/load";
      requestId: string;
      sessionId: string;
    }
  | {
      type: "session/resume";
      requestId: string;
      sessionId: string;
    }
  | {
      type: "session/fork";
      requestId: string;
      sessionId: string;
    }
  | {
      type: "session/close";
      requestId: string;
      sessionId: string;
    }
  | {
      type: "session/delete";
      requestId: string;
      sessionId: string;
    }
  | {
      type: "session/prompt";
      requestId: string;
      sessionId: string;
      prompt: ContentBlock[];
    }
  | { type: "session/cancel"; sessionId: string }
  | {
      type: "session/set_mode";
      requestId: string;
      sessionId: string;
      modeId: string;
    }
  | {
      type: "session/set_config_option";
      requestId: string;
      sessionId: string;
      configId: string;
      value: string | boolean;
    }
  | { type: "nes/start"; requestId: string }
  | {
      type: "nes/suggest";
      requestId: string;
      sessionId: string;
      uri: string;
      position: Position;
      selection?: Range;
      triggerKind: "automatic" | "diagnostic" | "manual";
    }
  | {
      type: "nes/accept";
      requestId: string;
      sessionId: string;
      suggestionId: string;
      text?: string;
    }
  | {
      type: "nes/reject";
      requestId: string;
      sessionId: string;
      suggestionId: string;
      reason?: NesRejectReason;
    }
  | { type: "nes/close"; requestId: string; sessionId: string }
  | {
      type: "document/open";
      requestId: string;
      sessionId: string;
      path: string;
      languageId: string;
    }
  | {
      type: "document/change";
      requestId: string;
      sessionId: string;
      uri: string;
      text: string;
    }
  | {
      type: "document/save";
      requestId: string;
      sessionId: string;
      uri: string;
    }
  | {
      type: "document/focus";
      sessionId: string;
      uri: string;
      position: Position;
      visibleRange: Range;
    }
  | {
      type: "document/close";
      requestId: string;
      sessionId: string;
      uri: string;
    }
  | {
      type: "permission/respond";
      requestId: string;
      permissionId: string;
      outcome: RequestPermissionResponse["outcome"];
    }
  | {
      type: "elicitation/respond";
      requestId: string;
      elicitationId: string;
      response: CreateElicitationResponse;
    };

export type ServerEvent =
  | {
      type: "bridge/hello";
      transport: AgentTransport;
      command: string[];
      cwd: string;
      readOnly: boolean;
      additionalDirectories: string[];
      mcpServers: Array<{ name: string; type: "stdio" | "http" | "sse" | "acp" }>;
    }
  | { type: "bridge/phase"; phase: ConnectionPhase }
  | { type: "bridge/pong"; nonce: string }
  | { type: "bridge/stderr"; chunk: string }
  | {
      type: "bridge/context_search_result";
      requestId: string;
      query: string;
      matches: WorkspaceContextMatch[];
    }
  | {
      type: "bridge/context_attached";
      requestId: string;
      sessionId: string;
      attachment: WorkspaceContextAttachment;
    }
  | {
      type: "bridge/error";
      message: string;
      requestId?: string;
      operation?: ClientCommand["type"];
      code?: number;
      data?: unknown;
      dataTruncated?: boolean;
      dataBytes?: number;
    }
  | { type: "acp/initialized"; response: InitializeResponse }
  | {
      type: "acp/authenticated";
      requestId: string;
      methodId: string;
      response: AuthenticateResponse;
    }
  | {
      type: "bridge/auth_terminal_started";
      requestId: string;
      methodId: string;
    }
  | {
      type: "bridge/auth_terminal_output";
      requestId: string;
      data: string;
    }
  | {
      type: "bridge/auth_terminal_exited";
      requestId: string;
      methodId: string;
      status: "succeeded" | "failed" | "cancelled";
      exitCode: number | null;
      signal?: number;
      message?: string;
    }
  | { type: "acp/logged_out"; requestId: string; response: LogoutResponse }
  | {
      type: "acp/session_created";
      requestId: string;
      cwd?: string;
      response: NewSessionResponse;
      earlyUpdates?: SessionNotification[];
    }
  | {
      type: "acp/sessions_listed";
      requestId: string;
      cursor?: string;
      response: ListSessionsResponse;
    }
  | {
      type: "acp/session_attached";
      requestId: string;
      method: "load" | "resume";
      sessionId: string;
      cwd?: string;
      response: LoadSessionResponse | ResumeSessionResponse;
    }
  | {
      type: "acp/session_forked";
      requestId: string;
      sourceSessionId: string;
      cwd?: string;
      response: ForkSessionResponse;
      earlyUpdates?: SessionNotification[];
    }
  | {
      type: "acp/session_closed";
      requestId: string;
      sessionId: string;
    }
  | {
      type: "acp/session_deleted";
      requestId: string;
      sessionId: string;
    }
  | { type: "acp/session_update"; notification: SessionNotification }
  | { type: "acp/terminal_state"; terminal: TerminalSnapshot }
  | {
      type: "acp/prompt_complete";
      requestId: string;
      sessionId: string;
      response: PromptResponse;
    }
  | {
      type: "acp/permission_request";
      permissionId: string;
      request: RequestPermissionRequest;
    }
  | {
      type: "acp/permission_resolved";
      permissionId: string;
      requestId?: string;
    }
  | {
      type: "acp/elicitation_request";
      elicitationId: string;
      request: CreateElicitationRequest;
    }
  | {
      type: "acp/elicitation_complete";
      notification: CompleteElicitationNotification;
    }
  | {
      type: "acp/elicitation_resolved";
      elicitationId: string;
      response: CreateElicitationResponse;
      requestId?: string;
    }
  | {
      type: "acp/elicitation_aborted";
      elicitationId: string;
      sessionId: string;
      reason: ElicitationAbortReason;
    }
  | {
      type: "acp/mode_changed";
      requestId: string;
      sessionId: string;
      modeId: string;
    }
  | {
      type: "acp/config_changed";
      requestId: string;
      sessionId: string;
      configId: string;
      value: string | boolean;
      response: SetSessionConfigOptionResponse;
    }
  | {
      type: "acp/mcp_connection";
      action: "connected" | "disconnected";
      serverId: string;
      connectionId: string;
      name: string;
    }
  | {
      type: "acp/mcp_message";
      direction: "agent-to-server" | "server-to-agent";
      connectionId: string;
      method: string;
      kind: "request" | "notification" | "response";
      params?: Record<string, unknown> | null;
      result?: unknown;
      error?: { code: number; message: string; data?: unknown };
    }
  | { type: "acp/nes_started"; requestId: string; response: StartNesResponse }
  | {
      type: "acp/nes_suggestions";
      requestId: string;
      sessionId: string;
      uri: string;
      response: SuggestNesResponse;
    }
  | {
      type: "acp/nes_suggestion_resolved";
      requestId: string;
      sessionId: string;
      suggestionId: string;
      outcome: "accepted" | "rejected";
      reason?: NesRejectReason;
    }
  | { type: "acp/nes_closed"; requestId: string; sessionId: string }
  | {
      type: "acp/document_opened";
      requestId: string;
      document: NesDocumentState;
      notification?: DidOpenDocumentNotification;
    }
  | {
      type: "acp/document_changed";
      requestId: string;
      document: NesDocumentState;
      notification?: DidChangeDocumentNotification;
    }
  | {
      type: "acp/document_saved";
      requestId: string;
      sessionId: string;
      uri: string;
      notification?: DidSaveDocumentNotification;
    }
  | {
      type: "acp/document_focused";
      notification: DidFocusDocumentNotification;
    }
  | {
      type: "acp/document_closed";
      requestId: string;
      sessionId: string;
      uri: string;
      notification?: DidCloseDocumentNotification;
    };

const SERVER_EVENT_TYPES = {
  "bridge/hello": true,
  "bridge/phase": true,
  "bridge/pong": true,
  "bridge/stderr": true,
  "bridge/context_search_result": true,
  "bridge/context_attached": true,
  "bridge/auth_terminal_started": true,
  "bridge/auth_terminal_output": true,
  "bridge/auth_terminal_exited": true,
  "bridge/error": true,
  "acp/initialized": true,
  "acp/authenticated": true,
  "acp/logged_out": true,
  "acp/session_created": true,
  "acp/sessions_listed": true,
  "acp/session_attached": true,
  "acp/session_forked": true,
  "acp/session_closed": true,
  "acp/session_deleted": true,
  "acp/session_update": true,
  "acp/terminal_state": true,
  "acp/prompt_complete": true,
  "acp/permission_request": true,
  "acp/permission_resolved": true,
  "acp/elicitation_request": true,
  "acp/elicitation_complete": true,
  "acp/elicitation_resolved": true,
  "acp/elicitation_aborted": true,
  "acp/mode_changed": true,
  "acp/config_changed": true,
  "acp/mcp_connection": true,
  "acp/mcp_message": true,
  "acp/nes_started": true,
  "acp/nes_suggestions": true,
  "acp/nes_suggestion_resolved": true,
  "acp/nes_closed": true,
  "acp/document_opened": true,
  "acp/document_changed": true,
  "acp/document_saved": true,
  "acp/document_focused": true,
  "acp/document_closed": true,
} as const satisfies Record<ServerEvent["type"], true>;

export function parseServerEvent(raw: string): ServerEvent {
  const value: unknown = JSON.parse(raw);
  if (!isRecord(value) || typeof value.type !== "string") {
    throw new Error("Server WebSocket event must be an object with a type");
  }
  if (value.type.length > MAX_BRIDGE_TYPE_LENGTH) {
    throw new Error("Server WebSocket event type is too long");
  }
  if (!Object.hasOwn(SERVER_EVENT_TYPES, value.type)) {
    throw new Error(`Unknown server WebSocket event: ${value.type}`);
  }
  validateServerEventEnvelope(value, value.type as ServerEvent["type"]);
  return value as ServerEvent;
}

function validateServerEventEnvelope(
  value: Record<string, unknown>,
  type: ServerEvent["type"],
): void {
  switch (type) {
    case "bridge/hello":
      requireEnum(value.transport, ["stdio", "http", "ws"], "bridge/hello transport");
      requireStringArray(value.command, "bridge/hello command");
      if (typeof value.cwd !== "string") throw new Error("bridge/hello requires cwd");
      if (typeof value.readOnly !== "boolean") throw new Error("bridge/hello requires readOnly");
      requireStringArray(value.additionalDirectories, "bridge/hello additionalDirectories");
      requireArray(value.mcpServers, "bridge/hello mcpServers");
      return;
    case "bridge/phase":
      requireEnum(value.phase, ["starting", "initializing", "ready", "stopped", "error"], "bridge/phase phase");
      return;
    case "bridge/pong":
      requireBridgeIdentifier(value, "nonce");
      return;
    case "bridge/stderr":
      requireString(value, "chunk");
      return;
    case "bridge/context_search_result": {
      requireBridgeIdentifier(value, "requestId");
      requireBoundedString(value, "query", 256);
      requireArray(value.matches, "bridge/context_search_result matches");
      if (value.matches.length > 24) {
        throw new Error("bridge/context_search_result exceeds 24 matches");
      }
      for (const [index, candidate] of value.matches.entries()) {
        const match = requireRecordValue(
          candidate,
          `bridge/context_search_result match ${index}`,
        );
        requireBoundedString(match, "path", MAX_BRIDGE_PATH_LENGTH);
        requireBoundedString(match, "name", 1_024);
        requireBoundedString(match, "relativePath", MAX_BRIDGE_PATH_LENGTH);
        requireBoundedString(match, "rootName", 1_024);
        if (!Number.isSafeInteger(match.size) || Number(match.size) < 0) {
          throw new Error("bridge/context_search_result match size must be non-negative");
        }
      }
      return;
    }
    case "bridge/context_attached": {
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      const attachment = requireRecordValue(
        value.attachment,
        "bridge/context_attached attachment",
      );
      requireBoundedString(attachment, "name", MAX_BRIDGE_PATH_LENGTH);
      if (!Number.isSafeInteger(attachment.size) || Number(attachment.size) < 0) {
        throw new Error("bridge/context_attached attachment size must be non-negative");
      }
      const block = requireRecordValue(
        attachment.block,
        "bridge/context_attached content block",
      );
      validateContentBlockSemantics(block as unknown as ContentBlock);
      if (block.type !== "resource") {
        throw new Error("bridge/context_attached requires an embedded resource");
      }
      return;
    }
    case "bridge/auth_terminal_started":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "methodId");
      return;
    case "bridge/auth_terminal_output":
      requireBridgeIdentifier(value, "requestId");
      requireStringValue(value.data, "bridge/auth_terminal_output data");
      if (value.data.length > 65_536) {
        throw new Error("bridge/auth_terminal_output data exceeds 65536 characters");
      }
      return;
    case "bridge/auth_terminal_exited":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "methodId");
      requireEnum(
        value.status,
        ["succeeded", "failed", "cancelled"],
        "bridge/auth_terminal_exited status",
      );
      if (
        value.exitCode !== null &&
        (
          !Number.isSafeInteger(value.exitCode) ||
          Number(value.exitCode) < 0 ||
          Number(value.exitCode) > 0xffff_ffff
        )
      ) {
        throw new Error("bridge/auth_terminal_exited exitCode must be null or a uint32 integer");
      }
      if (value.signal != null && !Number.isSafeInteger(value.signal)) {
        throw new Error("bridge/auth_terminal_exited signal must be an integer");
      }
      requireOptionalString(value.message, "bridge/auth_terminal_exited message");
      return;
    case "bridge/error":
      requireString(value, "message");
      requireOptionalString(value.requestId, "bridge/error requestId");
      requireOptionalString(value.operation, "bridge/error operation");
      if (value.code != null && !Number.isSafeInteger(value.code)) {
        throw new Error("bridge/error code must be a safe integer when provided");
      }
      if (value.dataTruncated != null && typeof value.dataTruncated !== "boolean") {
        throw new Error("bridge/error dataTruncated must be a boolean when provided");
      }
      if (
        value.dataBytes != null &&
        (!Number.isSafeInteger(value.dataBytes) || Number(value.dataBytes) < 0)
      ) {
        throw new Error("bridge/error dataBytes must be a non-negative safe integer when provided");
      }
      if (value.data !== undefined) {
        const serialized = JSON.stringify(value.data);
        if (serialized === undefined) throw new Error("bridge/error data must be JSON serializable");
        if (new TextEncoder().encode(serialized).byteLength > MAX_BRIDGE_ERROR_DATA_BYTES) {
          throw new Error("bridge/error data exceeds the relay limit");
        }
      }
      return;
    case "acp/initialized":
      requireRecordValue(value.response, "acp/initialized response");
      return;
    case "acp/authenticated":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "methodId");
      requireRecordValue(value.response, "acp/authenticated response");
      return;
    case "acp/logged_out":
      requireBridgeIdentifier(value, "requestId");
      requireRecordValue(value.response, "acp/logged_out response");
      return;
    case "acp/session_created": {
      requireString(value, "requestId");
      validateOptionalWorkspacePath(value.cwd, "acp/session_created cwd");
      const response = requireRecordValue(value.response, "acp/session_created response");
      requireStringValue(response.sessionId, "acp/session_created response sessionId");
      validateEarlySessionUpdates(value.earlyUpdates, response.sessionId, type);
      return;
    }
    case "acp/sessions_listed": {
      requireString(value, "requestId");
      requireOptionalString(value.cursor, "acp/sessions_listed cursor");
      const response = requireRecordValue(value.response, "acp/sessions_listed response");
      requireArray(response.sessions, "acp/sessions_listed sessions");
      return;
    }
    case "acp/session_attached":
      requireString(value, "requestId");
      requireString(value, "sessionId");
      validateOptionalWorkspacePath(value.cwd, "acp/session_attached cwd");
      requireEnum(value.method, ["load", "resume"], "acp/session_attached method");
      requireRecordValue(value.response, "acp/session_attached response");
      return;
    case "acp/session_forked": {
      requireString(value, "requestId");
      requireString(value, "sourceSessionId");
      validateOptionalWorkspacePath(value.cwd, "acp/session_forked cwd");
      const response = requireRecordValue(value.response, "acp/session_forked response");
      requireStringValue(response.sessionId, "acp/session_forked response sessionId");
      validateEarlySessionUpdates(value.earlyUpdates, response.sessionId, type);
      return;
    }
    case "acp/session_closed":
    case "acp/session_deleted":
    case "acp/nes_closed":
      requireString(value, "requestId");
      requireString(value, "sessionId");
      return;
    case "acp/session_update": {
      const notification = requireRecordValue(value.notification, "acp/session_update notification");
      requireString(notification, "sessionId");
      const update = requireRecordValue(notification.update, "acp/session_update update");
      requireString(update, "sessionUpdate");
      return;
    }
    case "acp/terminal_state": {
      const terminal = requireRecordValue(value.terminal, "acp/terminal_state terminal");
      requireString(terminal, "sessionId");
      requireString(terminal, "terminalId");
      requireStringValue(terminal.output, "acp/terminal_state output");
      if (terminal.output.length > 1_000_000) {
        throw new Error("acp/terminal_state output exceeds 1000000 characters");
      }
      if (typeof terminal.truncated !== "boolean") {
        throw new Error("acp/terminal_state requires truncated");
      }
      if (typeof terminal.released !== "boolean") {
        throw new Error("acp/terminal_state requires released");
      }
      if (terminal.exitStatus != null) {
        const status = requireRecordValue(
          terminal.exitStatus,
          "acp/terminal_state exitStatus",
        );
        if (
          status.exitCode != null &&
          (
            !Number.isSafeInteger(status.exitCode) ||
            Number(status.exitCode) < 0 ||
            Number(status.exitCode) > 0xffff_ffff
          )
        ) {
          throw new Error("acp/terminal_state exitCode must be a uint32 integer or null");
        }
        requireOptionalString(status.signal, "acp/terminal_state signal");
      }
      return;
    }
    case "acp/prompt_complete": {
      requireString(value, "requestId");
      requireString(value, "sessionId");
      const response = requireRecordValue(value.response, "acp/prompt_complete response");
      requireEnum(
        response.stopReason,
        ["end_turn", "max_tokens", "max_turn_requests", "refusal", "cancelled"],
        "acp/prompt_complete stopReason",
      );
      if (response.usage != null) validatePromptUsageEnvelope(response.usage);
      return;
    }
    case "acp/permission_request":
      requireString(value, "permissionId");
      requireRecordValue(value.request, "acp/permission_request request");
      return;
    case "acp/permission_resolved":
      requireString(value, "permissionId");
      requireOptionalString(value.requestId, "acp/permission_resolved requestId");
      return;
    case "acp/elicitation_request":
      requireString(value, "elicitationId");
      requireRecordValue(value.request, "acp/elicitation_request request");
      return;
    case "acp/elicitation_complete": {
      const notification = requireRecordValue(value.notification, "acp/elicitation_complete notification");
      requireString(notification, "elicitationId");
      return;
    }
    case "acp/elicitation_resolved":
      requireString(value, "elicitationId");
      requireRecordValue(value.response, "acp/elicitation_resolved response");
      requireOptionalString(value.requestId, "acp/elicitation_resolved requestId");
      return;
    case "acp/elicitation_aborted":
      requireString(value, "elicitationId");
      requireString(value, "sessionId");
      requireEnum(
        value.reason,
        ["session_cancelled", "session_closed", "nes_closed"],
        "acp/elicitation_aborted reason",
      );
      return;
    case "acp/mode_changed":
      requireString(value, "requestId");
      requireString(value, "sessionId");
      requireString(value, "modeId");
      return;
    case "acp/config_changed":
      requireString(value, "requestId");
      requireString(value, "sessionId");
      requireString(value, "configId");
      if (typeof value.value !== "string" && typeof value.value !== "boolean") {
        throw new Error("acp/config_changed requires a string or boolean value");
      }
      requireRecordValue(value.response, "acp/config_changed response");
      return;
    case "acp/mcp_connection":
      requireEnum(value.action, ["connected", "disconnected"], "acp/mcp_connection action");
      requireString(value, "serverId");
      requireString(value, "connectionId");
      requireString(value, "name");
      return;
    case "acp/mcp_message":
      requireEnum(value.direction, ["agent-to-server", "server-to-agent"], "acp/mcp_message direction");
      requireString(value, "connectionId");
      requireString(value, "method");
      requireEnum(value.kind, ["request", "notification", "response"], "acp/mcp_message kind");
      return;
    case "acp/nes_started": {
      requireString(value, "requestId");
      const response = requireRecordValue(value.response, "acp/nes_started response");
      requireBoundedStringValue(
        response.sessionId,
        "acp/nes_started response sessionId",
        MAX_BRIDGE_IDENTIFIER_LENGTH,
      );
      return;
    }
    case "acp/nes_suggestions": {
      requireString(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      requireBoundedString(value, "uri", MAX_BRIDGE_PATH_LENGTH);
      const response = requireRecordValue(value.response, "acp/nes_suggestions response");
      requireArray(response.suggestions, "acp/nes_suggestions suggestions");
      if (response.suggestions.length > MAX_NES_SUGGESTIONS) {
        throw new Error(
          `acp/nes_suggestions exceeds ${MAX_NES_SUGGESTIONS} suggestions`,
        );
      }
      response.suggestions.forEach((suggestion, index) => {
        validateNesSuggestionEnvelope(
          suggestion,
          `acp/nes_suggestions suggestion ${index}`,
        );
      });
      return;
    }
    case "acp/nes_suggestion_resolved":
      requireString(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      requireBridgeIdentifier(value, "suggestionId");
      requireEnum(value.outcome, ["accepted", "rejected"], "acp/nes_suggestion_resolved outcome");
      return;
    case "acp/document_opened":
    case "acp/document_changed":
      requireString(value, "requestId");
      validateDocumentState(value.document, type);
      return;
    case "acp/document_saved":
    case "acp/document_closed":
      requireString(value, "requestId");
      requireString(value, "sessionId");
      requireString(value, "uri");
      return;
    case "acp/document_focused": {
      const notification = requireRecordValue(value.notification, "acp/document_focused notification");
      requireString(notification, "sessionId");
      requireString(notification, "uri");
      return;
    }
  }

  const unhandled: never = type;
  throw new Error(`Unhandled server event: ${String(unhandled)}`);
}

export type AgentTransport = "stdio" | "http" | "ws";

export function isAbsoluteWorkspacePath(value: string): boolean {
  return value.startsWith("/") || /^[A-Za-z]:[\\/]/.test(value) || value.startsWith("\\\\");
}

function validateOptionalWorkspacePath(value: unknown, label: string): void {
  if (value == null) return;
  requireBoundedStringValue(value, label, MAX_BRIDGE_PATH_LENGTH);
  if (value.includes("\0") || !isAbsoluteWorkspacePath(value)) {
    throw new Error(`${label} must be an absolute path`);
  }
}

function validateEarlySessionUpdates(
  value: unknown,
  sessionId: string,
  label: "acp/session_created" | "acp/session_forked",
): void {
  if (value == null) return;
  requireArray(value, `${label} earlyUpdates`);
  if (value.length > 10_000) {
    throw new Error(`${label} earlyUpdates exceeds 10000 notifications`);
  }
  for (const item of value) {
    const notification = requireRecordValue(item, `${label} early update`);
    if (notification.sessionId !== sessionId) {
      throw new Error(`${label} early update belongs to a different session`);
    }
    const update = requireRecordValue(notification.update, `${label} early update payload`);
    requireString(update, "sessionUpdate");
  }
}

function validateDocumentState(value: unknown, label: string): void {
  const document = requireRecordValue(value, `${label} document`);
  for (const key of ["sessionId", "path", "uri", "languageId"] as const) {
    requireString(document, key);
  }
  requireStringValue(document.text, `${label} document text`);
  if (!Number.isSafeInteger(document.version) || Number(document.version) < 0) {
    throw new Error(`${label} document requires a non-negative version`);
  }
}

function validateNesSuggestionEnvelope(value: unknown, label: string): void {
  const suggestion = requireRecordValue(value, label);
  requireBoundedStringValue(
    suggestion.id,
    `${label} id`,
    MAX_BRIDGE_IDENTIFIER_LENGTH,
  );
  requireBoundedStringValue(
    suggestion.uri,
    `${label} uri`,
    MAX_BRIDGE_PATH_LENGTH,
  );
  requireEnum(
    suggestion.kind,
    ["edit", "jump", "rename", "searchAndReplace"],
    `${label} kind`,
  );
  switch (suggestion.kind) {
    case "edit":
      requireArray(suggestion.edits, `${label} edits`);
      if (suggestion.edits.length > MAX_NES_EDITS) {
        throw new Error(`${label} exceeds ${MAX_NES_EDITS} edits`);
      }
      suggestion.edits.forEach((value, index) => {
        const edit = requireRecordValue(value, `${label} edit ${index}`);
        requireRange(edit.range, `${label} edit ${index} range`);
        requireStringValue(edit.newText, `${label} edit ${index} newText`);
      });
      if (suggestion.cursorPosition != null) {
        requirePosition(suggestion.cursorPosition, `${label} cursorPosition`);
      }
      return;
    case "jump":
      requirePosition(suggestion.position, `${label} position`);
      return;
    case "rename":
      requirePosition(suggestion.position, `${label} position`);
      requireStringValue(suggestion.newName, `${label} newName`);
      return;
    case "searchAndReplace":
      requireStringValue(suggestion.search, `${label} search`);
      requireStringValue(suggestion.replace, `${label} replace`);
      if (suggestion.isRegex != null && typeof suggestion.isRegex !== "boolean") {
        throw new Error(`${label} isRegex must be a boolean when provided`);
      }
      return;
  }
}

function validatePromptUsageEnvelope(value: unknown): void {
  const usage = requireRecordValue(value, "acp/prompt_complete usage");
  const totalTokens = requireTokenCount(usage.totalTokens, "totalTokens");
  const inputTokens = requireTokenCount(usage.inputTokens, "inputTokens");
  const outputTokens = requireTokenCount(usage.outputTokens, "outputTokens");
  for (const name of ["thoughtTokens", "cachedReadTokens", "cachedWriteTokens"] as const) {
    if (usage[name] == null) continue;
    const count = requireTokenCount(usage[name], name);
    if (count > totalTokens) {
      throw new Error(`acp/prompt_complete usage ${name} exceeds totalTokens`);
    }
  }
  if (inputTokens + outputTokens > totalTokens) {
    throw new Error(
      "acp/prompt_complete usage inputTokens plus outputTokens exceeds totalTokens",
    );
  }
}

function requireTokenCount(value: unknown, name: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0) {
    throw new Error(
      `acp/prompt_complete usage ${name} must be a non-negative safe integer`,
    );
  }
  return Number(value);
}

function requireRecordValue(value: unknown, label: string): Record<string, unknown> {
  if (!isRecord(value)) throw new Error(`${label} must be an object`);
  return value;
}

function requireArray(value: unknown, label: string): asserts value is unknown[] {
  if (!Array.isArray(value)) throw new Error(`${label} must be an array`);
}

function requireStringArray(value: unknown, label: string): asserts value is string[] {
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string")) {
    throw new Error(`${label} must be a string array`);
  }
}

function requireEnum(
  value: unknown,
  allowed: readonly string[],
  label: string,
): void {
  if (typeof value !== "string" || !allowed.includes(value)) {
    throw new Error(`${label} is invalid`);
  }
}

export function parseClientCommand(raw: string): ClientCommand {
  const value: unknown = JSON.parse(raw);
  if (!isRecord(value) || typeof value.type !== "string") {
    throw new Error("WebSocket message must be an object with a type");
  }
  if (value.type.length > MAX_BRIDGE_TYPE_LENGTH) {
    throw new Error("WebSocket command type is too long");
  }
  switch (value.type) {
    case "bridge/ping":
      requireBridgeIdentifier(value, "nonce");
      break;
    case "auth/authenticate":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "methodId");
      break;
    case "auth/terminal_start":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "methodId");
      requireTerminalSize(value.cols, value.rows, "auth/terminal_start");
      break;
    case "auth/terminal_input":
      requireBridgeIdentifier(value, "requestId");
      requireStringValue(value.data, "auth/terminal_input data");
      if (value.data.length > 65_536) {
        throw new Error("auth/terminal_input data exceeds 65536 characters");
      }
      break;
    case "auth/terminal_resize":
      requireBridgeIdentifier(value, "requestId");
      requireTerminalSize(value.cols, value.rows, "auth/terminal_resize");
      break;
    case "auth/terminal_cancel":
      requireBridgeIdentifier(value, "requestId");
      break;
    case "auth/logout":
      requireBridgeIdentifier(value, "requestId");
      break;
    case "context/search":
      requireBridgeIdentifier(value, "requestId");
      requireBoundedString(value, "query", 256);
      break;
    case "context/read":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      requireBoundedString(value, "path", MAX_BRIDGE_PATH_LENGTH);
      break;
    case "session/new":
      requireBridgeIdentifier(value, "requestId");
      validateOptionalWorkspacePath(value.cwd, "session/new cwd");
      break;
    case "session/list":
      requireBridgeIdentifier(value, "requestId");
      if (value.cursor != null && typeof value.cursor !== "string") {
        throw new Error("session/list has an invalid cursor");
      }
      if (typeof value.cursor === "string" && value.cursor.length > MAX_BRIDGE_CURSOR_LENGTH) {
        throw new Error(`session/list cursor exceeds ${MAX_BRIDGE_CURSOR_LENGTH} characters`);
      }
      break;
    case "session/load":
    case "session/resume":
    case "session/fork":
    case "session/close":
    case "session/delete":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      break;
    case "session/prompt":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      if (!Array.isArray(value.prompt)) throw new Error("session/prompt requires a prompt array");
      validatePrompt(value.prompt);
      break;
    case "session/cancel":
      requireBridgeIdentifier(value, "sessionId");
      break;
    case "session/set_mode":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      requireBridgeIdentifier(value, "modeId");
      break;
    case "session/set_config_option":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      requireBridgeIdentifier(value, "configId");
      if (!["string", "boolean"].includes(typeof value.value)) {
        throw new Error("session/set_config_option has an invalid value");
      }
      if (typeof value.value === "string" && value.value.length > MAX_BRIDGE_IDENTIFIER_LENGTH) {
        throw new Error(`session/set_config_option value exceeds ${MAX_BRIDGE_IDENTIFIER_LENGTH} characters`);
      }
      break;
    case "nes/start":
      requireBridgeIdentifier(value, "requestId");
      break;
    case "nes/suggest":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      requireBoundedString(value, "uri", MAX_BRIDGE_PATH_LENGTH);
      requirePosition(value.position, "nes/suggest position");
      if (value.selection != null) requireRange(value.selection, "nes/suggest selection");
      if (!["automatic", "diagnostic", "manual"].includes(String(value.triggerKind))) {
        throw new Error("nes/suggest has an invalid triggerKind");
      }
      break;
    case "nes/accept":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      requireBridgeIdentifier(value, "suggestionId");
      if (value.text != null) {
        requireStringValue(value.text, "nes/accept text");
        if (value.text.length > 2_000_000) throw new Error("nes/accept text is too large");
      }
      break;
    case "nes/reject":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      requireBridgeIdentifier(value, "suggestionId");
      if (
        value.reason != null &&
        !["rejected", "ignored", "replaced", "cancelled"].includes(String(value.reason))
      ) {
        throw new Error("nes/reject has an invalid reason");
      }
      break;
    case "nes/close":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      break;
    case "document/open":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      requireBoundedString(value, "path", MAX_BRIDGE_PATH_LENGTH);
      requireBoundedString(value, "languageId", MAX_BRIDGE_LANGUAGE_ID_LENGTH);
      break;
    case "document/change":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      requireBoundedString(value, "uri", MAX_BRIDGE_PATH_LENGTH);
      requireStringValue(value.text, "document/change text");
      if (value.text.length > 2_000_000) throw new Error("document/change text is too large");
      break;
    case "document/save":
    case "document/close":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "sessionId");
      requireBoundedString(value, "uri", MAX_BRIDGE_PATH_LENGTH);
      break;
    case "document/focus":
      requireBridgeIdentifier(value, "sessionId");
      requireBoundedString(value, "uri", MAX_BRIDGE_PATH_LENGTH);
      requirePosition(value.position, "document/focus position");
      requireRange(value.visibleRange, "document/focus visibleRange");
      break;
    case "permission/respond":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "permissionId");
      if (!isRecord(value.outcome)) throw new Error("permission/respond requires an outcome");
      if (value.outcome.outcome === "selected") requireBridgeIdentifier(value.outcome, "optionId");
      else if (value.outcome.outcome !== "cancelled") throw new Error("Invalid permission outcome");
      break;
    case "elicitation/respond":
      requireBridgeIdentifier(value, "requestId");
      requireBridgeIdentifier(value, "elicitationId");
      if (!isRecord(value.response) || typeof value.response.action !== "string") {
        throw new Error("elicitation/respond requires a response action");
      }
      if (value.response.action.length > MAX_BRIDGE_TYPE_LENGTH) {
        throw new Error("elicitation/respond action is too long");
      }
      if (!["accept", "decline", "cancel"].includes(value.response.action)) {
        throw new Error(`Unsupported elicitation response action: ${value.response.action}`);
      }
      if (value.response.action !== "accept" && value.response.content != null) {
        throw new Error("Only accepted elicitation responses may contain content");
      }
      if (value.response.content != null) {
        if (!isRecord(value.response.content)) {
          throw new Error("elicitation/respond content must be an object");
        }
        for (const contentValue of Object.values(value.response.content)) {
          if (!isElicitationValue(contentValue)) {
            throw new Error("elicitation/respond contains an invalid content value");
          }
        }
      }
      break;
    default:
      throw new Error(`Unknown WebSocket command: ${value.type}`);
  }
  return value as ClientCommand;
}

function requireTerminalSize(cols: unknown, rows: unknown, label: string): void {
  if (!Number.isSafeInteger(cols) || Number(cols) < 2 || Number(cols) > 500) {
    throw new Error(`${label} cols must be an integer between 2 and 500`);
  }
  if (!Number.isSafeInteger(rows) || Number(rows) < 2 || Number(rows) > 300) {
    throw new Error(`${label} rows must be an integer between 2 and 300`);
  }
}

function validatePrompt(prompt: unknown[]): void {
  if (prompt.length === 0 || prompt.length > 64) {
    throw new Error("session/prompt requires between 1 and 64 content blocks");
  }
  for (const block of prompt) {
    if (!isRecord(block) || typeof block.type !== "string") {
      throw new Error("session/prompt contains an invalid content block");
    }
    if (block.type.length > MAX_BRIDGE_TYPE_LENGTH) {
      throw new Error("session/prompt content block type is too long");
    }
    switch (block.type) {
      case "text":
        requireStringValue(block.text, "text block text");
        break;
      case "image":
        requireStringValue(block.data, "image block data");
        requireStringValue(block.mimeType, "image block mimeType");
        requireOptionalString(block.uri, "image block uri");
        break;
      case "audio":
        requireStringValue(block.data, "audio block data");
        requireStringValue(block.mimeType, "audio block mimeType");
        break;
      case "resource_link":
        requireStringValue(block.name, "resource link name");
        requireStringValue(block.uri, "resource link uri");
        requireOptionalString(block.description, "resource link description");
        requireOptionalString(block.mimeType, "resource link mimeType");
        requireOptionalString(block.title, "resource link title");
        if (block.size != null && typeof block.size !== "number") {
          throw new Error("resource link size must be a number when provided");
        }
        break;
      case "resource": {
        if (!isRecord(block.resource)) {
          throw new Error("resource block requires a resource object");
        }
        requireStringValue(block.resource.uri, "embedded resource uri");
        const hasText = Object.hasOwn(block.resource, "text");
        const hasBlob = Object.hasOwn(block.resource, "blob");
        if (hasText === hasBlob) {
          throw new Error("embedded resource requires exactly one of text or blob");
        }
        if (hasText) requireStringValue(block.resource.text, "embedded resource text");
        if (hasBlob) requireStringValue(block.resource.blob, "embedded resource blob");
        requireOptionalString(block.resource.mimeType, "embedded resource mimeType");
        break;
      }
      default:
        throw new Error(`Unsupported prompt content block: ${block.type}`);
    }
    validateContentBlockSemantics(block as ContentBlock, "Prompt content block");
  }
}

function requireStringValue(value: unknown, label: string): asserts value is string {
  if (typeof value !== "string") throw new Error(`${label} must be a string`);
}

function requireOptionalString(value: unknown, label: string): void {
  if (value != null && typeof value !== "string") {
    throw new Error(`${label} must be a string when provided`);
  }
}

function isElicitationValue(value: unknown): boolean {
  return (
    typeof value === "string" ||
    (typeof value === "number" && Number.isFinite(value)) ||
    typeof value === "boolean" ||
    (Array.isArray(value) && value.every((item) => typeof item === "string"))
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function requirePosition(value: unknown, label: string): asserts value is Position {
  if (!isRecord(value)) throw new Error(`${label} must be an object`);
  if (
    !Number.isSafeInteger(value.line) ||
    !Number.isSafeInteger(value.character) ||
    Number(value.line) < 0 ||
    Number(value.character) < 0
  ) {
    throw new Error(`${label} must contain non-negative integer coordinates`);
  }
}

function requireRange(value: unknown, label: string): asserts value is Range {
  if (!isRecord(value)) throw new Error(`${label} must be an object`);
  requirePosition(value.start, `${label}.start`);
  requirePosition(value.end, `${label}.end`);
}

function requireBridgeIdentifier(value: Record<string, unknown>, key: string): void {
  requireBoundedString(value, key, MAX_BRIDGE_IDENTIFIER_LENGTH);
}

function requireBoundedString(
  value: Record<string, unknown>,
  key: string,
  maximum: number,
): void {
  requireBoundedStringValue(
    value[key],
    `${String(value.type ?? "message")} ${key}`,
    maximum,
  );
}

function requireBoundedStringValue(
  value: unknown,
  label: string,
  maximum: number,
): asserts value is string {
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${label} is required`);
  }
  if (value.length > maximum) {
    throw new Error(`${label} exceeds ${maximum} characters`);
  }
}

function requireString(value: Record<string, unknown>, key: string): void {
  if (typeof value[key] !== "string" || value[key].length === 0) {
    throw new Error(`${String(value.type ?? "message")} requires ${key}`);
  }
}
