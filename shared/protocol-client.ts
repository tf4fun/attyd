// What the HTTP clients of revision 3 share (docs/protocol.md §6, §7): the transport, the
// write queue and the resource client. Writes and actions go over HTTP and never change a
// replica: the stream alone does (C1). A write sent without a definite answer is never
// resent automatically (C3); queued input that was never sent, or was refused with a
// definite answer, is submitted once the document shows the operation ended (C2).

import type { ErrorCode, OpArgs, Operation, ProtocolError } from "./protocol";

// ---------------------------------------------------------------------------
// Transport

/** The part of `fetch` the client uses; a rejection is a write without a definite answer. */
export type HttpFetch = (
  url: string,
  init: { method: "POST"; headers: Record<string, string>; body: string },
) => Promise<{ status: number; json(): Promise<unknown> }>;

/** What the queue reads of a replica: its `live`, from the document the feed holds. */
export interface QueueSource {
  get(scope: string): { readonly live?: unknown } | undefined;
}

/** The `op` of the aligned replica of `scope`. */
function shownOp(replicas: QueueSource, scope: string): Operation | undefined {
  const live = replicas.get(scope)?.live;
  if (!isRecord(live) || !isRecord(live.op) || typeof live.op.id !== "string") return undefined;
  return live.op as unknown as Operation;
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
  write(scope: string, write: StartingWrite): Promise<QueueOutcome>;
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

  #answered(scope: string, id: number, outcome: QueueOutcome): void {
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
