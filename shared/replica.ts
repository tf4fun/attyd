// The generic replica engine of protocol v2 (docs/protocol.md §5, migration
// step 7). A replica holds one scope's document, `log` and `live`, as the
// bridge publishes it, and applies the stream's events to it one at a time.
// It holds no authoritative state (C1) and runs outside React.
//
// Test phase: the shapes below are the engine's interface; the behavior is
// not implemented yet.

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

function notImplemented(): never {
  throw new Error("replica engine: not implemented");
}

/** A replica of `scope` that holds `log`, unconfirmed, before any alignment. */
export function createReplica(_scope: string, _log: readonly LogEntry[] = []): Replica {
  return notImplemented();
}

/** The head of the log `replica` holds. */
export function headOf(_replica: Replica): Head {
  return notImplemented();
}

/**
 * Applies `ops` to `live` in order and returns the new value; `live` is not
 * modified. Throws `PatchError` when an op does not apply.
 */
export function applyOps(_live: unknown, _ops: readonly Op[]): unknown {
  return notImplemented();
}

/**
 * Applies one event of `replica.scope` (§5.2). Both parts of an event are
 * applied together: when either does not apply, nothing changes and the
 * client subscribes again with the head it holds.
 */
export function applyEvent(_replica: Replica, _event: Event): Applied {
  return notImplemented();
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
  /** Opens a new stream for `follow`; returns its ID and its URL, built from the heads held now. */
  open(_follow: readonly Follow[]): { readonly stream: number; readonly url: string } {
    return notImplemented();
  }

  /** Applies one event received on `stream`. */
  receive(_stream: number, _event: Event): Received {
    return notImplemented();
  }

  /** The replica of `scope`, if one is held. */
  get(_scope: string): Replica | undefined {
    return notImplemented();
  }
}
