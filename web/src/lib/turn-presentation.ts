import { assertNever } from "../../../shared/exhaustive";
import type { TimelineItem } from "./state";

type AssistantItem = Extract<TimelineItem, { type: "assistant" }>;

export interface TurnPresentation {
  prompts: TimelineItem[];
  process: TimelineItem[];
  output?: AssistantItem;
  outcomes: TimelineItem[];
}

export function splitTurnPresentation(items: TimelineItem[]): TurnPresentation {
  let outputItemIndex = -1;
  let outputChunkIndex = -1;
  for (let itemIndex = items.length - 1; itemIndex >= 0; itemIndex -= 1) {
    const item = items[itemIndex];
    if (item.type !== "assistant") continue;
    for (let chunkIndex = item.chunks.length - 1; chunkIndex >= 0; chunkIndex -= 1) {
      if (item.chunks[chunkIndex].role !== "agent") continue;
      outputItemIndex = itemIndex;
      outputChunkIndex = chunkIndex;
      break;
    }
    if (outputItemIndex !== -1) break;
  }

  const presentation: TurnPresentation = { prompts: [], process: [], outcomes: [] };
  for (const [itemIndex, item] of items.entries()) {
    switch (item.type) {
      case "message":
        presentation.prompts.push(item);
        break;
      case "assistant": {
        if (itemIndex !== outputItemIndex) {
          presentation.process.push(item);
          break;
        }
        if (item.chunks.length === 1) {
          presentation.output = item;
          break;
        }
        // Presentation-only copies keep every original chunk available without
        // changing the timeline used by history, search, or Markdown export.
        presentation.output = { ...item, chunks: [item.chunks[outputChunkIndex]] };
        presentation.process.push({
          ...item,
          id: `${item.id}:process`,
          chunks: item.chunks.filter((_, chunkIndex) => chunkIndex !== outputChunkIndex),
        });
        break;
      }
      case "stop":
      case "error":
        presentation.outcomes.push(item);
        break;
      case "tool":
      case "plan":
      case "compaction":
      case "protocol":
        presentation.process.push(item);
        break;
      default:
        assertNever(item, "turn presentation item");
    }
  }
  return presentation;
}
