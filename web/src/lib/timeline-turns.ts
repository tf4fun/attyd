import type { TimelineItem } from "./state";

export function timelineTurnStarts(timeline: TimelineItem[]): number[] {
  const starts = [0];
  let operationId: string | undefined;
  for (const [index, item] of timeline.entries()) {
    if (item.type === "message") {
      // Loaded histories may omit outcomes. A new prompt still separates turns;
      // multiple user chunks with the same operation ID belong to one prompt.
      if ((operationId == null || item.turnOperationId !== operationId) && starts.at(-1) !== index) {
        starts.push(index);
      }
      operationId = item.turnOperationId;
    }
    if (item.type === "stop" || (item.type === "error" && item.operation === "session/prompt")) {
      starts.push(index + 1);
      operationId = undefined;
    }
  }
  return starts;
}
