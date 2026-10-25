// The replica engine of the client of revision 4 (docs/protocol-r4-design.md §6). A replica holds,
// per followed path, the subtree as the bridge published it and its path version; it applies a
// stream's events to them and verifies what it holds by the Merkle hash of design §5. It holds no
// authoritative state (C1) and runs outside React.
//
//   snapshot   sets the content of the path and `v_p = at`; installed only if the hash of what
//              was installed is the bridge's
//   ops        applied if `from == v_p`, then `v_p = rev`; a repeat is ignored, any other mismatch
//              is a gap
//   check      `at == v_p`: compared; `at > v_p`: a change was missed; `at < v_p`: ignored
//
// Whatever cannot be applied or verified is a resubscription: with the versions held when the
// replica is still trusted, and without the path's (`full`) when its content disagrees with the
// bridge's, so that the bridge sends the whole subtree.

import { wire, type Hash } from "./digest";
import { PatchError, applyOps } from "./patch";
import type { Op } from "./protocol";
import { shapeAt } from "./tree-shape";
import { cachedValueHash, listHash, mapHash, objectHash } from "./tree-hash";

// ---------------------------------------------------------------------------
// The events (design §6.2, §6.3)

export type SNode =
  | { readonly v: unknown }
  | { readonly o: Record<string, unknown>; readonly k: readonly string[]; readonly c: Readonly<Record<string, SNode>> }
  | { readonly m: readonly string[]; readonly c: Readonly<Record<string, SNode>> }
  | { readonly l: number; readonly c: Readonly<Record<string, SNode>> }
  | { readonly same: true };

export interface Snapshot {
  readonly base: number | null;
  readonly at: number;
  readonly hash: string;
  readonly node: SNode;
}

export interface TreeEnd {
  readonly reason: "deleted" | "closed" | "load_failed" | "absent" | "replaced";
  readonly error?: { readonly code: string; readonly agentCode?: number };
}

export type Change =
  | { readonly path: string; readonly snapshot: Snapshot }
  | { readonly path: string; readonly from: number; readonly ops: readonly Op[] }
  | { readonly path: string; readonly end: TreeEnd };

export interface Check {
  readonly path: string;
  readonly at: number;
  readonly hash: string;
}

export interface TreeEvent {
  readonly instance?: string;
  readonly rev: number;
  readonly changes?: readonly Change[];
  readonly check?: readonly Check[];
}

// ---------------------------------------------------------------------------
// Values

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

/** A JSON Pointer token for `key`. */
function escape(key: string): string {
  return key.replace(/~/g, "~0").replace(/\//g, "~1");
}

function unescape(token: string): string {
  return token.replace(/~1/g, "/").replace(/~0/g, "~");
}

/** The tokens of a path (unescaped). */
export function tokensOf(path: string): string[] {
  return path === "" ? [] : path.slice(1).split("/").map(unescape);
}

class Mismatch extends Error {
  override name = "Mismatch";
}

// ---------------------------------------------------------------------------
// Hashes (design §5)

/** Hashes of containers by the (frozen) container itself; values are cached by `cachedValueHash`. */
const containerHashes = new WeakMap<object, Hash>();

/** The hash of `value`, the subtree at `tokens`, by the shape of the tree. */
export function hashAt(tokens: readonly string[], value: unknown): Hash {
  const shape = shapeAt(tokens);
  if (shape === undefined) throw new Mismatch(`no node at /${tokens.map(escape).join("/")}`);
  if (shape.kind === "value") return cachedValueHash(shape.rule, value);
  if (typeof value !== "object" || value === null) throw new Mismatch("a container is not an object or an array");
  const cached = containerHashes.get(value);
  if (cached !== undefined) return cached;
  let hash: Hash;
  if (shape.kind === "list") {
    if (!Array.isArray(value)) throw new Mismatch("a list is not an array");
    hash = listHash(value.map((item, index) => hashAt([...tokens, String(index)], item)));
  } else {
    if (!isRecord(value)) throw new Mismatch("an object or a map is not an object");
    if (shape.kind === "map") {
      hash = mapHash(new Map(Object.entries(value).map(([key, item]) => [key, hashAt([...tokens, key], item)])));
    } else {
      const own: Record<string, unknown> = {};
      const children = new Map<string, Hash>();
      for (const [key, item] of Object.entries(value)) {
        if (shape.children.has(key)) children.set(key, hashAt([...tokens, key], item));
        else own[key] = item;
      }
      hash = objectHash(own, children);
    }
  }
  if (Object.isFrozen(value)) containerHashes.set(value, hash);
  return hash;
}

// ---------------------------------------------------------------------------
// Snapshots (design §6.3)

/** What a client holding `held` has after installing `node`: it keeps every child the structure names and the snapshot omits, drops the rest, and replaces the listed. */
function installNode(held: unknown, node: SNode): unknown {
  const member = (container: unknown, key: string): unknown => {
    if (Array.isArray(container)) return /^(0|[1-9][0-9]*)$/.test(key) ? container[Number(key)] : undefined;
    return isRecord(container) && Object.hasOwn(container, key) ? container[key] : undefined;
  };
  const child = (key: string, changed: Readonly<Record<string, SNode>>): unknown => {
    if (Object.hasOwn(changed, key)) return installNode(member(held, key), changed[key]);
    const kept = member(held, key);
    if (kept === undefined) throw new Mismatch(`the snapshot keeps ${JSON.stringify(key)}, which is not held`);
    return kept;
  };
  if ("same" in node) {
    if (held === undefined) throw new Mismatch("an unchanged path that is not held");
    return held;
  }
  if ("v" in node) return node.v;
  if ("o" in node) {
    if (!isRecord(node.o) || !Array.isArray(node.k)) throw new Mismatch("a malformed object in a snapshot");
    const object: Record<string, unknown> = { ...node.o };
    for (const key of node.k) object[key] = child(key, node.c);
    return object;
  }
  if ("m" in node) {
    if (!Array.isArray(node.m)) throw new Mismatch("a malformed map in a snapshot");
    return Object.fromEntries(node.m.map((key) => [key, child(key, node.c)]));
  }
  if ("l" in node) {
    if (!Number.isInteger(node.l) || node.l < 0) throw new Mismatch("a malformed list in a snapshot");
    return Array.from({ length: node.l }, (_, index) => child(String(index), node.c));
  }
  throw new Mismatch("an unknown node in a snapshot");
}

// ---------------------------------------------------------------------------
// One path

export interface PathReplica {
  readonly path: string;
  readonly value: unknown;
  /** `v_p`. */
  readonly version: number;
  /** The path version whose hash this replica last matched. */
  readonly verified: number;
}

export type Applied =
  | { readonly kind: "applied"; readonly replica: PathReplica }
  | { readonly kind: "unchanged" }
  | { readonly kind: "ended"; readonly end: TreeEnd }
  | { readonly kind: "resubscribe"; readonly reason: string; readonly full: boolean };

function resubscribe(reason: string, full: boolean): Applied {
  return { kind: "resubscribe", reason, full };
}

function install(path: string, held: PathReplica | undefined, snapshot: Snapshot): Applied {
  if (!Number.isInteger(snapshot.at) || typeof snapshot.hash !== "string" || typeof snapshot.node !== "object") {
    return resubscribe("a malformed snapshot", true);
  }
  // A repeat: the path is held at this version or a later one.
  if (held !== undefined && snapshot.at <= held.version) return { kind: "unchanged" };
  if (snapshot.base !== null && (held === undefined || snapshot.base !== held.version)) {
    return resubscribe(`a snapshot built on ${snapshot.base} of a path held at ${held?.version ?? "nothing"}`, held === undefined);
  }
  let value: unknown;
  try {
    if (snapshot.base === null && !("v" in snapshot.node)) {
      return resubscribe("a snapshot from nothing that is not whole", true);
    }
    value = freeze(installNode(snapshot.base === null ? undefined : held?.value, snapshot.node));
    // Confirmed only if what was installed is what the bridge hashed.
    if (wire(hashAt(tokensOf(path), value)) !== snapshot.hash) {
      return resubscribe("the snapshot does not match its hash", true);
    }
  } catch (error) {
    if (error instanceof Mismatch) return resubscribe(error.message, true);
    throw error;
  }
  return { kind: "applied", replica: { path, value, version: snapshot.at, verified: snapshot.at } };
}

function applyChange(
  path: string,
  held: PathReplica | undefined,
  change: Change,
  rev: number,
): Applied {
  if ("end" in change) return { kind: "ended", end: change.end };
  if ("snapshot" in change) return install(path, held, change.snapshot);
  if (held === undefined) return resubscribe("ops before any snapshot", false);
  // A repeat of a commit the replica has.
  if (rev <= held.version) return { kind: "unchanged" };
  if (change.from !== held.version) return resubscribe(`ops from ${change.from} for a path held at ${held.version}`, false);
  let value: unknown;
  try {
    value = freeze(applyOps(held.value, change.ops));
  } catch (error) {
    if (error instanceof PatchError) return resubscribe(error.message, false);
    throw error;
  }
  return { kind: "applied", replica: { ...held, value, version: rev } };
}

function applyCheck(held: PathReplica | undefined, check: Check): Applied {
  if (held === undefined) return { kind: "unchanged" };
  if (check.at < held.version) return { kind: "unchanged" };
  if (check.at > held.version) return resubscribe(`a check at ${check.at} of a path held at ${held.version}: a change was missed`, false);
  try {
    if (wire(hashAt(tokensOf(held.path), held.value)) !== check.hash) {
      return resubscribe("the replica does not match the checkpoint hash", true);
    }
  } catch (error) {
    if (error instanceof Mismatch) return resubscribe(error.message, true);
    throw error;
  }
  return { kind: "applied", replica: { ...held, verified: held.version } };
}

// ---------------------------------------------------------------------------
// The set

/** A path to follow, and the working directory to load its session with (design §7.2). */
export interface Follow {
  readonly path: string;
  readonly cwd?: string;
  /** Hold the text positions under the path as windows of at least this many bytes (docs/protocol.md §5.4). */
  readonly window?: number;
}

export type Received = "applied" | "ignored" | "resubscribe";

/** How many replicas of paths no longer followed are kept, so following one again asks only for what changed. */
export const KEPT_UNFOLLOWED = 4;

/**
 * The replicas of the paths one client follows, and the stream that feeds them. A new stream
 * replaces the previous one; events of a replaced stream change nothing.
 */
export class TreeReplicaSet {
  readonly #replicas = new Map<string, PathReplica>();
  readonly #failures = new Map<string, number>();
  #instance: string | undefined;
  #stream = 0;
  #current: number | undefined;
  #following = new Set<string>();
  #unfollowed: string[] = [];
  #lastResubscribe: { readonly reason: string; readonly full: boolean; readonly path?: string } | undefined;
  /** The paths whose last event ended them, and why. */
  readonly #ends = new Map<string, TreeEnd>();
  readonly #base: string;

  constructor(base = "/api/v2/stream") {
    this.#base = base;
  }

  get instance(): string | undefined {
    return this.#instance;
  }

  /** Opens a new stream for `follow`; returns its ID and URL, built from the versions held now. */
  open(follow: readonly Follow[]): { readonly stream: number; readonly url: string } {
    const params = new URLSearchParams();
    const held = follow.filter(({ path }) => this.#replicas.has(path));
    if (this.#instance !== undefined && held.length > 0) params.append("instance", this.#instance);
    for (const { path } of follow) params.append("follow", path);
    if (this.#instance !== undefined) {
      for (const { path } of held) params.append("have", `${path}:${this.#replicas.get(path)!.version}`);
    }
    for (const { path, window } of follow) {
      if (window !== undefined) params.append("window", `${path}:${window}`);
    }
    for (const { path, cwd } of follow) {
      const session = sessionOf(path);
      if (cwd !== undefined && session !== undefined) params.append("cwd", `${encodeURIComponent(session)}:${cwd}`);
    }
    this.#stream += 1;
    this.#current = this.#stream;
    const following = new Set(follow.map(({ path }) => path));
    const left = [...this.#replicas.keys()].filter((path) => !following.has(path) && !this.#unfollowed.includes(path));
    this.#unfollowed = [...this.#unfollowed.filter((path) => !following.has(path)), ...left];
    while (this.#unfollowed.length > KEPT_UNFOLLOWED) {
      const dropped = this.#unfollowed.shift()!;
      this.#replicas.delete(dropped);
      this.#failures.delete(dropped);
    }
    this.#following = following;
    for (const path of following) this.#ends.delete(path);
    return { stream: this.#stream, url: `${this.#base}?${params}` };
  }

  /**
   * Applies one event received on `stream`. The changes of one event are applied together: if any
   * cannot be, none is. `changed` lists the paths whose replica changed or ended.
   */
  receive(stream: number, event: TreeEvent): { readonly kind: Received; readonly changed: readonly string[] } {
    if (stream !== this.#current) return { kind: "ignored", changed: [] };
    if (typeof event.rev !== "number") return this.#fail("an event without a rev", true, undefined);
    const dropped = event.instance !== undefined && event.instance !== this.#instance;
    const next = new Map(dropped ? [] : this.#replicas);
    const ended = new Map<string, TreeEnd>();
    const changed: string[] = [];
    for (const change of event.changes ?? []) {
      if (!this.#following.has(change.path)) continue;
      const result = applyChange(change.path, next.get(change.path), change, event.rev);
      if (result.kind === "resubscribe") return this.#fail(result.reason, result.full, change.path, dropped, event.instance);
      if (result.kind === "applied") {
        next.set(change.path, result.replica);
        changed.push(change.path);
      } else if (result.kind === "ended") {
        next.delete(change.path);
        ended.set(change.path, result.end);
        changed.push(change.path);
      } else if ("snapshot" in change && next.has(change.path)) {
        // The stream is aligned with a path whose replica was already current: nothing to install,
        // but the page waits to be told that the path is followed again.
        changed.push(change.path);
      }
    }
    for (const check of event.check ?? []) {
      if (!this.#following.has(check.path)) continue;
      const result = applyCheck(next.get(check.path), check);
      if (result.kind === "resubscribe") return this.#fail(result.reason, result.full, check.path, dropped, event.instance);
      if (result.kind === "applied") {
        next.set(check.path, result.replica);
      }
    }
    if (dropped) {
      this.#replicas.clear();
      this.#failures.clear();
      this.#unfollowed = [];
      this.#instance = event.instance;
    }
    for (const [path, replica] of next) this.#replicas.set(path, replica);
    for (const path of [...this.#replicas.keys()]) if (!next.has(path)) this.#replicas.delete(path);
    for (const [path, end] of ended) this.#ends.set(path, end);
    for (const change of event.changes ?? []) {
      if ("snapshot" in change && this.#following.has(change.path)) this.#failures.delete(change.path);
    }
    for (const path of changed) if (!ended.has(path)) this.#ends.delete(path);
    return { kind: changed.length > 0 ? "applied" : "ignored", changed };
  }

  #fail(
    reason: string,
    full: boolean,
    path: string | undefined,
    dropped = false,
    instance?: string,
  ): { readonly kind: Received; readonly changed: readonly string[] } {
    // The stream is closed; only a new one changes the replicas. A replica that disagrees with the
    // bridge is dropped, so the new stream asks for the whole subtree. A new instance is adopted
    // even though the event could not be applied: nothing held of the old one is of any use.
    if (dropped && instance !== undefined) {
      this.#replicas.clear();
      this.#failures.clear();
      this.#unfollowed = [];
      this.#instance = instance;
    }
    if (full && path !== undefined) {
      this.#replicas.delete(path);
      this.#failures.set(path, (this.#failures.get(path) ?? 0) + 1);
    }
    this.#lastResubscribe = { reason, full, path };
    this.#current = undefined;
    return { kind: "resubscribe", changed: [] };
  }

  get(path: string): PathReplica | undefined {
    return this.#replicas.get(path);
  }

  /** Why `path` ended, if it did since it was followed. */
  endOf(path: string): TreeEnd | undefined {
    return this.#ends.get(path);
  }

  /** How many times in a row the content of `path` failed verification; 0 once a snapshot installed. */
  consecutiveFailures(path: string): number {
    return this.#failures.get(path) ?? 0;
  }

  get lastResubscribe(): { readonly reason: string; readonly full: boolean; readonly path?: string } | undefined {
    return this.#lastResubscribe;
  }
}

/** The session a path belongs to: `/sessions/<id>/…` and `/views/<id>/…` hold it. */
export function sessionOf(path: string): string | undefined {
  const [top, id] = tokensOf(path);
  return (top === "sessions" || top === "views") && id !== undefined ? id : undefined;
}
