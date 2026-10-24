// The turn index the bridge would publish for some entries (docs/protocol-r4-design.md §4.3,
// bridge memory model §5), for the style showcase and the tests, which write entries by hand. The
// page itself derives no turns: it reads the ones the bridge publishes.
import type { Entry, MessageEntry, PromptState, TurnHead } from "../../../shared/entries";

function isUser(entry: Entry): entry is MessageEntry {
  return entry.kind === "message" && entry.role === "user";
}

/**
 * A turn starts at each user message, except one whose `messageId` is that of the message that
 * started the current turn; entries before the first user message form a first turn.
 */
export function turnHeads(entries: readonly Entry[], prompts: readonly PromptState[], historyRev = 1): TurnHead[] {
  const starts: number[] = [];
  let opener: MessageEntry | undefined;
  entries.forEach((entry, index) => {
    if (!isUser(entry)) return;
    if (opener?.messageId !== undefined && opener.messageId === entry.messageId) return;
    opener = entry;
    starts.push(index);
  });
  if (starts[0] !== 0 && entries.length > 0) starts.unshift(0);
  return starts.map((start, n) => {
    const end = starts[n + 1] ?? entries.length;
    const first = entries[start];
    const prompt = prompts.find((candidate) => candidate.entry === start);
    let output: number | null = null;
    for (let index = end - 1; index >= start; index -= 1) {
      const entry = entries[index];
      if (entry.kind === "message" && entry.role === "agent") {
        output = index;
        break;
      }
    }
    return {
      historyRev,
      start,
      length: end - start,
      ...(isUser(first) && first.messageId !== undefined ? { messageId: first.messageId } : {}),
      state: prompt !== undefined && prompt.end === undefined ? "running" : "ended",
      output,
    } as TurnHead;
  });
}
