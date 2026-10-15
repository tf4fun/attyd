// The generic replica engine of protocol v2 (docs/protocol.md §5, migration
// step 7). A replica holds one scope's document, `log` and `live`, as the
// bridge publishes it, and applies the stream's events to it one at a time.
// It holds no authoritative state (C1) and runs outside React.

import type { End, Event, LogEntry, Op } from "./protocol";

/** `h_0` on the wire: 16 zero bytes, unpadded base64url (§3.3). */
export const H0 = "AAAAAAAAAAAAAAAAAAAAAA";

/** One scope's document as this client holds it. */
export interface Replica {
  readonly scope: string;
  readonly log: readonly LogEntry[];
  /** Whether the bridge has confirmed `log`; a held log stays until it does (S3). */
  readonly confirmed: boolean;
  /** `undefined` until the scope's first alignment. */
  readonly live: unknown;
}

/** The head of a log, `(n, h_n)` (S3). */
export interface Head {
  readonly n: number;
  readonly hash: string;
}

export type Applied =
  | { readonly kind: "applied"; readonly replica: Replica }
  | { readonly kind: "ended"; readonly end: End }
  | { readonly kind: "resubscribe"; readonly reason: string };

/** A patch op that does not apply (§5.3). */
export class PatchError extends Error {
  override name = "PatchError";
}

/** A replica of `scope` that holds `log`, unconfirmed, before any alignment. */
export function createReplica(scope: string, log: readonly LogEntry[] = []): Replica {
  return { scope, log, confirmed: false, live: undefined };
}

/** The head of the log `replica` holds. */
export function headOf(replica: Replica): Head {
  const n = replica.log.length;
  return { n, hash: n === 0 ? H0 : replica.log[n - 1].hash };
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

/**
 * Applies one event of `replica.scope` (§5.2). Both parts of an event are
 * applied together: when either does not apply, nothing changes and the
 * client subscribes again with the head it holds.
 */
export function applyEvent(replica: Replica, event: Event): Applied {
  if ("end" in event) return { kind: "ended", end: event.end };
  let { log, confirmed, live } = replica;

  if ("log" in event) {
    const { from, prev, entries } = event.log;
    if (!Number.isInteger(from) || from < 0 || from > log.length) {
      return { kind: "resubscribe", reason: `log from ${from} past a log of ${log.length}` };
    }
    if ((from === 0 ? H0 : log[from - 1].hash) !== prev) {
      return { kind: "resubscribe", reason: `log prev at ${from} is not the hash held` };
    }
    if (from < log.length || entries.length > 0) log = [...log.slice(0, from), ...entries];
    confirmed = true;
  }

  const change = event.live;
  if (change !== undefined) {
    if ("replace" in change) {
      live = change.replace;
      // An alignment of live alone: the bridge does not know the log (S1, S3).
      if (!("log" in event)) confirmed = false;
    } else {
      if (live === undefined) return { kind: "resubscribe", reason: "a live patch before any alignment" };
      try {
        live = applyOps(live, change.patch);
      } catch (error) {
        if (error instanceof PatchError) return { kind: "resubscribe", reason: error.message };
        throw error;
      }
    }
  }

  return { kind: "applied", replica: { scope: replica.scope, log, confirmed, live } };
}

/** A scope to follow, and the working directory to load an unheld session with. */
export interface Follow {
  readonly scope: string;
  readonly cwd?: string;
}

export type Received = "applied" | "ended" | "ignored" | "resubscribe";

/**
 * The replicas of the scopes one client follows, and the stream that feeds
 * them. A new stream replaces the previous one; events of a replaced stream
 * change nothing.
 */
export class ReplicaSet {
  readonly #replicas = new Map<string, Replica>();
  #stream = 0;
  /** The current stream; `undefined` once it may no longer be applied. */
  #current: number | undefined;
  /** The scopes the current stream follows and has not ended. */
  #following = new Set<string>();

  /** Opens a new stream for `follow`; returns its ID and its URL, built from the heads held now. */
  open(follow: readonly Follow[]): { readonly stream: number; readonly url: string } {
    const params = new URLSearchParams();
    for (const { scope } of follow) params.append("scope", scope);
    for (const { scope } of follow) {
      const replica = this.#replicas.get(scope);
      if (!scope.startsWith("session/") || replica === undefined) continue;
      const head = headOf(replica);
      if (head.n > 0) params.append("have", `${scope}:${head.n}:${head.hash}`);
    }
    for (const { scope, cwd } of follow) {
      if (cwd !== undefined) params.append("cwd", `${scope}:${cwd}`);
    }
    this.#stream += 1;
    this.#current = this.#stream;
    this.#following = new Set(follow.map(({ scope }) => scope));
    return { stream: this.#stream, url: `/api/v2/stream?${params}` };
  }

  /** Applies one event received on `stream`. */
  receive(stream: number, event: Event): Received {
    if (stream !== this.#current || !this.#following.has(event.scope)) return "ignored";
    const held = this.#replicas.get(event.scope) ?? createReplica(event.scope);
    const result = applyEvent(held, event);
    switch (result.kind) {
      case "applied":
        this.#replicas.set(event.scope, result.replica);
        return "applied";
      case "ended":
        this.#replicas.delete(event.scope);
        this.#following.delete(event.scope);
        return "ended";
      case "resubscribe":
        // The stream is closed; only a new one changes the replicas.
        this.#current = undefined;
        return "resubscribe";
    }
  }

  /** The replica of `scope`, if one is held. */
  get(scope: string): Replica | undefined {
    return this.#replicas.get(scope);
  }
}
