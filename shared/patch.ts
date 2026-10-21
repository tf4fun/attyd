// JSON Pointer patch ops on a document (docs/protocol.md §5.3): the engine the
// replica applies a stream's `ops` with. Values are copied along the path an op
// changes and never modified, so a document held before an op stays what it was.

import type { Op } from "./protocol";

/** A patch op that does not apply (§5.3). */
export class PatchError extends Error {
  override name = "PatchError";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parsePointer(path: unknown): string[] {
  if (typeof path !== "string") throw new PatchError("a path must be a string");
  if (path === "") return [];
  if (!path.startsWith("/")) throw new PatchError(`path ${JSON.stringify(path)} does not start with "/"`);
  return path.slice(1).split("/").map((token) => token.replace(/~1/g, "/").replace(/~0/g, "~"));
}

/** The index `token` names in `array`, which must hold it. */
function indexOf(array: readonly unknown[], token: string): number {
  if (!/^(0|[1-9][0-9]*)$/.test(token) || Number(token) >= array.length) {
    throw new PatchError(`no index ${token} in an array of ${array.length}`);
  }
  return Number(token);
}

function child(node: unknown, token: string): unknown {
  if (Array.isArray(node)) return node[indexOf(node, token)];
  if (isRecord(node) && Object.hasOwn(node, token)) return node[token];
  throw new PatchError(`no member ${JSON.stringify(token)}`);
}

/** A copy of `node` with `token` set to `value`; `node` is not modified. */
function withChild(node: unknown, token: string, value: unknown): unknown {
  if (Array.isArray(node)) {
    const copy = node.slice();
    copy[indexOf(node, token)] = value;
    return copy;
  }
  if (isRecord(node)) return { ...node, [token]: value };
  throw new PatchError(`no member ${JSON.stringify(token)} in a ${typeof node}`);
}

function withoutChild(node: unknown, token: string): unknown {
  if (Array.isArray(node)) {
    const copy = node.slice();
    copy.splice(indexOf(node, token), 1);
    return copy;
  }
  if (isRecord(node) && Object.hasOwn(node, token)) {
    const copy = { ...node };
    delete copy[token];
    return copy;
  }
  throw new PatchError(`no member ${JSON.stringify(token)} to remove`);
}

/** The target of an `append` or `trim`, changed. */
function changed(target: unknown, op: Op): unknown {
  if (op.op === "append") {
    if ("values" in op) {
      if (!Array.isArray(target) || !Array.isArray(op.values)) throw new PatchError("append of values needs an array");
      return [...target, ...op.values];
    }
    if (typeof target !== "string" || typeof op.text !== "string") throw new PatchError("append of text needs a string");
    return target + op.text;
  }
  if (op.op === "trim") {
    const { count } = op;
    if (typeof target !== "string" && !Array.isArray(target)) throw new PatchError("trim needs a string or an array");
    if (!Number.isInteger(count) || count < 0 || count > target.length) {
      throw new PatchError(`cannot trim ${count} of ${target.length}`);
    }
    return target.slice(count);
  }
  throw new PatchError(`unknown op ${JSON.stringify((op as { op: unknown }).op)}`);
}

function applyAt(node: unknown, tokens: readonly string[], op: Op): unknown {
  const [token, ...rest] = tokens;
  if (rest.length > 0) return withChild(node, token, applyAt(child(node, token), rest, op));
  switch (op.op) {
    case "set":
      return withChild(node, token, op.value);
    case "remove":
      return withoutChild(node, token);
    default:
      return withChild(node, token, changed(child(node, token), op));
  }
}

function applyOp(live: unknown, op: Op): unknown {
  if (typeof op !== "object" || op === null) throw new PatchError("an op must be an object");
  const tokens = parsePointer(op.path);
  if (tokens.length > 0) return applyAt(live, tokens, op);
  switch (op.op) {
    case "set":
      return op.value;
    case "remove":
      throw new PatchError("cannot remove the root");
    default:
      return changed(live, op);
  }
}

/**
 * Applies `ops` to `live` in order and returns the new value; `live` is not
 * modified. Throws `PatchError` when an op does not apply.
 */
export function applyOps(live: unknown, ops: readonly Op[]): unknown {
  return ops.reduce(applyOp, live);
}
