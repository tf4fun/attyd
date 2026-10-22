// The HTTP and stream client of protocol v2 (docs/protocol.md §5–§7,
// migration step 7). The stream feeds the replicas; writes and resource
// actions go over HTTP and never change a replica: the stream alone does
// (C1). A write sent without a definite answer is never resent
// automatically (C3); queued input that was never sent, or was refused with
// a definite answer, is submitted once the replica shows the operation
// ended (C2).

import type { Answer, ErrorCode, Event, OpArgs, OpOutcome, Operation, ProtocolError } from "./protocol";
import { ReplicaSet, type Follow, type Received, type Replica } from "./replica";

// ---------------------------------------------------------------------------
// Transport

/** The part of `fetch` the client uses; a rejection is a write without a definite answer. */
export type HttpFetch = (
  url: string,
  init: { method: "POST"; headers: Record<string, string>; body: string },
) => Promise<{ status: number; json(): Promise<unknown> }>;

/** One SSE connection, as `EventSource` offers it. */
export interface StreamSource {
  onmessage: ((message: { data: string }) => void) | null;
  onerror: (() => void) | null;
  close(): void;
}

export type OpenStream = (url: string) => StreamSource;

/** Runs `reconnect` later; the delay is the scheduler's. */
export type Schedule = (reconnect: () => void) => void;

// ---------------------------------------------------------------------------
// Stream (§5)

/**
 * Follows a set of scopes on one stream at a time. A new set closes the
 * stream it replaces; a broken stream or an event that does not apply closes
 * it too, and a new one subscribes with the heads held then. It reads
 * nothing over HTTP.
 */
export class ReplicaClient {
  readonly #set = new ReplicaSet();
  readonly #listeners = new Set<(scope: string) => void>();
  readonly #openStream: OpenStream;
  readonly #schedule: Schedule;
  #follow: readonly Follow[] = [];
  #source: StreamSource | undefined;
  /** Counts connections; a callback or reconnect of an earlier one changes nothing. */
  #generation = 0;
  #closed = false;

  constructor(openStream: OpenStream, schedule: Schedule = (reconnect) => void setTimeout(reconnect, 1000)) {
    this.#openStream = openStream;
    this.#schedule = schedule;
  }

  follow(follow: readonly Follow[]): void {
    if (this.#closed) return;
    this.#follow = follow;
    this.#connect();
  }

  get(scope: string): Replica | undefined {
    return this.#set.get(scope);
  }

  /** Calls `listener` with a scope each time its replica changes or ends. */
  subscribe(listener: (scope: string) => void): () => void {
    this.#listeners.add(listener);
    return () => {
      this.#listeners.delete(listener);
    };
  }

  close(): void {
    this.#closed = true;
    this.#disconnect();
  }

  #connect(): void {
    this.#disconnect();
    const generation = this.#generation;
    const { stream, url } = this.#set.open(this.#follow);
    const source = this.#openStream(url);
    this.#source = source;
    source.onmessage = ({ data }) => {
      if (generation === this.#generation) this.#receive(stream, data);
    };
    source.onerror = () => {
      if (generation === this.#generation) this.#broken();
    };
  }

  /** Closes the current stream, if any, and lets nothing of it through. */
  #disconnect(): void {
    this.#generation += 1;
    const source = this.#source;
    this.#source = undefined;
    if (source === undefined) return;
    source.onmessage = null;
    source.onerror = null;
    source.close();
  }

  #receive(stream: number, data: string): void {
    let scope: string;
    let received: Received;
    try {
      const event = JSON.parse(data) as Event;
      scope = event.scope;
      received = this.#set.receive(stream, event);
    } catch {
      // Not an event of the protocol: the stream cannot be trusted further.
      this.#broken();
      return;
    }
    if (received === "resubscribe") this.#broken();
    else if (received !== "ignored") this.#listeners.forEach((listener) => listener(scope));
  }

  /** The stream failed: close it, and subscribe again later with the heads held then (§5.4). */
  #broken(): void {
    this.#disconnect();
    const generation = this.#generation;
    this.#schedule(() => {
      if (!this.#closed && generation === this.#generation) this.#connect();
    });
  }
}

// ---------------------------------------------------------------------------
// Operation writes (§6)

export type WriteOutcome =
  /** `202`: admitted, nothing more; the replica shows what follows. */
  | { readonly kind: "accepted"; readonly op: string }
  /** `200` of `create`, `fork` and `list`, at the operation's end. */
  | { readonly kind: "ended"; readonly op: string; readonly outcome: OpOutcome }
  /** A definite refusal: the write changed nothing (§6.4). */
  | { readonly kind: "refused"; readonly error: ProtocolError }
  /** No definite answer: the write may have taken effect (C3). */
  | { readonly kind: "uncertain" }
  /** Not sent: the replica does not allow it as it stands. */
  | { readonly kind: "blocked"; readonly reason: string };

/** The replicas a writer reads `expect` and the pending interactions from. */
export interface ReplicaSource {
  get(scope: string): Replica | undefined;
}

/** What the queue reads of a replica: its `live`, whichever revision of the protocol holds it. */
export interface QueueSource {
  get(scope: string): { readonly live?: unknown } | undefined;
}

/**
 * Sends operation writes against the `op` the replica shows. It waits for
 * the HTTP answer only, with no short timeout, and never for the stream.
 */
export class OpsClient {
  readonly #fetch: HttpFetch;
  readonly #replicas: ReplicaSource;

  constructor(fetch: HttpFetch, replicas: ReplicaSource) {
    this.#fetch = fetch;
    this.#replicas = replicas;
  }

  /** Sends `write` with `expect` set to the replica's `op.id`. */
  write(scope: string, write: OpArgs): Promise<WriteOutcome> {
    const op = shownOp(this.#replicas, scope);
    if (op === undefined) return Promise.resolve(blocked(`no aligned replica of ${scope}`));
    return this.#send(scope, op.id, write);
  }

  /**
   * Answers the running operation's pending interactions, all of them at
   * once; it is blocked, unsent, unless `answers` covers exactly those the
   * replica shows.
   */
  answer(scope: string, answers: Readonly<Record<string, Answer>>): Promise<WriteOutcome> {
    const op = shownOp(this.#replicas, scope);
    if (op === undefined) return Promise.resolve(blocked(`no aligned replica of ${scope}`));
    if (op.status !== "running") return Promise.resolve(blocked(`${op.id} is not running`));
    const pending = Object.keys(op.interactions ?? {});
    const given = Object.keys(answers);
    if (given.length !== pending.length || !pending.every((id) => Object.hasOwn(answers, id))) {
      return Promise.resolve(blocked("the answers do not cover exactly the pending interactions"));
    }
    return this.#send(scope, op.id, { kind: "answer", args: { answers: { ...answers } } });
  }

  async #send(scope: string, expect: string, write: OpArgs): Promise<WriteOutcome> {
    let response: Awaited<ReturnType<HttpFetch>>;
    try {
      response = await this.#fetch(`/api/v2/scopes/${encodeURIComponent(scope)}/ops`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ expect, kind: write.kind, args: write.args }),
      });
    } catch {
      return { kind: "uncertain" };
    }
    const body = await response.json().catch(() => undefined);
    if (response.status === 202) {
      return isRecord(body) && typeof body.op === "string" ? { kind: "accepted", op: body.op } : { kind: "uncertain" };
    }
    if (response.status === 200) {
      if (!isRecord(body) || typeof body.op !== "string") return { kind: "uncertain" };
      if ("result" in body) return { kind: "ended", op: body.op, outcome: { result: body.result } };
      if (isProtocolError(body.error)) return { kind: "ended", op: body.op, outcome: { error: body.error } };
      return { kind: "uncertain" };
    }
    return refusal(response.status, body);
  }
}

/** The `op` of the aligned replica of `scope`. */
function shownOp(replicas: QueueSource, scope: string): Operation | undefined {
  const live = replicas.get(scope)?.live;
  if (!isRecord(live) || !isRecord(live.op) || typeof live.op.id !== "string") return undefined;
  return live.op as unknown as Operation;
}

function blocked(reason: string): WriteOutcome {
  return { kind: "blocked", reason };
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

const ERROR_CODES: ReadonlySet<string> = new Set<ErrorCode>([
  "invalid_request", "not_found", "stale", "not_allowed", "catalog_changed", "unsupported", "unavailable", "agent_error",
]);

export function isProtocolError(value: unknown): value is ProtocolError {
  return isRecord(value) && typeof value.code === "string" && ERROR_CODES.has(value.code) && typeof value.message === "string";
}

/**
 * A non-success answer: refused when its body is the protocol's error (§9),
 * otherwise, as from a proxy, without a definite answer.
 */
export function refusal(status: number, body: unknown): { kind: "refused"; error: ProtocolError } | { kind: "uncertain" } {
  if (status >= 200 && status < 300) return { kind: "uncertain" };
  return isRecord(body) && isProtocolError(body.error) ? { kind: "refused", error: body.error } : { kind: "uncertain" };
}

// ---------------------------------------------------------------------------
// Queued writes (C2, C3)

/**
 * What the queue sends writes through: the revision 2 `OpsClient`, or the revision 3 one,
 * whose outcomes have the same kinds the queue acts on.
 */
export interface QueueOps {
  write(scope: string, write: StartingWrite): Promise<WriteOutcome | QueueOutcome>;
}

/** The outcomes of a write as the queue reads them. */
export type QueueOutcome =
  | { readonly kind: "accepted" | "ended" | "uncertain" }
  | { readonly kind: "refused"; readonly error: ProtocolError }
  | { readonly kind: "blocked"; readonly reason: string };

/** A write that starts an operation; `cancel` and `answer` are sent at once, never queued. */
export type StartingWrite = Exclude<OpArgs, { kind: "cancel" | "answer" }>;

export type QueuedState = "queued" | "sending" | "refused" | "uncertain";

export interface QueuedWrite {
  readonly id: number;
  readonly write: StartingWrite;
  readonly state: QueuedState;
  /** The refusal of a `refused` write. */
  readonly error?: ProtocolError;
}

/**
 * The writes a user queued for a scope while its operation ran. One is sent
 * at a time, once the replica shows `op` ended, against that `op`; a write
 * admitted, or answered at its operation's end, leaves the queue, and the
 * next is sent as soon as its answer comes if the replica already shows a
 * later `op` ended. A refused write is sent again only after the replica shows a later
 * `op` ended; an uncertain one is never sent again unless the user
 * resubmits it.
 */
export class WriteQueue {
  readonly #ops: QueueOps;
  readonly #replicas: QueueSource;
  readonly #scopes = new Map<string, ScopeQueue>();
  #next = 0;

  constructor(ops: QueueOps, replicas: QueueSource) {
    this.#ops = ops;
    this.#replicas = replicas;
  }

  enqueue(scope: string, write: StartingWrite): number {
    this.#next += 1;
    const queue = this.#queue(scope);
    queue.items = [...queue.items, { id: this.#next, write, state: "queued" }];
    this.update(scope);
    return this.#next;
  }

  items(scope: string): readonly QueuedWrite[] {
    return this.#scopes.get(scope)?.items ?? [];
  }

  /** Called when the replica of `scope` changes; sends the next write if it may. */
  update(scope: string): void {
    const queue = this.#scopes.get(scope);
    if (queue === undefined || queue.sending !== undefined) return;
    const op = shownOp(this.#replicas, scope);
    // Once a write against an ended `op` was admitted, a second one against
    // it could only be refused as stale: wait for the replica to show another.
    if (op === undefined || op.status === "running" || op.id === queue.admitted) return;
    // Uncertain writes stay with the user and hold nothing back; the first
    // other write goes next, a refused one only against a later `op`.
    const index = queue.items.findIndex(({ state }) => state !== "uncertain");
    if (index < 0) return;
    const { id, write, state } = queue.items[index];
    if (state === "refused" && queue.refused.get(id) === op.id) return;
    queue.items = replaced(queue.items, index, { id, write, state: "sending" });
    queue.sending = { id, expect: op.id };
    void this.#ops.write(scope, write).then((outcome) => this.#answered(scope, id, outcome));
  }

  /** The user sends `id` again, as new input. */
  resubmit(scope: string, id: number): void {
    const queue = this.#scopes.get(scope);
    const item = queue?.items.find((candidate) => candidate.id === id);
    if (queue === undefined || item === undefined || item.state === "queued" || item.state === "sending") return;
    queue.items = [...queue.items.filter((candidate) => candidate.id !== id), { id, write: item.write, state: "queued" }];
    queue.refused.delete(id);
    // New input is sent against the `op` the replica shows, whichever it is.
    queue.admitted = undefined;
    this.update(scope);
  }

  /** The user takes `id` back. */
  remove(scope: string, id: number): void {
    const queue = this.#scopes.get(scope);
    if (queue === undefined || queue.sending?.id === id) return;
    queue.items = queue.items.filter((item) => item.id !== id);
    queue.refused.delete(id);
  }

  #queue(scope: string): ScopeQueue {
    let queue = this.#scopes.get(scope);
    if (queue === undefined) {
      queue = { items: [], refused: new Map() };
      this.#scopes.set(scope, queue);
    }
    return queue;
  }

  #answered(scope: string, id: number, outcome: WriteOutcome | QueueOutcome): void {
    const queue = this.#queue(scope);
    const expect = queue.sending?.expect;
    queue.sending = undefined;
    const index = queue.items.findIndex((item) => item.id === id);
    const { write } = queue.items[index];
    queue.refused.delete(id);
    switch (outcome.kind) {
      case "accepted":
      case "ended":
        queue.items = queue.items.filter((item) => item.id !== id);
        queue.admitted = expect;
        break;
      case "refused":
        // A refusal changed nothing (§6.4): the `op` stays free for other input.
        queue.items = replaced(queue.items, index, { id, write, state: "refused", error: outcome.error });
        if (expect !== undefined) queue.refused.set(id, expect);
        break;
      case "uncertain":
        // Only this input is uncertain: it is never sent again by itself, and
        // other input still competes for the `op`, the bridge's `expect` deciding.
        queue.items = replaced(queue.items, index, { id, write, state: "uncertain" });
        break;
      case "blocked":
        queue.items = replaced(queue.items, index, { id, write, state: "queued" });
        break;
    }
    // The stream may already show a later `op` ended.
    this.update(scope);
  }
}

interface ScopeQueue {
  items: readonly QueuedWrite[];
  /** The write waiting for its HTTP answer, and the `op.id` it was sent against. */
  sending?: { readonly id: number; readonly expect: string };
  /** The `op.id` the last write was admitted against. */
  admitted?: string;
  /** The `op.id` each refused write was refused against. */
  refused: Map<number, string>;
}

function replaced<T>(items: readonly T[], index: number, item: T): readonly T[] {
  return items.map((candidate, i) => (i === index ? item : candidate));
}

// ---------------------------------------------------------------------------
// Resource actions (§7)

export type ActionOutcome =
  | { readonly kind: "done" }
  | { readonly kind: "refused"; readonly error: ProtocolError }
  | { readonly kind: "uncertain" };

/**
 * Sends resource actions. A terminal's input is sent in order, one request
 * at a time per terminal, and input whose delivery is uncertain is never
 * resent (§7, C3).
 */
export class ResourceClient {
  readonly #fetch: HttpFetch;
  readonly #base: string;
  /** The last input sent to each terminal; the next waits for its answer. */
  readonly #inputs = new Map<string, Promise<ActionOutcome>>();

  /** `base` is the API prefix; the revision 3 routes are served beside the revision 2 ones until the cutover. */
  constructor(fetch: HttpFetch, base = "/api/v2") {
    this.#fetch = fetch;
    this.#base = base;
  }

  input(scope: string, terminalId: string, data: string): Promise<ActionOutcome> {
    const key = JSON.stringify([scope, terminalId]);
    const previous = this.#inputs.get(key);
    const send = () => this.#send(scope, terminalId, "input", { data });
    const sent = previous === undefined ? send() : previous.then(send);
    this.#inputs.set(key, sent);
    void sent.then(() => {
      if (this.#inputs.get(key) === sent) this.#inputs.delete(key);
    });
    return sent;
  }

  resize(scope: string, terminalId: string, cols: number, rows: number): Promise<ActionOutcome> {
    return this.#send(scope, terminalId, "resize", { cols, rows });
  }

  dismiss(scope: string, resourceId: string): Promise<ActionOutcome> {
    return this.#send(scope, resourceId, "dismiss", {});
  }

  async #send(scope: string, resourceId: string, action: string, body: unknown): Promise<ActionOutcome> {
    let response: Awaited<ReturnType<HttpFetch>>;
    try {
      response = await this.#fetch(
        `${this.#base}/scopes/${encodeURIComponent(scope)}/resources/${encodeURIComponent(resourceId)}/${action}`,
        { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) },
      );
    } catch {
      return { kind: "uncertain" };
    }
    if (response.status >= 200 && response.status < 300) return { kind: "done" };
    return refusal(response.status, await response.json().catch(() => undefined));
  }
}
