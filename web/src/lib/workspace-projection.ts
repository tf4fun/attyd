// The parts of the web UI's projection that do not depend on a scope's entries (docs/protocol.md,
// C1): error shaping, authentication, URL flows, terminals and the session list. Every value is
// computed afresh from what it is given; nothing is carried over from an earlier projection.

import type { SessionInfo } from "@agentclientprotocol/sdk";
import type { TerminalSnapshot } from "../../../shared/bridge";
import type { GlobalLive, Operation, ProtocolError, PromptBlock, UrlFlow } from "../../../shared/protocol";
import type { ExternalElicitationFlow, TimelineItem } from "./state";

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function promptInput(op: Operation): PromptBlock[] {
  const input = op.input;
  return isRecord(input) && Array.isArray(input.prompt) ? input.prompt as PromptBlock[] : [];
}

/** The ACP error an `agent_error` carries, if any (§9). */
export function agentErrorCode(error: ProtocolError | null | undefined): number | undefined {
  if (error?.code !== "agent_error" || !isRecord(error.data)) return undefined;
  return typeof error.data.code === "number" ? error.data.code : undefined;
}

/** Whether the Agent refused for want of authentication (ACP `auth_required`). */
export function needsAuthentication(error: ProtocolError | null | undefined): boolean {
  return agentErrorCode(error) === -32_000;
}

export function operationError(id: string, op: Operation, error: ProtocolError): Extract<TimelineItem, { type: "error" }> {
  const agent = error.code === "agent_error" && isRecord(error.data) ? error.data : undefined;
  const retry = op.kind === "prompt" ? promptInput(op) : [];
  return {
    id,
    type: "error",
    message: agent != null && typeof agent.message === "string" ? agent.message : error.message,
    requestId: op.id,
    operation: op.kind === "prompt" ? "session/prompt" : op.kind,
    code: agent != null && typeof agent.code === "number" ? agent.code : undefined,
    data: agent != null ? agent.data : error.data,
    ...(retry.length > 0 ? { retryBlocks: retry } : {}),
  };
}

/** The terminals no tool card of the thread shows: they live on after their operation (S7). */
export function standaloneTerminals(terminals: readonly TerminalSnapshot[], timeline: readonly TimelineItem[]): TerminalSnapshot[] {
  const shown = new Set(timeline.flatMap((item) => item.type === "tool"
    ? (item.call.content ?? []).flatMap((content) => content.type === "terminal" ? [content.terminalId] : [])
    : []));
  return terminals.filter(({ terminalId }) => !shown.has(terminalId));
}

export function externalFlow(flow: UrlFlow, sessionId?: string): ExternalElicitationFlow {
  const request = flow.request as { message?: unknown; url?: unknown };
  return {
    elicitationId: flow.interactionId,
    ...(sessionId ? { sessionId } : {}),
    ...(typeof request.url === "string" ? { url: request.url } : {}),
    message: typeof request.message === "string" ? request.message : flow.elicitationId,
    status: "waiting",
  };
}

/**
 * The sessions the page lists: the catalog's pages and every session the
 * bridge holds (`global.sessions`), less those a delete has removed or is
 * removing.
 */
export function listedSessions(catalog: readonly SessionInfo[], global: GlobalLive | undefined): SessionInfo[] {
  const op: Operation | undefined = global?.op;
  const deleting = op?.kind === "delete" && op.status !== "failed" && op.status !== "cancelled" &&
    isRecord(op.input) && typeof op.input.sessionId === "string" ? op.input.sessionId : undefined;
  const sessions = new Map(catalog.map((session) => [session.sessionId, session]));
  for (const summary of Object.values(global?.sessions ?? {})) {
    const listed = sessions.get(summary.sessionId);
    // A held session's summary is current: its `null` title is a cleared title.
    sessions.set(summary.sessionId, {
      ...listed,
      sessionId: summary.sessionId,
      cwd: summary.cwd,
      title: summary.title,
    });
  }
  if (deleting != null) sessions.delete(deleting);
  return [...sessions.values()];
}
