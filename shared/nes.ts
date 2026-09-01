import type {
  NesTextEdit,
  Position,
  Range,
  TextDocumentContentChangeEvent,
} from "@agentclientprotocol/sdk";

export function positionAt(text: string, offset: number): Position {
  if (!Number.isSafeInteger(offset) || offset < 0 || offset > text.length) {
    throw new Error(`Text offset is outside the document: ${offset}`);
  }
  let line = 0;
  let lineStart = 0;
  for (let index = 0; index < offset; index += 1) {
    if (text.charCodeAt(index) === 10) {
      line += 1;
      lineStart = index + 1;
    }
  }
  return { line, character: offset - lineStart };
}

export function offsetAt(text: string, position: Position): number {
  assertPositionShape(position);
  let line = 0;
  let lineStart = 0;
  while (line < position.line) {
    const next = text.indexOf("\n", lineStart);
    if (next < 0) throw new Error("Position line is outside the document");
    line += 1;
    lineStart = next + 1;
  }
  const lineEnd = text.indexOf("\n", lineStart);
  const contentEnd = lineEnd < 0 ? text.length : lineEnd;
  const offset = lineStart + position.character;
  if (offset > contentEnd) throw new Error("Position character is outside the line");
  return offset;
}

export function assertRange(text: string, range: Range): void {
  const start = offsetAt(text, range.start);
  const end = offsetAt(text, range.end);
  if (start > end) throw new Error("Range start must not follow its end");
}

export function fullOrIncrementalChange(
  previous: string,
  next: string,
  syncKind: "full" | "incremental",
): TextDocumentContentChangeEvent {
  if (syncKind === "full") return { text: next };

  let prefix = 0;
  const commonLength = Math.min(previous.length, next.length);
  while (prefix < commonLength && previous[prefix] === next[prefix]) prefix += 1;
  prefix = safeUtf16Boundary(previous, next, prefix);

  let suffix = 0;
  while (
    suffix < previous.length - prefix &&
    suffix < next.length - prefix &&
    previous[previous.length - 1 - suffix] === next[next.length - 1 - suffix]
  ) {
    suffix += 1;
  }
  const previousEnd = safeTrailingUtf16Boundary(previous, previous.length - suffix, prefix);
  const nextEnd = safeTrailingUtf16Boundary(next, next.length - suffix, prefix);

  return {
    range: {
      start: positionAt(previous, prefix),
      end: positionAt(previous, previousEnd),
    },
    text: next.slice(prefix, nextEnd),
  };
}

export function applyTextEdits(text: string, edits: NesTextEdit[]): string {
  const resolved = edits.map((edit) => {
    const start = offsetAt(text, edit.range.start);
    const end = offsetAt(text, edit.range.end);
    if (start > end) throw new Error("Edit range start must not follow its end");
    return { start, end, newText: edit.newText };
  }).sort((left, right) => right.start - left.start || right.end - left.end);

  let previousStart = text.length;
  let result = text;
  for (const edit of resolved) {
    if (edit.end > previousStart) throw new Error("NES edits must not overlap");
    result = result.slice(0, edit.start) + edit.newText + result.slice(edit.end);
    previousStart = edit.start;
  }
  return result;
}

export function unifiedTextDiff(
  uri: string,
  previous: string,
  next: string,
  contextLines = 3,
): string {
  if (!Number.isSafeInteger(contextLines) || contextLines < 0) {
    throw new Error("Diff context lines must be a non-negative integer");
  }
  if (previous === next) return "";

  const previousLines = splitDiffLines(previous);
  const nextLines = splitDiffLines(next);
  const commonLength = Math.min(previousLines.length, nextLines.length);
  let prefix = 0;
  while (prefix < commonLength && previousLines[prefix] === nextLines[prefix]) {
    prefix += 1;
  }

  let suffix = 0;
  while (
    suffix < previousLines.length - prefix &&
    suffix < nextLines.length - prefix &&
    previousLines[previousLines.length - 1 - suffix] ===
      nextLines[nextLines.length - 1 - suffix]
  ) {
    suffix += 1;
  }

  const leadingContext = Math.min(prefix, contextLines);
  const trailingContext = Math.min(suffix, contextLines);
  const previousChangeEnd = previousLines.length - suffix;
  const nextChangeEnd = nextLines.length - suffix;
  const previousStart = prefix - leadingContext;
  const nextStart = prefix - leadingContext;
  const previousEnd = previousChangeEnd + trailingContext;
  const nextEnd = nextChangeEnd + trailingContext;
  const previousCount = previousEnd - previousStart;
  const nextCount = nextEnd - nextStart;

  const body = [
    ...previousLines.slice(previousStart, prefix).map((line) => ` ${line}`),
    ...previousLines.slice(prefix, previousChangeEnd).map((line) => `-${line}`),
    ...nextLines.slice(prefix, nextChangeEnd).map((line) => `+${line}`),
    ...previousLines.slice(previousChangeEnd, previousEnd).map((line) => ` ${line}`),
  ];

  return [
    `--- ${uri}`,
    `+++ ${uri}`,
    `@@ -${formatDiffRange(previousStart, previousCount)} +${formatDiffRange(nextStart, nextCount)} @@`,
    ...body,
  ].join("\n");
}

function assertPositionShape(position: Position): void {
  if (
    !Number.isSafeInteger(position.line) ||
    !Number.isSafeInteger(position.character) ||
    position.line < 0 ||
    position.character < 0
  ) {
    throw new Error("Position must contain non-negative integer line and character values");
  }
}

function safeUtf16Boundary(left: string, right: string, offset: number): number {
  if (
    offset > 0 &&
    (splitsSurrogate(left, offset) || splitsSurrogate(right, offset))
  ) {
    return offset - 1;
  }
  return offset;
}

function safeTrailingUtf16Boundary(text: string, offset: number, minimum: number): number {
  return offset > minimum && splitsSurrogate(text, offset) ? offset + 1 : offset;
}

function splitsSurrogate(text: string, offset: number): boolean {
  const previous = text.charCodeAt(offset - 1);
  const next = text.charCodeAt(offset);
  return previous >= 0xd800 && previous <= 0xdbff && next >= 0xdc00 && next <= 0xdfff;
}

function splitDiffLines(text: string): string[] {
  if (text.length === 0) return [];
  const lines = text.split("\n");
  if (lines.at(-1) === "") lines.pop();
  return lines;
}

function formatDiffRange(zeroBasedStart: number, count: number): string {
  const oneBasedStart = count === 0 ? zeroBasedStart : zeroBasedStart + 1;
  return `${oneBasedStart},${count}`;
}
