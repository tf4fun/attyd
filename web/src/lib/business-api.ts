import type {
  ContentBlock,
  CreateElicitationRequest,
  InitializeResponse,
  NewSessionResponse,
  PromptResponse,
  RequestPermissionRequest,
  SessionInfo,
  SessionUpdate,
} from "@agentclientprotocol/sdk";
import type {
  AgentTransport,
  ConnectionPhase,
  TerminalSnapshot,
  WorkspaceContextAttachment,
  WorkspaceContextMatch,
} from "../../../shared/bridge";
import { parseServerEvent } from "../../../shared/bridge";

export type SessionSyncPhase =
  | "cold"
  | "loading"
  | "ready"
  | "running"
  | "reconciling"
  | "blocked";

export interface BridgeTurnOverlay {
  operationId: string;
  clientIntentId: string;
  prompt: ContentBlock[];
  updates: SessionUpdate[];
  terminal: unknown | null;
}

export interface BridgeTurnOutcome {
  operationId: string;
  afterUpdate: number;
  response: PromptResponse;
}

interface PendingInteraction<T> {
  interactionId: string;
  request: T;
  operationId?: string | null;
  respondingOperationId?: string | null;
}

export interface BridgeSessionView {
  bridgeEpoch: string;
  sessionId: string;
  sessionIncarnation: number;
  viewRevision: number;
  historyRevision: string | null;
  phase: SessionSyncPhase;
  syncError: string | null;
  timeline: SessionUpdate[];
  turnOutcomes?: BridgeTurnOutcome[];
  activeTurn: BridgeTurnOverlay | null;
  workspace: {
    cwd: string | null;
    session: Omit<NewSessionResponse, "sessionId"> | null;
  };
  controls: Record<string, SessionUpdate>;
  interactions: {
    permissions: Record<string, PendingInteraction<RequestPermissionRequest>>;
    elicitations: Record<string, PendingInteraction<CreateElicitationRequest>>;
    urlFlows: Record<string, {
      elicitationId: string;
      request: CreateElicitationRequest;
      status: "waiting" | "completed" | "cancelled";
    }>;
  };
  operation: { operationId: string; kind: string; stage: string } | null;
  terminals: Record<string, TerminalSnapshot>;
}

export interface RuntimeView {
  generation: number;
  connected: boolean;
  hello: {
    type: "bridge/hello";
    transport: AgentTransport;
    command: string[];
    cwd: string;
    readOnly: boolean;
    additionalDirectories: string[];
    mcpServers: Array<{ name: string; type: "stdio" | "http" | "sse" | "acp" }>;
  } | null;
  initialized: { type: "acp/initialized"; response: InitializeResponse } | null;
  phase: { type: "bridge/phase"; phase: ConnectionPhase } | null;
  error: { type: "bridge/error"; message: string; code?: number; data?: unknown } | null;
}

export type GlobalBusinessEvent =
  | { type: "bridge/connection"; phase: ConnectionPhase }
  | { type: "bridge/connection_error"; message: string; code?: number; data?: unknown }
  | { type: "acp/authenticated"; requestId: string; methodId: string; response: unknown }
  | { type: "acp/logged_out"; requestId: string; response: unknown }
  | { type: "bridge/auth_terminal_started"; requestId: string; methodId: string }
  | { type: "bridge/auth_terminal_output"; requestId: string; data: string }
  | {
      type: "bridge/auth_terminal_exited";
      requestId: string;
      methodId: string;
      status: "succeeded" | "failed" | "cancelled";
      exitCode: number | null;
      signal?: number;
      message?: string;
    };

export type SessionBusinessEvent =
  | {
      type: "bridge/session_reset";
      bridgeEpoch: string;
      sessionId: string;
      sessionIncarnation: number;
      viewRevision: number;
      historyRevision: string | null;
      phase: SessionSyncPhase;
      syncError: string | null;
    }
  | {
      type: "bridge/session_delta";
      bridgeEpoch: string;
      sessionId: string;
      sessionIncarnation: number;
      fromRevision: number;
      viewRevision: number;
      change: {
        kind: "terminal_update";
        terminal: TerminalSnapshot;
      } | {
        kind: "turn_update" | "sync_state" | "interaction_upsert" | "interaction_remove" | "control_update";
        update?: SessionUpdate;
        [key: string]: unknown;
      };
    }
  | {
      type: "bridge/session_turn_complete";
      bridgeEpoch: string;
      sessionId: string;
      sessionIncarnation: number;
      viewRevision: number;
      historyRevision: string | null;
      phase: SessionSyncPhase;
      operationId: string;
      clientIntentId: string;
      response: PromptResponse;
    }
  | {
      type: "bridge/session_turn_failed";
      bridgeEpoch: string;
      sessionId: string;
      sessionIncarnation: number;
      viewRevision: number;
      historyRevision: string | null;
      phase: SessionSyncPhase;
      operationId: string;
      clientIntentId: string;
      prompt: ContentBlock[];
      error: {
        code?: number;
        message: string;
        data?: unknown;
      };
    };

export interface SessionListResult {
  sessions: SessionInfo[];
  nextCursor?: string | null;
}

export interface CreatedSessionResult {
  sessionId: string;
  cwd?: string;
  view: BridgeSessionView;
}

export interface StartTurnResult {
  operationId: string;
  disposition: "accepted" | "duplicate";
  status: string;
}

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly body?: unknown,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

export async function requestJson<T>(
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const headers = new Headers(init.headers);
  if (init.body != null && !headers.has("content-type")) {
    headers.set("content-type", "application/json");
  }
  const response = await fetch(path, { ...init, headers });
  const text = await response.text();
  let body: unknown;
  if (text !== "") {
    try {
      body = JSON.parse(text);
    } catch {
      body = text;
    }
  }
  if (!response.ok) {
    const message = isRecord(body) && typeof body.error === "string"
      ? body.error
      : `${init.method ?? "GET"} ${path} failed (${response.status})`;
    throw new ApiError(message, response.status, body);
  }
  return body as T;
}

export function parseGlobalBusinessEvent(raw: string): GlobalBusinessEvent {
  const value = parseEventObject(raw);
  switch (value.type) {
    case "bridge/connection":
    case "bridge/connection_error":
    case "acp/authenticated":
    case "acp/logged_out":
    case "bridge/auth_terminal_started":
    case "bridge/auth_terminal_output":
    case "bridge/auth_terminal_exited":
      return value as unknown as GlobalBusinessEvent;
    default:
      throw new Error(`Unknown global event: ${value.type}`);
  }
}

export function parseSessionBusinessEvent(raw: string): SessionBusinessEvent {
  const value = parseEventObject(raw);
  if (
    value.type === "bridge/session_turn_complete" ||
    value.type === "bridge/session_turn_failed"
  ) {
    if (
      typeof value.bridgeEpoch !== "string" ||
      typeof value.sessionId !== "string" ||
      !Number.isSafeInteger(value.sessionIncarnation) ||
      !Number.isSafeInteger(value.viewRevision) ||
      (value.historyRevision !== null && typeof value.historyRevision !== "string") ||
      !isSessionSyncPhase(value.phase) ||
      typeof value.operationId !== "string" ||
      typeof value.clientIntentId !== "string" ||
      (value.type === "bridge/session_turn_complete" && !isRecord(value.response)) ||
      (value.type === "bridge/session_turn_failed" &&
        (!Array.isArray(value.prompt) || !isRecord(value.error) ||
          typeof value.error.message !== "string"))
    ) {
      throw new Error("Session turn event has an invalid payload");
    }
    return value as unknown as SessionBusinessEvent;
  }
  if (value.type !== "bridge/session_reset" && value.type !== "bridge/session_delta") {
    throw new Error(`Unknown session event: ${value.type}`);
  }
  if (
    typeof value.bridgeEpoch !== "string" ||
    typeof value.sessionId !== "string" ||
    !Number.isSafeInteger(value.sessionIncarnation) ||
    !Number.isSafeInteger(value.viewRevision)
  ) {
    throw new Error("Session event has an invalid identity or revision");
  }
  if (
    value.type === "bridge/session_delta" &&
    (!Number.isSafeInteger(value.fromRevision) || !isRecord(value.change))
  ) {
    throw new Error("Session delta has an invalid predecessor or change");
  }
  if (value.type === "bridge/session_delta" && isRecord(value.change) && value.change.kind === "terminal_update") {
    const parsed = parseServerEvent(JSON.stringify({ type: "acp/terminal_state", terminal: value.change.terminal }));
    if (parsed.type !== "acp/terminal_state" || parsed.terminal.sessionId !== value.sessionId) {
      throw new Error("Terminal delta does not match its session identity");
    }
  }
  return value as unknown as SessionBusinessEvent;
}

export function strongEtag(revision: string): string {
  if (revision === "" || /["\r\n]/u.test(revision)) {
    throw new Error("History revision cannot be represented as a strong ETag");
  }
  return `"${revision}"`;
}

export function workspaceContextSearchPath(query: string, sessionId: string): string {
  return `/api/v1/context/search?${new URLSearchParams({ query, sessionId })}`;
}

export type { WorkspaceContextAttachment, WorkspaceContextMatch };

function parseEventObject(raw: string): Record<string, unknown> & { type: string } {
  const value: unknown = JSON.parse(raw);
  if (!isRecord(value) || typeof value.type !== "string") {
    throw new Error("Bridge event must be an object with a type");
  }
  return value as Record<string, unknown> & { type: string };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value != null && typeof value === "object" && !Array.isArray(value);
}

function isSessionSyncPhase(value: unknown): value is SessionSyncPhase {
  return value === "cold" || value === "loading" || value === "ready" ||
    value === "running" || value === "reconciling" || value === "blocked";
}
