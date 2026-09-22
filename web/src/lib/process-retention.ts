import type { TerminalSnapshot } from "../../../shared/bridge";
import { sameSessionOwner, type BridgeSessionView, type CollapsedTurn } from "./business-api";
import type { DeferredTurnProcess, TimelineItem } from "./state";
import { splitTurnPresentation } from "./turn-presentation";

/** Reopening a completed turn within this window reuses its loaded process. */
export const COLLAPSED_PROCESS_RETENTION_MS = 5 * 60 * 1000;

export function sameTurnProcess(left: DeferredTurnProcess | undefined, right: DeferredTurnProcess): boolean {
  return left != null && sameSessionOwner(left.owner, right.owner) &&
    left.turnId === right.turnId && left.historyRevision === right.historyRevision;
}

export function releaseTimelineProcess(timeline: TimelineItem[], process: DeferredTurnProcess): TimelineItem[] {
  const first = timeline.findIndex((item) => sameTurnProcess(item.retainedProcess, process));
  if (first < 0) return timeline;
  let end = first + 1;
  while (end < timeline.length && timeline[end].historyTurnId === process.turnId) end += 1;
  const presentation = splitTurnPresentation(timeline.slice(first, end));
  const visible = [...presentation.prompts, ...(presentation.output ? [presentation.output] : []), ...presentation.outcomes];
  if (visible.length === 0) visible.push({ id: `history:${process.turnId}:empty`, type: "assistant", chunks: [] });
  return [...timeline.slice(0, first), ...visible.map((item, index): TimelineItem => {
    const { retainedProcess: _retained, deferredProcess: _deferred, ...entry } = item;
    return { ...entry, historyTurnId: process.turnId, ...(index === 0 ? { deferredProcess: process } : {}) };
  }), ...timeline.slice(end)];
}

function terminalReferences(value: unknown, ids = new Set<string>()): Set<string> {
  if (value == null || typeof value !== "object") return ids;
  if (Array.isArray(value)) {
    for (const item of value) terminalReferences(item, ids);
  } else {
    const object = value as Record<string, unknown>;
    if (typeof object.terminalId === "string") ids.add(object.terminalId);
    for (const item of Object.values(object)) terminalReferences(item, ids);
  }
  return ids;
}

/** Drop only retired outputs made unreachable by this release; live resources stay. */
export function releaseUnreferencedTerminals(
  terminals: TerminalSnapshot[], before: unknown, after: unknown,
): TerminalSnapshot[] {
  const candidates = terminalReferences(before);
  const retained = terminalReferences(after);
  return terminals.filter((terminal) => !candidates.has(terminal.terminalId) || retained.has(terminal.terminalId) ||
    (!terminal.released && terminal.exitStatus == null));
}

/** Remove bodies from the hook's raw snapshot too, including late refresh responses. */
export function releaseSessionViewProcess(
  view: BridgeSessionView, shouldRelease: (turn: CollapsedTurn) => boolean,
): BridgeSessionView {
  const turns = view.collapsedTurns;
  if (turns == null || !turns.some((turn) => turn.processIncluded && turn.visibleRanges != null && shouldRelease(turn))) return view;
  const timeline: BridgeSessionView["timeline"] = [];
  const appendRange = (start: number, end: number) => {
    for (let index = start; index < end; index += 1) timeline.push(view.timeline[index]);
  };
  const collapsedTurns = turns.map((turn): CollapsedTurn => {
    const beforeUpdate = timeline.length;
    const release = turn.processIncluded && turn.visibleRanges != null && shouldRelease(turn);
    let visibleRanges = turn.visibleRanges?.map(({ start, end }) => ({
      start: start - turn.beforeUpdate + beforeUpdate, end: end - turn.beforeUpdate + beforeUpdate,
    }));
    if (release) {
      visibleRanges = turn.visibleRanges!.map(({ start, end }) => {
        const offset = timeline.length;
        appendRange(start, end);
        return { start: offset, end: timeline.length };
      });
    } else appendRange(turn.beforeUpdate, turn.afterUpdate);
    return { ...turn, beforeUpdate, afterUpdate: timeline.length, visibleRanges,
      ...(release ? { processIncluded: false } : {}),
      outcomes: turn.outcomes.map((outcome) => ({ ...outcome, afterUpdate: timeline.length })),
    };
  });
  const terminals = releaseUnreferencedTerminals(Object.values(view.terminals), view.timeline,
    [timeline, view.activeTurn, view.interactions]);
  return { ...view, timeline, collapsedTurns, turnOutcomes: collapsedTurns.flatMap((turn) => turn.outcomes),
    terminals: Object.fromEntries(terminals.map((terminal) => [terminal.terminalId, terminal])),
  };
}
