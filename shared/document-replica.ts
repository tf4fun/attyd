// The replica engine of the client protocol (docs/protocol.md §5). A replica holds
// one scope's document as the bridge published it and applies a stream's events to
// it one at a time. It holds no authoritative state (C1) and runs outside React.
//
//   snapshot    installs a document (or, within an epoch, the entries that changed);
//               installed only if the digest of what was installed matches (§5.5)
//   ops         the next rev, applied to a copy and adopted whole
//   checkpoint  the digest at the current rev, compared with the replica's own
//
// Whatever cannot be applied or verified is a resubscription: with the version
// held when the replica is still trusted, without it (`full`) when its content
// disagrees with the bridge's, so that the bridge sends the whole document.

import { entryHash, scopeDigest, sessionDigest, type Hash } from "./digest";
import { PatchError, applyOps } from "./patch";
import type { End, Op } from "./protocol";

/** A document without its `epoch` and `rev`: a session's has `history?`, `entries?`, `prompts` and `live`; the others `live`. */
export type DocumentBody = Readonly<Record<string, unknown>>;

export interface DocumentReplica {
  readonly scope: string;
  readonly epoch: string;
  readonly rev: number;
  readonly doc: DocumentBody;
  /** The rev whose digest this replica last matched. */
  readonly verifiedRev: number;
}

/** What a snapshot says about `entries` (§5.2); absent while the history is unknown. */
export interface SnapshotEntries {
  /** `update`: the entries held in the same epoch stay, those listed are replaced. */
  readonly base: "replace" | "update";
  readonly length: number;
  readonly changed: ReadonlyArray<readonly [index: number, entry: unknown]>;
}

export interface Snapshot {
  readonly epoch: string;
  readonly rev: number;
  readonly digest: string;
  /** The document but its entries, `epoch` and `rev`. */
  readonly doc: DocumentBody;
  readonly entries?: SnapshotEntries;
}

export type StreamEvent =
  | { readonly scope: string; readonly snapshot: Snapshot }
  | { readonly scope: string; readonly rev: number; readonly ops: readonly Op[] }
  | { readonly scope: string; readonly rev: number; readonly digest: string }
  | { readonly scope: string; readonly end: End };

export type Applied =
  | { readonly kind: "applied"; readonly replica: DocumentReplica }
  | { readonly kind: "ended"; readonly end: End }
  | { readonly kind: "resubscribe"; readonly reason: string; readonly full: boolean };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Freezes `value` and everything below it; frozen parts are not visited again. */
function freeze<T>(value: T): T {
  if (typeof value !== "object" || value === null || Object.isFrozen(value)) return value;
  Object.freeze(value);
  for (const member of Object.values(value)) freeze(member);
  return value;
}

/**
 * Hashes of entries by the entry object itself. A replica's entries are frozen,
 * so a hash can only be reused for the very content it was computed from; an
 * entry replaced without an event is a new object and is hashed again.
 */
const hashes = new WeakMap<object, Hash>();

function hashOf(entry: unknown): Hash {
  if (typeof entry !== "object" || entry === null) return entryHash(entry);
  let hash = hashes.get(entry);
  if (hash === undefined) {
    hash = entryHash(entry);
    hashes.set(entry, hash);
  }
  return hash;
}

/** The digest of `doc` at `epoch` and `rev`, computed from the content held. */
export function digestOf(scope: string, epoch: string, rev: number, doc: DocumentBody): string {
  if (!scope.startsWith("session/")) return scopeDigest({ epoch, rev, ...doc });
  const { entries, ...rest } = doc;
  return sessionDigest({ epoch, rev, ...rest }, Array.isArray(entries) ? entries.map(hashOf) : null);
}

function resubscribe(reason: string, full: boolean): Applied {
  return { kind: "resubscribe", reason, full };
}

/** The entries a snapshot leaves the replica with, or why they cannot be installed. */
function installedEntries(
  snapshot: Snapshot,
  held: DocumentReplica | undefined,
): { readonly entries: readonly unknown[] } | { readonly problem: string } {
  const spec = snapshot.entries;
  if (spec === undefined) return { entries: [] };
  const { base, length, changed } = spec;
  if (!Number.isInteger(length) || length < 0 || !Array.isArray(changed)) {
    return { problem: "malformed entries in a snapshot" };
  }
  let entries: unknown[];
  if (base === "update") {
    const previous = held?.epoch === snapshot.epoch ? held.doc.entries : undefined;
    if (!Array.isArray(previous)) return { problem: "an update of entries not held in this epoch" };
    if (length < previous.length) return { problem: "entries shrank within an epoch" };
    entries = previous.slice(0, length);
    entries.length = length;
  } else if (base === "replace") {
    entries = new Array<unknown>(length);
  } else {
    return { problem: `unknown entries base ${JSON.stringify(base)}` };
  }
  const listed = new Set<number>();
  for (const pair of changed) {
    const index: unknown = Array.isArray(pair) ? pair[0] : undefined;
    if (typeof index !== "number" || !Number.isInteger(index) || index < 0 || index >= length || listed.has(index)) {
      return { problem: "a changed entry outside the entries or listed twice" };
    }
    listed.add(index);
    entries[index] = pair[1];
  }
  for (let index = 0; index < length; index += 1) {
    if (!(index in entries)) return { problem: `entry ${index} is neither held nor listed` };
  }
  return { entries };
}

function install(scope: string, held: DocumentReplica | undefined, snapshot: Snapshot): Applied {
  if (typeof snapshot.epoch !== "string" || !Number.isInteger(snapshot.rev) || !isRecord(snapshot.doc)) {
    return resubscribe("a malformed snapshot", true);
  }
  const installed = installedEntries(snapshot, held);
  if ("problem" in installed) return resubscribe(installed.problem, true);
  const doc = freeze({
    ...snapshot.doc,
    ...(snapshot.entries === undefined ? {} : { entries: installed.entries }),
  });
  // Confirmed only if what was installed is what the bridge digested.
  if (digestOf(scope, snapshot.epoch, snapshot.rev, doc) !== snapshot.digest) {
    return resubscribe("the snapshot does not match its digest", true);
  }
  return {
    kind: "applied",
    replica: { scope, epoch: snapshot.epoch, rev: snapshot.rev, doc, verifiedRev: snapshot.rev },
  };
}

/**
 * Applies one event of `scope` to `held` (§5.2, §5.4). Nothing in `held` is
 * modified: an event that cannot be applied leaves the replica as it was.
 */
export function applyEvent(held: DocumentReplica | undefined, event: StreamEvent): Applied {
  if ("end" in event) return { kind: "ended", end: event.end };
  if ("snapshot" in event) return install(event.scope, held, event.snapshot);
  if (held === undefined) return resubscribe("an event before any snapshot", false);

  if ("ops" in event) {
    if (event.rev !== held.rev + 1) {
      return resubscribe(`ops for rev ${event.rev} after rev ${held.rev}`, false);
    }
    let doc: unknown;
    try {
      doc = applyOps(held.doc, event.ops);
    } catch (error) {
      if (error instanceof PatchError) return resubscribe(error.message, false);
      throw error;
    }
    if (!isRecord(doc)) return resubscribe("ops left no document", false);
    return {
      kind: "applied",
      replica: { ...held, rev: event.rev, doc: freeze(doc) },
    };
  }

  if (event.rev !== held.rev) {
    return resubscribe(`a checkpoint for rev ${event.rev} at rev ${held.rev}`, false);
  }
  if (digestOf(held.scope, held.epoch, held.rev, held.doc) !== event.digest) {
    return resubscribe("the replica does not match the checkpoint digest", true);
  }
  return { kind: "applied", replica: { ...held, verifiedRev: held.rev } };
}

/** A scope to follow, and the working directory to load an unheld session with. */
export interface Follow {
  readonly scope: string;
  readonly cwd?: string;
}

export type Received = "applied" | "ended" | "ignored" | "resubscribe";

/**
 * The replicas of the scopes one client follows, and the stream that feeds them.
 * A new stream replaces the previous one; events of a replaced stream change nothing.
 */
export class DocumentReplicaSet {
  readonly #replicas = new Map<string, DocumentReplica>();
  readonly #failures = new Map<string, number>();
  #stream = 0;
  /** The current stream; `undefined` once it may no longer be applied. */
  #current: number | undefined;
  /** The scopes the current stream follows and has not ended. */
  #following = new Set<string>();
  #lastResubscribe: { readonly reason: string; readonly full: boolean } | undefined;

  /** Opens a new stream for `follow`; returns its ID and its URL, built from the versions held now. */
  open(follow: readonly Follow[]): { readonly stream: number; readonly url: string } {
    const params = new URLSearchParams();
    for (const { scope } of follow) params.append("scope", scope);
    for (const { scope } of follow) {
      const replica = this.#replicas.get(scope);
      if (replica !== undefined) params.append("have", `${scope}:${replica.epoch}:${replica.rev}`);
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
  receive(stream: number, event: StreamEvent): Received {
    if (stream !== this.#current || !this.#following.has(event.scope)) return "ignored";
    const result = applyEvent(this.#replicas.get(event.scope), event);
    switch (result.kind) {
      case "applied":
        this.#replicas.set(event.scope, result.replica);
        if ("snapshot" in event) this.#failures.delete(event.scope);
        return "applied";
      case "ended":
        this.#replicas.delete(event.scope);
        this.#failures.delete(event.scope);
        this.#following.delete(event.scope);
        return "ended";
      case "resubscribe":
        // The stream is closed; only a new one changes the replicas. A replica
        // that disagrees with the bridge is dropped, so the new stream asks for
        // the whole document.
        if (result.full) {
          this.#replicas.delete(event.scope);
          this.#failures.set(event.scope, (this.#failures.get(event.scope) ?? 0) + 1);
        }
        this.#lastResubscribe = { reason: result.reason, full: result.full };
        this.#current = undefined;
        return "resubscribe";
    }
  }

  /** The replica of `scope`, if one is held. */
  get(scope: string): DocumentReplica | undefined {
    return this.#replicas.get(scope);
  }

  /** How many times in a row the content of `scope` failed verification; 0 once a snapshot installed. */
  consecutiveFailures(scope: string): number {
    return this.#failures.get(scope) ?? 0;
  }

  /** Why the last stream had to be replaced, and whether the whole document is asked for. */
  get lastResubscribe(): { readonly reason: string; readonly full: boolean } | undefined {
    return this.#lastResubscribe;
  }
}
