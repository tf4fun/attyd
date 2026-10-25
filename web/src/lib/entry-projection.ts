// The thread of a session projected from the entries of its document (docs/protocol-r4-design.md
// §4.3, bridge memory model §3–§5). The entries are in the order they were received:
// messages, tool calls, plans and compactions. The turns are the partition the bridge publishes
// (the client derives none), and each prompt's end closes its turn. The projection is a pure
// function of the entries and the turns (C1):
// an item is reused only for the very entry objects it was computed from, which a
// replica freezes, so a view can skip the items an update left unchanged.

import type { PromptResponse } from "@agentclientprotocol/sdk";
import type { PromptBlock } from "../../../shared/protocol";
import type { Chunk, Entry, MessageEntry, PlanEntry, PromptState, TurnHead } from "../../../shared/entries";
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

/** Values derived from one entry object, with the key (id and turn) they were made for. */
const derived = new WeakMap<object, { key: string; value: unknown }>();
/** The assistant item a run of messages formed, by its first entry. */
const runs = new WeakMap<object, Extract<TimelineItem, { type: "assistant" }>>();

/** What `make` derives from `entry` for `key`, reused while both are the same. */
function derive<T>(entry: Entry, key: string, make: () => T): T {
  const held = derived.get(entry);
  if (held?.key === key) return held.value as T;
  const value = make();
  derived.set(entry, { key, value });
  return value;
}

/** `item`, or the identical one made earlier from the same chunks. */
function reuseRun(item: TimelineItem): TimelineItem {
  if (item.type !== "assistant") return item;
  const first = item.chunks[0].raw[0] as object;
  const held = runs.get(first);
  if (held?.id === item.id && held.historyTurnId === item.historyTurnId &&
    held.chunks.length === item.chunks.length && held.chunks.every((chunk, n) => chunk === item.chunks[n])) {
    return held;
  }
  runs.set(first, item);
  return item;
}

function isUser(entry: Entry): entry is MessageEntry {
  return entry.kind === "message" && entry.role === "user";
}

/**
 * The turns the bridge published, each with the prompt its first entry sent. Entries the index
 * does not cover (it is published with them, so none) would be left out of the thread; an index
 * missing for entries that exist is one turn of all of them.
 */
function turnsFrom(heads: readonly TurnHead[], entries: readonly Entry[], prompts: readonly PromptState[]): Turn[] {
  if (heads.length === 0) {
    return entries.length === 0 ? [] : [{ start: 0, end: entries.length, prompt: prompts.find((prompt) => prompt.entry === 0) }];
  }
  // The prompt of a turn is the first one that opened at its start: found by entry, not by a scan per turn.
  const opened = new Map<number, PromptState>();
  for (const prompt of prompts) if (!opened.has(prompt.entry)) opened.set(prompt.entry, prompt);
  return heads.map(({ start, length }) => ({
    start,
    end: Math.min(start + length, entries.length),
    prompt: opened.get(start),
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
  heads: readonly TurnHead[],
): ProjectedEntries {
  void sessionId;
  const turns = turnsFrom(heads, entries, prompts);
  const running = prompts.some((prompt) => prompt.end === undefined);
  // The last plan of a running prompt, while it has unfinished steps, is shown above the composer.
  let activeIndex = -1;
  if (running) {
    let last = entries.length - 1;
    while (last >= 0 && entries[last].kind !== "plan") last -= 1;
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
            timeline.push(derive(entry, `${id} ${historyTurnId}`, (): TimelineItem => ({
              id, type: "message", role: "user", blocks: blocksOf(entry.chunks),
              messageId: entry.messageId, raw: [entry], historyTurnId,
            })));
            break;
          }
          const { role } = entry;
          const chunk = derive(entry, id, (): AssistantMessageChunk => ({
            id, role, blocks: blocksOf(entry.chunks), messageId: entry.messageId, raw: [entry],
          }));
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
          timeline.push(derive(entry, `${id} ${historyTurnId}`, (): TimelineItem => ({
            id, type: "tool", call: toolCall(entry), raw: [entry], historyTurnId,
          })));
          break;
        case "plan": {
          const item = derive(entry, `${id} ${historyTurnId}`, () => ({
            id, type: "plan" as const, update: planUpdate(entry), raw: [entry], historyTurnId,
          }));
          if (index === activeIndex) activePlan = item;
          else timeline.push(item);
          break;
        }
        case "compaction":
          timeline.push(derive(entry, `${id} ${historyTurnId}`, (): TimelineItem => ({
            id, type: "compaction", compactionId: entry.compactionId, status: entry.status as never,
            blocks: blocksOf(entry.summary), ...(entry.error !== undefined ? { error: entry.error } : {}),
            raw: [entry], historyTurnId,
          })));
          break;
      }
    }
    const closing = outcome(turn, entries);
    if (closing !== undefined) timeline.push(closing);
  }
  return { timeline: timeline.map(reuseRun), activePlan, activity: running ? activityOf(entries) : undefined };
}
