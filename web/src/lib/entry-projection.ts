// The thread of a session projected from the entries of its document (docs/protocol.md
// §3, bridge memory model §3–§5). The entries are in the order they were received:
// messages, tool calls, plans and compactions. Turns are derived from them, and each
// prompt's end closes its turn. Every value is computed afresh; nothing is carried over
// from an earlier projection (C1).

import type { PromptResponse } from "@agentclientprotocol/sdk";
import type { PromptBlock } from "../../../shared/protocol";
import type { Chunk, Entry, MessageEntry, PlanEntry, PromptState } from "../../../shared/entries";
import type { AgentActivity, AssistantMessageChunk, TimelineItem, ViewToolCall } from "./state";

export interface ProjectedEntries {
  timeline: TimelineItem[];
  /** An unfinished plan of a running prompt, shown above the composer instead of in the thread. */
  activePlan?: Extract<TimelineItem, { type: "plan" }>;
  /** What the Agent is doing now: only while a prompt runs. */
  activity?: AgentActivity;
}

interface Turn {
  start: number;
  end: number;
  prompt: PromptState | undefined;
}

function isUser(entry: Entry): entry is MessageEntry {
  return entry.kind === "message" && entry.role === "user";
}

/**
 * The turns of `entries`: one starts at each user message, except one whose
 * `messageId` is that of the message that started the current turn; entries before
 * the first user message form a first turn.
 */
function turnsOf(entries: readonly Entry[], prompts: readonly PromptState[]): Turn[] {
  const starts: number[] = [];
  let opener: MessageEntry | undefined;
  entries.forEach((entry, index) => {
    if (!isUser(entry)) return;
    if (opener?.messageId !== undefined && opener.messageId === entry.messageId) return;
    opener = entry;
    starts.push(index);
  });
  if (starts[0] !== 0 && entries.length > 0) starts.unshift(0);
  return starts.map((start, n) => ({
    start,
    end: starts[n + 1] ?? entries.length,
    prompt: prompts.find((prompt) => prompt.entry === start),
  }));
}

/** The blocks of `chunks`, adjacent text joined when nothing but the text differs. */
function blocksOf(chunks: readonly Chunk[]): PromptBlock[] {
  const blocks: PromptBlock[] = [];
  for (const { content } of chunks) {
    const block = content as PromptBlock;
    const last = blocks.at(-1);
    if (
      last?.type === "text" && block?.type === "text" &&
      JSON.stringify({ ...last, text: "" }) === JSON.stringify({ ...block, text: "" })
    ) {
      blocks[blocks.length - 1] = { ...last, text: last.text + block.text };
    } else {
      blocks.push(block);
    }
  }
  return blocks;
}

function planUpdate(entry: PlanEntry): Extract<TimelineItem, { type: "plan" }>["update"] {
  if (entry.legacy !== undefined) return { sessionUpdate: "plan", ...entry.legacy } as never;
  const update = entry.update ?? {};
  return (update.plan !== undefined
    ? { sessionUpdate: "plan_update", ...update }
    : { sessionUpdate: "plan_removed", ...update }) as never;
}

/** The steps a plan entry holds, if it holds a plan. */
function planSteps(entry: PlanEntry): readonly { readonly status?: string }[] {
  return entry.legacy?.entries ?? entry.update?.plan?.entries ?? [];
}

function unfinished(entry: PlanEntry): boolean {
  return planSteps(entry).some(({ status }) => status !== "completed");
}

function toolCall(entry: Extract<Entry, { kind: "tool" }>): ViewToolCall {
  const call = entry.call as Partial<ViewToolCall>;
  return { ...call, toolCallId: entry.call.toolCallId, title: call.title ?? "Tool" } as ViewToolCall;
}

function activityOf(entries: readonly Entry[]): AgentActivity {
  const index = entries.length - 1;
  const last = entries[index];
  switch (last?.kind) {
    case "message":
      if (last.role === "agent") return { kind: "responding" };
      if (last.role === "thought") return { kind: "thinking", timelineId: `entry:${index}` };
      return { kind: "waiting" };
    case "tool": {
      const call = toolCall(last);
      return call.status == null || call.status === "pending" || call.status === "in_progress"
        ? { kind: "tool", toolCallId: call.toolCallId, title: call.title }
        : { kind: "waiting" };
    }
    case "plan":
      return unfinished(last) ? { kind: "planning" } : { kind: "waiting" };
    case "compaction":
      return last.status === "in_progress" ? { kind: "compacting" } : { kind: "waiting" };
    default:
      return { kind: "waiting" };
  }
}

/** The item that closes a turn whose prompt ended. */
function outcome(turn: Turn, entries: readonly Entry[]): TimelineItem | undefined {
  const end = turn.prompt?.end;
  if (end === undefined) return undefined;
  const historyTurnId = `turn:${turn.start}`;
  if ("response" in end) {
    return { id: `${historyTurnId}:stop`, type: "stop", response: end.response as unknown as PromptResponse, historyTurnId };
  }
  const asked = entries.slice(turn.start, turn.end).filter(isUser).flatMap((entry) => blocksOf(entry.chunks));
  const retry = asked.length > 0 ? { retryBlocks: asked } : {};
  if ("error" in end) {
    return {
      id: `${historyTurnId}:error`, type: "error", operation: "session/prompt", historyTurnId,
      message: end.error.message ?? "The Agent returned an error.", code: end.error.code, data: end.error.data, ...retry,
    };
  }
  return {
    id: `${historyTurnId}:error`, type: "error", operation: "session/prompt", historyTurnId,
    message: "The connection to the Agent ended before it answered the prompt.", ...retry,
  };
}

export function projectEntries(
  sessionId: string,
  entries: readonly Entry[],
  prompts: readonly PromptState[],
): ProjectedEntries {
  void sessionId;
  const turns = turnsOf(entries, prompts);
  const running = prompts.some((prompt) => prompt.end === undefined);
  // The last plan of a running prompt, while it has unfinished steps, is shown above the composer.
  let activeIndex = -1;
  if (running) {
    const last = entries.findLastIndex((entry) => entry.kind === "plan");
    const plan = entries[last];
    if (plan?.kind === "plan" && unfinished(plan)) activeIndex = last;
  }
  const timeline: TimelineItem[] = [];
  let activePlan: ProjectedEntries["activePlan"];
  for (const turn of turns) {
    const historyTurnId = `turn:${turn.start}`;
    for (let index = turn.start; index < turn.end; index += 1) {
      const entry = entries[index];
      const id = `entry:${index}`;
      switch (entry.kind) {
        case "message": {
          if (entry.role === "user") {
            timeline.push({
              id, type: "message", role: "user", blocks: blocksOf(entry.chunks),
              messageId: entry.messageId, raw: [entry], historyTurnId,
            });
            break;
          }
          const chunk: AssistantMessageChunk = {
            id, role: entry.role, blocks: blocksOf(entry.chunks), messageId: entry.messageId, raw: [entry],
          };
          // Consecutive assistant messages are one item, a chunk each.
          const previous = timeline.at(-1);
          if (previous?.type === "assistant" && previous.historyTurnId === historyTurnId && index > turn.start &&
            entries[index - 1].kind === "message") {
            timeline[timeline.length - 1] = { ...previous, chunks: [...previous.chunks, chunk] };
          } else {
            timeline.push({ id, type: "assistant", chunks: [chunk], historyTurnId });
          }
          break;
        }
        case "tool":
          timeline.push({ id, type: "tool", call: toolCall(entry), raw: [entry], historyTurnId });
          break;
        case "plan": {
          const item = { id, type: "plan" as const, update: planUpdate(entry), raw: [entry], historyTurnId };
          if (index === activeIndex) activePlan = item;
          else timeline.push(item);
          break;
        }
        case "compaction":
          timeline.push({
            id, type: "compaction", compactionId: entry.compactionId, status: entry.status as never,
            blocks: blocksOf(entry.summary), ...(entry.error !== undefined ? { error: entry.error } : {}),
            raw: [entry], historyTurnId,
          });
          break;
      }
    }
    const closing = outcome(turn, entries);
    if (closing !== undefined) timeline.push(closing);
  }
  return { timeline, activePlan, activity: running ? activityOf(entries) : undefined };
}
