// The web UI's projection of protocol v2 documents (docs/protocol.md, C1).
// Every value here is computed afresh from a scope's `log` and `live`; the
// only other input is the page's own UI state, passed in explicitly. Nothing
// is carried over from an earlier projection.

import type { PromptResponse, SessionInfo } from "@agentclientprotocol/sdk";
import type { TerminalSnapshot } from "../../../shared/bridge";
import type {
  Block,
  CatalogLive,
  GlobalLive,
  Interaction,
  LogEntry,
  Operation,
  ProcessUpdate,
  ProtocolError,
  PromptBlock,
  SessionLive,
  UrlFlow,
} from "../../../shared/protocol";
import type {
  AgentActivity,
  AssistantMessageChunk,
  ExternalElicitationFlow,
  TimelineItem,
  ViewToolCall,
} from "./state";

/** A notification as a running prompt's process carries it (§4.4). */
type ProcessNotification = { sessionId: string; update: ProcessUpdate };

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** `live` of a session replica, once it has one. */
export function sessionLive(live: unknown): SessionLive | undefined {
  return isRecord(live) && isRecord(live.op) && typeof live.sessionId === "string" ? live as unknown as SessionLive : undefined;
}

export function globalLive(live: unknown): GlobalLive | undefined {
  return isRecord(live) && isRecord(live.op) && isRecord(live.agent) ? live as unknown as GlobalLive : undefined;
}

export function catalogLive(live: unknown): CatalogLive | undefined {
  return isRecord(live) && isRecord(live.op) && typeof live.revision === "string" ? live as unknown as CatalogLive : undefined;
}

export function promptInput(op: Operation): PromptBlock[] {
  const input = op.input;
  return isRecord(input) && Array.isArray(input.prompt) ? input.prompt as PromptBlock[] : [];
}

export function isPromptResult(value: unknown): value is PromptResponse {
  return isRecord(value) && typeof value.stopReason === "string";
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

/**
 * The thread of a session: its log's turns, each a prompt and its reply, and
 * then the current operation. A completed turn keeps its final messages only
 * (S2): the whole `reply`, every Agent message of the turn, in one entry. A
 * running prompt shows its input and its folded process; a completed one its
 * result, a failed one its error.
 */
export function projectTimeline(sessionId: string, log: readonly LogEntry[], op: Operation | undefined): TimelineItem[] {
  const timeline: TimelineItem[] = [];
  const committed = op?.kind === "prompt" && op.status === "completed";
  log.forEach((entry, index) => {
    const key = `turn:${entry.hash}`;
    // The completed prompt is the log's last entry (S2): it keeps the identity
    // the running turn had, so the thread does not remount it at the commit.
    const turnOperationId = committed && index === log.length - 1 ? op.id : undefined;
    if (entry.turn.prompt.length > 0) {
      timeline.push({
        id: `${key}:prompt`, type: "message", role: "user", blocks: entry.turn.prompt, raw: [],
        historyTurnId: key, ...(turnOperationId ? { turnOperationId } : {}),
      });
    }
    if (entry.turn.reply.length > 0) {
      timeline.push({
        id: `${key}:reply`, type: "assistant", historyTurnId: key,
        chunks: [{ id: `${key}:reply:0`, role: "agent", blocks: entry.turn.reply, raw: [] }],
      });
    }
  });
  if (op == null) return timeline;
  const base = `op:${op.id}`;
  if (op.kind === "prompt") {
    if (op.status === "completed") {
      // The result belongs to the turn the commit appended: the log's last entry.
      const last = log.at(-1);
      if (isPromptResult(op.result)) {
        timeline.push({
          id: `${base}:stop`, type: "stop", response: op.result,
          ...(last != null ? { historyTurnId: `turn:${last.hash}` } : {}),
        });
      }
      return timeline;
    }
    const prompt = promptInput(op);
    if (prompt.length > 0) {
      timeline.push({ id: `${base}:prompt`, type: "message", role: "user", blocks: prompt, raw: [], turnOperationId: op.id });
    }
    timeline.push(...projectProcess(sessionId, op.id, op.process ?? [], prompt).items);
  }
  if (op.status === "failed" && op.error != null) {
    timeline.push(operationError(`${base}:error`, op, op.error));
  }
  return timeline;
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

export interface ProjectedProcess {
  items: TimelineItem[];
  activePlan?: Extract<TimelineItem, { type: "plan" }>;
  activity?: AgentActivity;
}

/**
 * The thread entries of a running prompt's process. The bridge has folded it
 * already (§4.4): chunks of one message are one entry, a tool call update is
 * in its call, a plan replaces the plan before it. Each entry keeps its
 * position as its key.
 */
export function projectProcess(
  sessionId: string,
  operationId: string,
  process: readonly ProcessUpdate[],
  prompt: readonly PromptBlock[] = [],
): ProjectedProcess {
  const base = `op:${operationId}`;
  const items: TimelineItem[] = [];
  let activePlan: ProjectedProcess["activePlan"];
  let activity: AgentActivity | undefined;
  process.forEach((update, index) => {
    const id = `${base}:process:${index}`;
    const notification: ProcessNotification = { sessionId, update };
    switch (update.sessionUpdate) {
      case "user_message_chunk":
        // An Agent may echo the prompt, which the operation's input shows
        // already; any other user message of the turn is shown as it came.
        if (echoesPrompt(update.content, prompt)) return;
        items.push({
          id, type: "message", role: "protocol-user", blocks: [update.content],
          messageId: update.messageId ?? undefined, turnOperationId: operationId, raw: [notification],
        });
        return;
      case "agent_message_chunk":
      case "agent_thought_chunk": {
        const role: AssistantMessageChunk["role"] = update.sessionUpdate === "agent_message_chunk" ? "agent" : "thought";
        const chunk: AssistantMessageChunk = {
          id, role, blocks: [update.content], messageId: update.messageId ?? undefined, raw: [notification],
        };
        const last = items.at(-1);
        if (last?.type === "assistant") items[items.length - 1] = { ...last, chunks: [...last.chunks, chunk] };
        else items.push({ id, type: "assistant", chunks: [chunk] });
        activity = role === "thought" ? { kind: "thinking", timelineId: id } : { kind: "responding" };
        return;
      }
      case "tool_call":
      case "tool_call_update": {
        const call = toolCall(update);
        items.push({ id: `${base}:tool:${call.toolCallId}`, type: "tool", call, raw: [notification] });
        activity = call.status == null || call.status === "pending" || call.status === "in_progress"
          ? { kind: "tool", toolCallId: call.toolCallId, title: call.title }
          : { kind: "waiting" };
        return;
      }
      case "compaction_update":
      case "compaction_summary_chunk": {
        const position = items.findIndex((item) => item.type === "compaction" && item.compactionId === update.compactionId);
        const previous = position >= 0 ? items[position] : undefined;
        const prior = previous?.type === "compaction" ? previous : undefined;
        let blocks: PromptBlock[] = prior?.blocks ?? [];
        let status = prior?.status ?? "in_progress";
        let error = prior?.error;
        if (update.sessionUpdate === "compaction_summary_chunk") {
          blocks = appendText(blocks, update.content);
        } else {
          status = update.status;
          if (update.summary !== undefined) blocks = update.summary ?? [];
          if (update.error !== undefined) error = update.error ?? undefined;
        }
        const item: TimelineItem = {
          id: prior?.id ?? `${base}:compaction:${update.compactionId}`, type: "compaction",
          compactionId: update.compactionId, status, blocks, error,
          raw: [...(prior?.raw ?? []), notification],
        };
        if (prior) items[position] = item;
        else items.push(item);
        activity = status === "in_progress" ? { kind: "compacting" } : { kind: "waiting" };
        return;
      }
      case "plan": {
        const item: Extract<TimelineItem, { type: "plan" }> = { id, type: "plan", update, raw: [notification] };
        // An unfinished plan is shown above the composer; a finished one in its turn.
        if (update.entries.some(({ status }) => status !== "completed")) {
          activePlan = item;
          activity = { kind: "planning" };
        } else {
          activePlan = undefined;
          if (update.entries.length > 0) items.push(item);
          activity = { kind: "waiting" };
        }
        return;
      }
      case "plan_update":
      case "plan_removed":
        items.push({ id, type: "plan", update, raw: [notification] });
        activity = { kind: "planning" };
        return;
      default:
        items.push({ id, type: "protocol", notification: { sessionId, update } });
    }
  });
  return { items, activePlan, activity };
}

function toolCall(update: Extract<ProcessUpdate, { sessionUpdate: "tool_call" | "tool_call_update" }>): ViewToolCall {
  const fields = Object.fromEntries(Object.entries(update).filter(([key, value]) =>
    key !== "sessionUpdate" && value != null)) as Partial<ViewToolCall>;
  return { ...fields, toolCallId: update.toolCallId, title: fields.title ?? "Tool" };
}

/** Whether a user message chunk repeats the prompt the operation was given. */
function echoesPrompt(content: Block, prompt: readonly PromptBlock[]): boolean {
  if (prompt.some((block) => JSON.stringify(block) === JSON.stringify(content))) return true;
  if (content.type !== "text") return false;
  const text = prompt.flatMap((block) => block.type === "text" ? [block.text] : []).join("");
  return content.text.length > 0 && text.includes(content.text);
}

function appendText(blocks: PromptBlock[], incoming: Block): PromptBlock[] {
  const last = blocks.at(-1);
  if (last?.type === "text" && incoming.type === "text" &&
    JSON.stringify({ ...last, text: "" }) === JSON.stringify({ ...incoming, text: "" })) {
    return [...blocks.slice(0, -1), { ...last, text: last.text + incoming.text }];
  }
  return [...blocks, incoming];
}

/** A session's terminals, as the thread's tool cards show them (§4.6). */
export function sessionTerminals(live: SessionLive): TerminalSnapshot[] {
  return Object.values(live.terminals ?? {}).map((terminal) => ({
    sessionId: live.sessionId,
    terminalId: terminal.terminalId,
    output: terminal.output,
    truncated: terminal.truncated,
    exitStatus: terminal.exitStatus,
    released: false,
  }));
}

/** The terminals no tool card of the thread shows: they live on after their operation (S7). */
export function standaloneTerminals(terminals: readonly TerminalSnapshot[], timeline: readonly TimelineItem[]): TerminalSnapshot[] {
  const shown = new Set(timeline.flatMap((item) => item.type === "tool"
    ? (item.call.content ?? []).flatMap((content) => content.type === "terminal" ? [content.terminalId] : [])
    : []));
  return terminals.filter(({ terminalId }) => !shown.has(terminalId));
}

/** The pending interactions of `op`, in the order the bridge allocated them. */
export function pendingInteractions(op: Operation | undefined): Interaction[] {
  return op?.status === "running" ? Object.values(op.interactions ?? {}) : [];
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
