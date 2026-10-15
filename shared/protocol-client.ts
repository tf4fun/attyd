// The HTTP and stream client of protocol v2 (docs/protocol.md §5–§7,
// migration step 7). The stream feeds the replicas; writes and resource
// actions go over HTTP and never change a replica: the stream alone does
// (C1). A write sent without a definite answer is never resent
// automatically (C3); queued input that was never sent, or was refused with
// a definite answer, is submitted once the replica shows the operation
// ended (C2).
//
// Test phase: the shapes below are the client's interface; the behavior is
// not implemented yet.

import type { Answer, OpArgs, OpOutcome, ProtocolError } from "./protocol";
import type { Follow, Replica } from "./replica";

function notImplemented(): never {
  throw new Error("protocol client: not implemented");
}

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
  constructor(_openStream: OpenStream, _schedule?: Schedule) {}

  follow(_follow: readonly Follow[]): void {
    notImplemented();
  }

  get(_scope: string): Replica | undefined {
    return notImplemented();
  }

  /** Calls `listener` with a scope each time its replica changes or ends. */
  subscribe(_listener: (scope: string) => void): () => void {
    return notImplemented();
  }

  close(): void {
    notImplemented();
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

/**
 * Sends operation writes against the `op` the replica shows. It waits for
 * the HTTP answer only, with no short timeout, and never for the stream.
 */
export class OpsClient {
  constructor(_fetch: HttpFetch, _replicas: ReplicaSource) {}

  /** Sends `write` with `expect` set to the replica's `op.id`. */
  write(_scope: string, _write: OpArgs): Promise<WriteOutcome> {
    return notImplemented();
  }

  /**
   * Answers the running operation's pending interactions, all of them at
   * once; it is blocked, unsent, unless `answers` covers exactly those the
   * replica shows.
   */
  answer(_scope: string, _answers: Readonly<Record<string, Answer>>): Promise<WriteOutcome> {
    return notImplemented();
  }
}

// ---------------------------------------------------------------------------
// Queued writes (C2, C3)

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
  constructor(_ops: OpsClient, _replicas: ReplicaSource) {}

  enqueue(_scope: string, _write: StartingWrite): number {
    return notImplemented();
  }

  items(_scope: string): readonly QueuedWrite[] {
    return notImplemented();
  }

  /** Called when the replica of `scope` changes; sends the next write if it may. */
  update(_scope: string): void {
    notImplemented();
  }

  /** The user sends `id` again, as new input. */
  resubmit(_scope: string, _id: number): void {
    notImplemented();
  }

  /** The user takes `id` back. */
  remove(_scope: string, _id: number): void {
    notImplemented();
  }
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
  constructor(_fetch: HttpFetch) {}

  input(_scope: string, _terminalId: string, _data: string): Promise<ActionOutcome> {
    return notImplemented();
  }

  resize(_scope: string, _terminalId: string, _cols: number, _rows: number): Promise<ActionOutcome> {
    return notImplemented();
  }

  dismiss(_scope: string, _resourceId: string): Promise<ActionOutcome> {
    return notImplemented();
  }
}
