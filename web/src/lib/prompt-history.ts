import type { ContentBlock } from "@agentclientprotocol/sdk";
import type { TimelineItem } from "./state";

export const MAX_PROMPT_HISTORY = 100;

/**
 * Builds input history from the active ACP thread itself, including history
 * replayed by an Agent during load/resume. No second client session store is
 * introduced.
 */
export function collectPromptHistory(
  timeline: TimelineItem[],
  limit = MAX_PROMPT_HISTORY,
): ContentBlock[][] {
  const boundedLimit = Number.isFinite(limit)
    ? Math.min(MAX_PROMPT_HISTORY, Math.max(0, Math.trunc(limit)))
    : MAX_PROMPT_HISTORY;
  if (boundedLimit === 0) return [];
  const prompts: Array<{ blocks: ContentBlock[]; role: "user" | "protocol-user" }> = [];

  for (let index = 0; index < timeline.length; index += 1) {
    const item = timeline[index];
    if (
      item.type !== "message" ||
      (item.role !== "user" && item.role !== "protocol-user") ||
      item.blocks.length === 0
    ) continue;

    const previousItem = timeline[index - 1];
    const previousPrompt = prompts.at(-1);
    const isAdjacentProtocolEcho =
      previousPrompt != null &&
      previousItem?.type === "message" &&
      (previousItem.role === "user" || previousItem.role === "protocol-user") &&
      previousPrompt.role !== item.role &&
      contentBlocksEqual(previousPrompt.blocks, item.blocks);

    if (!isAdjacentProtocolEcho) {
      prompts.push({ blocks: item.blocks, role: item.role });
    }
  }

  return prompts.slice(-boundedLimit).map(({ blocks }) => blocks);
}

function contentBlocksEqual(left: ContentBlock[], right: ContentBlock[]): boolean {
  return jsonValueEqual(left, right);
}

function jsonValueEqual(left: unknown, right: unknown): boolean {
  if (left === right) return true;
  if (left == null || right == null || typeof left !== "object" || typeof right !== "object") {
    return false;
  }
  if (Array.isArray(left) || Array.isArray(right)) {
    return Array.isArray(left) &&
      Array.isArray(right) &&
      left.length === right.length &&
      left.every((value, index) => jsonValueEqual(value, right[index]));
  }

  const leftRecord = left as Record<string, unknown>;
  const rightRecord = right as Record<string, unknown>;
  const leftKeys = Object.keys(leftRecord).filter((key) => leftRecord[key] !== undefined);
  const rightKeys = Object.keys(rightRecord).filter((key) => rightRecord[key] !== undefined);
  return leftKeys.length === rightKeys.length &&
    leftKeys.every((key) =>
      Object.hasOwn(rightRecord, key) && jsonValueEqual(leftRecord[key], rightRecord[key])
    );
}
