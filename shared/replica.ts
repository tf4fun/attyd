// A generic replica of the protocol v2 stream (docs/protocol.md §4, §4.1).
//
// It holds one document per subscribed scope and applies `snapshot`, `reset`,
// `ops` and `end` entries with a JSON Pointer patcher. It knows nothing about
// ACP or React: any JavaScript client can feed it SSE messages and fetched
// snapshots, and read documents, positions and intents back.

import type {
  EndReason,
  Entry,
  GlobalState,
  Op,
  Position,
  Request,
  SessionState,
  Snapshot,
  TurnOutcome,
} from "./protocol.js";
import { validateScopeState } from "./protocol-validate.js";

export class ReplicaError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ReplicaError";
  }
}

export const GLOBAL_SCOPE = "global";

/** The owner-qualified subscription of a session scope (protocol §4.2). */
export function sessionSubscription(sessionId: string, epoch: string, incarnation: number): string {
  return `session/${sessionId}@${epoch}:${incarnation}`;
}

/** The scope a subscription's entries carry: `global` or `session/<id>`. */
export function entryScope(subscription: string): string {
  if (subscription === GLOBAL_SCOPE) return subscription;
  const at = subscription.lastIndexOf("@");
  return at < 0 ? subscription : subscription.slice(0, at);
}

/** `<epoch>:<seq>` of an SSE id or `Last-Event-ID`. */
export function parseEventId(id: string): Position | undefined {
  const colon = id.lastIndexOf(":");
  if (colon <= 0) return undefined;
  const seq = Number(id.slice(colon + 1));
  return Number.isSafeInteger(seq) && seq >= 0 && id.slice(colon + 1) !== ""
    ? { epoch: id.slice(0, colon), seq }
    : undefined;
}

// ---------------------------------------------------------------------------
// JSON Pointer ops

type Json = unknown;

function parsePointer(path: string): string[] {
  if (path === "") return [];
  if (!path.startsWith("/")) throw new ReplicaError(`${path}: a pointer starts with /`);
  return path.slice(1).split("/").map((token) => {
    if (/~[^01]|~$/u.test(token)) throw new ReplicaError(`${path}: invalid escape`);
    return token.replaceAll("~1", "/").replaceAll("~0", "~");
  });
}

function arrayIndex(token: string, length: number, path: string): number {
  if (!/^(0|[1-9][0-9]*)$/u.test(token)) throw new ReplicaError(`${path}: ${token} is not an array index`);
  const index = Number(token);
  if (index >= length) throw new ReplicaError(`${path}: array index out of range`);
  return index;
}

function isContainer(value: Json): value is Record<string, Json> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function child(container: Json, token: string, path: string): Json {
  if (Array.isArray(container)) return container[arrayIndex(token, container.length, path)];
  if (isContainer(container) && Object.hasOwn(container, token)) return container[token];
  throw new ReplicaError(`${path}: missing ${token}`);
}

function replaced(container: Json, token: string, value: Json, path: string): Json {
  if (Array.isArray(container)) {
    const copy = container.slice();
    copy[arrayIndex(token, container.length, path)] = value;
    return copy;
  }
  if (isContainer(container)) return { ...container, [token]: value };
  throw new ReplicaError(`${path}: parent is not a container`);
}

/** Rebuilds the path to `tokens`, replacing the value there with `change(old)`. */
function update(document: Json, tokens: string[], path: string, change: (value: Json) => Json): Json {
  if (tokens.length === 0) return change(document);
  const [head, ...rest] = tokens;
  return replaced(document, head, update(child(document, head, path), rest, path, change), path);
}

function trimText(text: string, count: number, path: string): string {
  if (count > text.length) throw new ReplicaError(`${path}: trim exceeds the text`);
  const code = text.charCodeAt(count);
  if (count > 0 && code >= 0xdc00 && code <= 0xdfff) {
    const previous = text.charCodeAt(count - 1);
    if (previous >= 0xd800 && previous <= 0xdbff) throw new ReplicaError(`${path}: trim splits a character`);
  }
  return text.slice(count);
}

/** Applies one op without changing `document`; untouched subtrees are shared. */
export function applyOp(document: Json, op: Op): Json {
  const tokens = parsePointer(op.path);
  const { path } = op;
  switch (op.op) {
    case "set": {
      if (tokens.length === 0) return op.value;
      const leaf = tokens[tokens.length - 1];
      return update(document, tokens.slice(0, -1), path, (parent) => {
        if (Array.isArray(parent)) return replaced(parent, leaf, op.value, path);
        if (isContainer(parent)) return { ...parent, [leaf]: op.value };
        throw new ReplicaError(`${path}: parent is not a container`);
      });
    }
    case "remove": {
      if (tokens.length === 0) throw new ReplicaError("cannot remove the root");
      const leaf = tokens[tokens.length - 1];
      return update(document, tokens.slice(0, -1), path, (parent) => {
        if (Array.isArray(parent)) {
          const copy = parent.slice();
          copy.splice(arrayIndex(leaf, parent.length, path), 1);
          return copy;
        }
        if (isContainer(parent) && Object.hasOwn(parent, leaf)) {
          const { [leaf]: _removed, ...rest } = parent;
          return rest;
        }
        throw new ReplicaError(`${path}: missing ${leaf}`);
      });
    }
    case "append":
      return update(document, tokens, path, (target) => {
        if ("values" in op && Array.isArray(target)) return [...target, ...op.values];
        if ("text" in op && typeof target === "string") return target + op.text;
        throw new ReplicaError(`${path}: target type does not match the append`);
      });
    case "trim":
      return update(document, tokens, path, (target) => {
        if (!Number.isSafeInteger(op.count) || op.count < 0) throw new ReplicaError(`${path}: invalid count`);
        if (Array.isArray(target)) {
          if (op.count > target.length) throw new ReplicaError(`${path}: trim exceeds the array`);
          return target.slice(op.count);
        }
        if (typeof target === "string") return trimText(target, op.count, path);
        throw new ReplicaError(`${path}: target type does not match the trim`);
      });
    default:
      throw new ReplicaError(`unknown op ${(op as { op: unknown }).op}`);
  }
}

export function applyOps(document: Json, ops: readonly Op[]): Json {
  return ops.reduce(applyOp, document);
}

// ---------------------------------------------------------------------------
// Scope replicas

interface ScopeReplica {
  subscription: string;
  document: Json | undefined;
  /** The last `seq` applied: a snapshot's `at.seq` or the last `ops`. */
  installed: number;
  /** `installed` is a snapshot position, so any earlier `prev` continues. */
  fromSnapshot: boolean;
  /** Since the subscription was made, a snapshot was installed. */
  synced: boolean;
  /** Waiting for a snapshot at or after this position. */
  resetAt: number | undefined;
  buffer: Array<{ seq: number; prev: number; ops: Op[] }>;
  ended: EndReason | undefined;
}

function freshScope(subscription: string): ScopeReplica {
  return {
    subscription, document: undefined, installed: 0, fromSnapshot: false, synced: false,
    resetAt: undefined, buffer: [], ended: undefined,
  };
}

function pending(replica: ScopeReplica): boolean {
  return replica.ended == null && (!replica.synced || replica.resetAt != null);
}

export type IntentMatch =
  | { kind: "turn"; operationId: string }
  | { kind: "outcome"; outcome: TurnOutcome }
  | { kind: "request"; request: Request };

export interface ReplicaOptions {
  /** Validates a document; `touched` names the top-level fields ops changed. */
  validate?: (scope: string, state: unknown, touched?: Iterable<string>) => void;
}

interface Waiter {
  at: Position;
  resolve: () => void;
  reject: (error: Error) => void;
}

export class Replica {
  /** The epoch of the entries read, once any arrived. */
  epoch: string | undefined;
  /** The `at.seq` of the last entry read (protocol §4.1). */
  received = 0;
  /** The `at.seq` of the last entry that carried an SSE id. */
  checkpoint = 0;
  private scopes = new Map<string, ScopeReplica>();
  private freshStream = true;
  private streamEnded = new Set<string>();
  private listeners = new Set<(scopes: ReadonlySet<string>) => void>();
  private waiters: Waiter[] = [];
  private readonly validate: NonNullable<ReplicaOptions["validate"]>;

  constructor(options: ReplicaOptions = {}) {
    this.validate = options.validate ?? validateScopeState;
  }

  /**
   * Starts a new stream for `subscriptions` (owner-qualified). Every scope
   * starts pending. A scope that stays subscribed keeps its document for
   * display until its new first entry arrives.
   */
  subscribe(subscriptions: readonly string[]): void {
    const next = new Map<string, ScopeReplica>();
    for (const subscription of subscriptions) {
      const scope = entryScope(subscription);
      const previous = this.scopes.get(scope);
      next.set(scope, previous != null && previous.subscription === subscription && previous.ended == null
        ? { ...previous, synced: false, resetAt: undefined, buffer: [] }
        : freshScope(subscription));
    }
    this.scopes = next;
    this.freshStream = true;
    this.streamEnded.clear();
    this.received = 0;
    this.checkpoint = 0;
  }

  /** The same stream reconnected, resuming after `lastEventId`. */
  reconnect(lastEventId?: string): void {
    this.freshStream = true;
    this.streamEnded.clear();
    const position = lastEventId == null ? undefined : parseEventId(lastEventId);
    this.received = position != null && position.epoch === this.epoch ? position.seq : 0;
    this.checkpoint = this.received;
  }

  subscriptions(): string[] {
    return [...this.scopes.values()].map(({ subscription }) => subscription);
  }

  document<T = GlobalState | SessionState>(scope: string): T | undefined {
    const replica = this.scopes.get(scope);
    return replica?.ended == null ? replica?.document as T | undefined : undefined;
  }

  ended(scope: string): EndReason | undefined {
    return this.scopes.get(scope)?.ended;
  }

  pending(scope: string): boolean {
    const replica = this.scopes.get(scope);
    return replica != null && pending(replica);
  }

  /** The `seq` installed for `scope`, 0 while nothing is. */
  installed(scope: string): number {
    return this.scopes.get(scope)?.installed ?? 0;
  }

  /** `min(checkpoint, installed of every pending scope)` (protocol §4.1). */
  applied(): number {
    let applied = this.checkpoint;
    for (const replica of this.scopes.values()) {
      if (pending(replica)) applied = Math.min(applied, replica.installed);
    }
    return applied;
  }

  /** Subscriptions waiting for a fetched snapshot. */
  fetches(): string[] {
    return [...this.scopes.values()]
      .filter((replica) => replica.resetAt != null && replica.ended == null)
      .map(({ subscription }) => subscription);
  }

  onChange(listener: (scopes: ReadonlySet<string>) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /**
   * Read your writes (protocol §5.1): resolves once `applied` reaches `at`.
   * Rejects when the replica moves to another epoch.
   */
  whenApplied(at: Position): Promise<void> {
    if (this.epoch != null && at.epoch !== this.epoch) {
      return Promise.reject(new ReplicaError(`position from epoch ${at.epoch} in epoch ${this.epoch}`));
    }
    if (at.epoch === this.epoch && this.applied() >= at.seq) return Promise.resolve();
    return new Promise((resolve, reject) => this.waiters.push({ at, resolve, reject }));
  }

  /** Recovery after a lost response (protocol §5.2): turn, outcomes, requests. */
  findIntent(clientIntentId: string, sessionScope?: string): IntentMatch | undefined {
    const session = sessionScope == null ? undefined : this.document<SessionState>(sessionScope);
    if (session?.turn?.clientIntentId === clientIntentId) {
      return { kind: "turn", operationId: session.turn.operationId };
    }
    const outcome = Object.values(session?.outcomes ?? {}).find((candidate) => candidate.clientIntentId === clientIntentId);
    if (outcome != null) return { kind: "outcome", outcome };
    const request = Object.values(this.document<GlobalState>(GLOBAL_SCOPE)?.requests ?? {})
      .find((candidate) => candidate.clientIntentId === clientIntentId);
    return request == null ? undefined : { kind: "request", request };
  }

  /** Reads one SSE message: its `data` and its `id` ("" when it has none). */
  receive(data: string, id: string): void {
    let entry: Entry;
    try {
      entry = JSON.parse(data) as Entry;
    } catch {
      throw new ReplicaError("an entry is not JSON");
    }
    if (typeof entry !== "object" || entry === null || !isPosition(entry.at)) {
      throw new ReplicaError("an entry needs a position");
    }
    const { at } = entry;
    if (id !== "" && id !== `${at.epoch}:${at.seq}`) throw new ReplicaError(`SSE id ${id} for an entry at ${at.epoch}:${at.seq}`);
    const changed = new Set<string>();
    this.position(at, changed);
    if (id !== "") this.checkpoint = at.seq;
    if (entry.type !== "position") {
      const replica = this.scopes.get(entry.scope);
      // Entries of scopes this stream does not carry are not ours to apply.
      if (replica != null) this.entry(entry, replica, changed);
    }
    this.settle(changed);
  }

  /** Installs a fetched snapshot. Returns false when it is not usable. */
  install(subscription: string, snapshot: Snapshot<unknown>): boolean {
    const scope = entryScope(subscription);
    const replica = this.scopes.get(scope);
    if (replica == null || replica.subscription !== subscription || replica.ended != null) return false;
    if (!isPosition(snapshot.at) || (this.epoch != null && snapshot.at.epoch !== this.epoch)) return false;
    if (!ownsState(subscription, snapshot.state)) return false;
    const changed = new Set<string>();
    const installed = this.installSnapshot(scope, replica, snapshot.at.seq, snapshot.state, changed);
    this.settle(changed);
    return installed;
  }

  private position(at: Position, changed: Set<string>): void {
    if (this.epoch !== at.epoch) {
      if (this.epoch != null && !this.freshStream) {
        throw new ReplicaError(`entry from epoch ${at.epoch} in epoch ${this.epoch}`);
      }
      if (this.epoch != null) {
        // Nothing of the old epoch stays (protocol §2).
        for (const [scope, replica] of this.scopes) {
          if (replica.document != null || replica.ended != null) changed.add(scope);
          this.scopes.set(scope, freshScope(replica.subscription));
        }
        const error = new ReplicaError(`epoch ${this.epoch} ended`);
        for (const waiter of this.waiters.splice(0)) waiter.reject(error);
        this.received = 0;
        this.checkpoint = 0;
      }
      this.epoch = at.epoch;
    }
    this.freshStream = false;
    if (at.seq < this.received) throw new ReplicaError(`entry at ${at.seq} after ${this.received}`);
    this.received = at.seq;
  }

  private entry(entry: Exclude<Entry, { type: "position" }>, replica: ScopeReplica, changed: Set<string>): void {
    const { scope, at } = entry;
    switch (entry.type) {
      case "snapshot":
        if (replica.ended != null) throw new ReplicaError(`${scope}: snapshot after end`);
        this.installSnapshot(scope, replica, at.seq, entry.state, changed);
        return;
      case "reset":
        if (replica.ended != null) throw new ReplicaError(`${scope}: reset after end`);
        if (replica.synced && replica.resetAt == null && at.seq <= replica.installed) return;
        if (replica.resetAt != null && replica.resetAt >= at.seq) return;
        replica.resetAt = at.seq;
        replica.buffer = [];
        return;
      case "ops":
        if (replica.ended != null) throw new ReplicaError(`${scope}: ops after end`);
        if (pending(replica)) {
          if (replica.resetAt == null) throw new ReplicaError(`${scope}: ops before the scope's first entry`);
          replica.buffer.push({ seq: at.seq, prev: entry.prev, ops: entry.ops });
          return;
        }
        if (!this.apply(scope, replica, at.seq, entry.prev, entry.ops)) this.reset(replica, at.seq);
        else changed.add(scope);
        return;
      case "end":
        if (this.streamEnded.has(scope)) throw new ReplicaError(`${scope}: a second end`);
        this.streamEnded.add(scope);
        if (replica.document != null || replica.ended == null) changed.add(scope);
        this.scopes.set(scope, { ...freshScope(replica.subscription), ended: entry.reason });
        return;
      default:
        throw new ReplicaError(`unknown entry type ${(entry as { type: unknown }).type}`);
    }
  }

  private installSnapshot(scope: string, replica: ScopeReplica, seq: number, state: unknown, changed: Set<string>): boolean {
    if (replica.synced && replica.resetAt == null && seq <= replica.installed) return true;
    if (replica.resetAt != null && seq < replica.resetAt) return false;
    try {
      this.validate(scope, state);
    } catch {
      this.reset(replica, seq);
      return false;
    }
    replica.document = state;
    replica.installed = seq;
    replica.fromSnapshot = true;
    replica.synced = true;
    replica.resetAt = undefined;
    changed.add(scope);
    for (const buffered of replica.buffer.splice(0)) {
      if (!this.apply(scope, replica, buffered.seq, buffered.prev, buffered.ops)) {
        this.reset(replica, buffered.seq);
        return false;
      }
    }
    return true;
  }

  /** Applies one `ops` entry; false when it breaks continuity or does not apply. */
  private apply(scope: string, replica: ScopeReplica, seq: number, prev: number, ops: Op[]): boolean {
    // Covered by the installed snapshot, or replayed after a resume.
    if (seq <= replica.installed) return true;
    if (prev !== replica.installed && !(replica.fromSnapshot && prev <= replica.installed)) return false;
    let document: Json;
    try {
      document = applyOps(replica.document, ops);
      this.validate(scope, document, touchedFields(ops));
    } catch {
      return false;
    }
    replica.document = document;
    replica.installed = seq;
    replica.fromSnapshot = false;
    return true;
  }

  /** The scope waits for a snapshot at or after `seq`; its document stays visible. */
  private reset(replica: ScopeReplica, seq: number): void {
    replica.resetAt = Math.max(seq, replica.resetAt ?? 0);
    replica.buffer = [];
  }

  private settle(changed: Set<string>): void {
    if (this.waiters.length > 0 && this.epoch != null) {
      const applied = this.applied();
      this.waiters = this.waiters.filter((waiter) => {
        if (waiter.at.epoch !== this.epoch) {
          waiter.reject(new ReplicaError(`epoch ${waiter.at.epoch} ended`));
          return false;
        }
        if (waiter.at.seq > applied) return true;
        waiter.resolve();
        return false;
      });
    }
    if (changed.size === 0) return;
    for (const listener of [...this.listeners]) listener(changed);
  }
}

function isPosition(value: unknown): value is Position {
  return typeof value === "object" && value !== null &&
    typeof (value as Position).epoch === "string" && Number.isSafeInteger((value as Position).seq);
}

function touchedFields(ops: readonly Op[]): string[] | undefined {
  const fields = new Set<string>();
  for (const { path } of ops) {
    if (path === "") return undefined;
    fields.add(parsePointer(path)[0]);
  }
  return [...fields];
}

/** A fetched session snapshot belongs to the incarnation its subscription names. */
function ownsState(subscription: string, state: unknown): boolean {
  if (subscription === GLOBAL_SCOPE) return true;
  const owner = subscription.slice(subscription.lastIndexOf("@") + 1);
  const incarnation = Number(owner.slice(owner.lastIndexOf(":") + 1));
  return typeof state === "object" && state !== null && (state as SessionState).incarnation === incarnation;
}
