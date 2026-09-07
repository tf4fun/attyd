import type { ToolCallStatus } from "@agentclientprotocol/sdk";
import type { TimelineItem } from "./state";
import { timelineTurnStarts } from "./timeline-turns";

const MAX_EXACT_DIFF_CELLS = 500_000;
const CONTEXT_LINES = 3;
const MAX_RENDERED_LINES = 800;

export type ReviewLineKind = "context" | "added" | "removed" | "hunk";

export interface ReviewLine {
  kind: ReviewLineKind;
  text: string;
  oldLine?: number;
  newLine?: number;
}

export interface ReviewDiff {
  id: string;
  path: string;
  toolCallId: string;
  title: string;
  status?: ToolCallStatus;
  oldText?: string | null;
  newText: string;
  addedLines: number;
  removedLines: number;
  lines: ReviewLine[];
  approximate: boolean;
  truncated: boolean;
}

export interface ReviewFile {
  path: string;
  diffs: ReviewDiff[];
  addedLines: number;
  removedLines: number;
  approximate: boolean;
}

export interface ReviewSummary {
  files: ReviewFile[];
  fileCount: number;
  diffCount: number;
  addedLines: number;
  removedLines: number;
  approximate: boolean;
}

export interface ReviewTurn {
  id: string;
  items: TimelineItem[];
  summary: ReviewSummary;
}

export function collectTurnReviewChanges(timeline: TimelineItem[]): ReviewTurn[] {
  const starts = timelineTurnStarts(timeline);
  return starts.filter((start) => start < timeline.length).map((start, index) => {
    const items = timeline.slice(start, starts[index + 1]);
    return { id: items[0].id, items, summary: collectReviewChanges(items) };
  });
}

export function collectReviewChanges(timeline: TimelineItem[]): ReviewSummary {
  const byPath = new Map<string, ReviewFile>();
  let diffCount = 0;
  let addedLines = 0;
  let removedLines = 0;
  let approximate = false;

  for (const item of timeline) {
    if (item.type !== "tool") continue;
    for (const [index, content] of (item.call.content ?? []).entries()) {
      if (content.type !== "diff") continue;
      const result = buildReviewLines(content.oldText, content.newText);
      const diff: ReviewDiff = {
        id: `${item.id}:${index}`,
        path: content.path,
        toolCallId: item.call.toolCallId,
        title: item.call.title,
        status: item.call.status,
        oldText: content.oldText,
        newText: content.newText,
        ...result,
      };
      const file = byPath.get(content.path) ?? {
        path: content.path,
        diffs: [],
        addedLines: 0,
        removedLines: 0,
        approximate: false,
      };
      file.diffs.push(diff);
      file.addedLines += diff.addedLines;
      file.removedLines += diff.removedLines;
      file.approximate ||= diff.approximate;
      byPath.set(content.path, file);
      diffCount += 1;
      addedLines += diff.addedLines;
      removedLines += diff.removedLines;
      approximate ||= diff.approximate;
    }
  }

  const files = [...byPath.values()];
  return {
    files,
    fileCount: files.length,
    diffCount,
    addedLines,
    removedLines,
    approximate,
  };
}

function buildReviewLines(oldText: string | null | undefined, newText: string): Pick<
  ReviewDiff,
  "lines" | "addedLines" | "removedLines" | "approximate" | "truncated"
> {
  const oldLines = oldText == null ? [] : splitLines(oldText);
  const newLines = splitLines(newText);
  let lines: ReviewLine[];
  let approximate = false;

  if (oldText == null) {
    lines = newLines.map((text, index) => ({ kind: "added", text, newLine: index + 1 }));
  } else if (oldLines.length * newLines.length <= MAX_EXACT_DIFF_CELLS) {
    lines = exactLineDiff(oldLines, newLines);
  } else {
    lines = boundedLineDiff(oldLines, newLines);
    approximate = true;
  }

  const addedLines = lines.reduce((count, line) => count + (line.kind === "added" ? 1 : 0), 0);
  const removedLines = lines.reduce((count, line) => count + (line.kind === "removed" ? 1 : 0), 0);
  const compacted = compactContext(lines);
  if (compacted.length <= MAX_RENDERED_LINES) {
    return { lines: compacted, addedLines, removedLines, approximate, truncated: false };
  }
  const firstCount = Math.floor((MAX_RENDERED_LINES - 1) / 2);
  const lastCount = MAX_RENDERED_LINES - firstCount - 1;
  return {
    lines: [
      ...compacted.slice(0, firstCount),
      { kind: "hunk", text: `… ${compacted.length - firstCount - lastCount} review rows omitted …` },
      ...compacted.slice(-lastCount),
    ],
    addedLines,
    removedLines,
    approximate,
    truncated: true,
  };
}

function exactLineDiff(oldLines: string[], newLines: string[]): ReviewLine[] {
  const width = newLines.length + 1;
  const lcs = new Uint32Array((oldLines.length + 1) * width);
  for (let oldIndex = oldLines.length - 1; oldIndex >= 0; oldIndex -= 1) {
    for (let newIndex = newLines.length - 1; newIndex >= 0; newIndex -= 1) {
      const offset = oldIndex * width + newIndex;
      lcs[offset] = oldLines[oldIndex] === newLines[newIndex]
        ? lcs[(oldIndex + 1) * width + newIndex + 1] + 1
        : Math.max(lcs[(oldIndex + 1) * width + newIndex], lcs[offset + 1]);
    }
  }

  const lines: ReviewLine[] = [];
  let oldIndex = 0;
  let newIndex = 0;
  while (oldIndex < oldLines.length || newIndex < newLines.length) {
    if (
      oldIndex < oldLines.length &&
      newIndex < newLines.length &&
      oldLines[oldIndex] === newLines[newIndex]
    ) {
      lines.push({
        kind: "context",
        text: oldLines[oldIndex],
        oldLine: oldIndex + 1,
        newLine: newIndex + 1,
      });
      oldIndex += 1;
      newIndex += 1;
    } else if (
      newIndex < newLines.length &&
      (oldIndex >= oldLines.length ||
        lcs[oldIndex * width + newIndex + 1] > lcs[(oldIndex + 1) * width + newIndex])
    ) {
      lines.push({ kind: "added", text: newLines[newIndex], newLine: newIndex + 1 });
      newIndex += 1;
    } else {
      lines.push({ kind: "removed", text: oldLines[oldIndex], oldLine: oldIndex + 1 });
      oldIndex += 1;
    }
  }
  return lines;
}

function boundedLineDiff(oldLines: string[], newLines: string[]): ReviewLine[] {
  let prefix = 0;
  while (
    prefix < oldLines.length &&
    prefix < newLines.length &&
    oldLines[prefix] === newLines[prefix]
  ) prefix += 1;

  let suffix = 0;
  while (
    suffix < oldLines.length - prefix &&
    suffix < newLines.length - prefix &&
    oldLines[oldLines.length - suffix - 1] === newLines[newLines.length - suffix - 1]
  ) suffix += 1;

  const lines: ReviewLine[] = [];
  for (let index = 0; index < prefix; index += 1) {
    lines.push({ kind: "context", text: oldLines[index], oldLine: index + 1, newLine: index + 1 });
  }
  for (let index = prefix; index < oldLines.length - suffix; index += 1) {
    lines.push({ kind: "removed", text: oldLines[index], oldLine: index + 1 });
  }
  for (let index = prefix; index < newLines.length - suffix; index += 1) {
    lines.push({ kind: "added", text: newLines[index], newLine: index + 1 });
  }
  for (let index = suffix; index > 0; index -= 1) {
    const oldIndex = oldLines.length - index;
    const newIndex = newLines.length - index;
    lines.push({
      kind: "context",
      text: oldLines[oldIndex],
      oldLine: oldIndex + 1,
      newLine: newIndex + 1,
    });
  }
  return lines;
}

function compactContext(lines: ReviewLine[]): ReviewLine[] {
  const changed = lines.flatMap((line, index) =>
    line.kind === "added" || line.kind === "removed" ? [index] : []
  );
  if (changed.length === 0) {
    if (lines.length <= CONTEXT_LINES * 2) return lines;
    return [
      ...lines.slice(0, CONTEXT_LINES),
      { kind: "hunk", text: `… ${lines.length - CONTEXT_LINES * 2} unchanged lines …` },
      ...lines.slice(-CONTEXT_LINES),
    ];
  }

  const keep = new Uint8Array(lines.length);
  for (const index of changed) {
    const start = Math.max(0, index - CONTEXT_LINES);
    const end = Math.min(lines.length - 1, index + CONTEXT_LINES);
    keep.fill(1, start, end + 1);
  }
  const compacted: ReviewLine[] = [];
  for (let index = 0; index < lines.length;) {
    if (keep[index]) {
      compacted.push(lines[index]);
      index += 1;
      continue;
    }
    const start = index;
    while (index < lines.length && !keep[index]) index += 1;
    compacted.push({ kind: "hunk", text: `… ${index - start} unchanged lines …` });
  }
  return compacted;
}

function splitLines(text: string): string[] {
  if (text === "") return [];
  const lines = text.split("\n");
  if (lines.at(-1) === "") lines.pop();
  return lines;
}
