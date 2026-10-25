// The documents of the scopes `global`, `catalog` and `session/<id>` as the page reads them,
// assembled from the paths of the revision 4 tree it follows (docs/protocol-r4-design.md §4.2):
// the operation is its head and its detail put together again, a session's entries, prompts and
// turns are the lists the bridge publishes. The page reads these documents and nothing of the
// stream; what makes them is `TreeFeedClient`.

import type { End, ProtocolError } from "./protocol";
import type { FeedStatus, TreeFeedClient } from "./tree-feed";
import type { TreeEnd } from "./tree-replica";
import { isOpRecord, type OpRecord } from "./outcome";

/** A scope to follow, and the working directory to load an unheld session with. */
export interface Follow {
  readonly scope: string;
  readonly cwd?: string;
}

export interface ScopeReplica {
  readonly scope: string;
  /** The instance of the bridge. */
  readonly epoch: string;
  /** The path version the replica holds. */
  readonly rev: number;
  /** A session's `{ history?, entries?, prompts, turns?, live }`; the others' `{ live }`. */
  readonly doc: Readonly<Record<string, unknown>>;
  /** The path version whose hash the replica last matched. */
  readonly verifiedRev: number;
  /** The log of the operations that occupied the scope (design §9.3). */
  readonly ops: readonly OpRecord[];
  /** The revision at which the bridge started holding the session; null for `global` and `catalog`. */
  readonly heldSince: number | null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function escape(key: string): string {
  return key.replace(/~/g, "~0").replace(/\//g, "~1");
}

/** The tree path of a scope. */
export function pathOf(scope: string): string {
  if (scope === "global" || scope === "catalog") return `/${scope}`;
  if (scope.startsWith("session/")) return `/sessions/${escape(scope.slice("session/".length))}`;
  throw new TypeError(`no scope ${JSON.stringify(scope)}`);
}

/** The scope of a tree path, if it is the root of one. */
export function scopeOf(path: string): string | undefined {
  if (path === "/global" || path === "/catalog") return path.slice(1);
  const match = /^\/sessions\/([^/]+)$/.exec(path);
  return match === null ? undefined : `session/${match[1].replace(/~1/g, "/").replace(/~0/g, "~")}`;
}

/** The error an end or an operation carries when only its summary is known. */
function errorOf(summary: unknown): ProtocolError | undefined {
  if (!isRecord(summary) || typeof summary.code !== "string") return undefined;
  const { code, agentCode } = summary;
  return {
    code: code as ProtocolError["code"],
    message: "",
    ...(typeof agentCode === "number" ? { data: { code: agentCode } } : {}),
  } as ProtocolError;
}

/** Operations by their head and, within it, their detail: the very objects the replica holds. */
const operations = new WeakMap<object, WeakMap<object, unknown>>();
const NO_DETAIL = {};

/** The operation as the page reads it: the head with the input, result and whole error. */
export function operationOf(head: unknown, detail: unknown): unknown {
  if (!isRecord(head)) return undefined;
  const d = isRecord(detail) ? detail : NO_DETAIL;
  let byDetail = operations.get(head);
  if (byDetail === undefined) {
    byDetail = new WeakMap();
    operations.set(head, byDetail);
  }
  let op = byDetail.get(d);
  if (op === undefined) {
    const error = isRecord(d) && d.error !== undefined ? d.error : errorOf(head.error);
    op = Object.freeze({
      ...head,
      input: (d as Record<string, unknown>).input ?? null,
      ...((d as Record<string, unknown>).result === undefined ? {} : { result: (d as Record<string, unknown>).result }),
      ...(error === undefined ? {} : { error }),
    });
    byDetail.set(d, op);
  }
  return op;
}

/** Documents by the value they were made from, so that an unchanged scope is the same document. */
const documents = new WeakMap<object, Readonly<Record<string, unknown>>>();

function documentOf(scope: string, value: unknown): Readonly<Record<string, unknown>> | undefined {
  if (!isRecord(value)) return undefined;
  const cached = documents.get(value);
  if (cached !== undefined) return cached;
  const { op, opDetail, ops: _ops, ...rest } = value;
  const merged = operationOf(op, opDetail);
  if (merged === undefined) return undefined;
  let doc: Readonly<Record<string, unknown>>;
  if (scope.startsWith("session/")) {
    const { history, entries, prompts, turns, historyRev, heldSince, ...live } = rest;
    doc = Object.freeze({
      ...(history === undefined ? {} : { history }),
      ...(entries === undefined ? {} : { entries }),
      prompts: prompts ?? [],
      ...(turns === undefined ? {} : { turns }),
      ...(historyRev === undefined ? {} : { historyRev }),
      ...(heldSince === undefined ? {} : { heldSince }),
      live: Object.freeze({ ...live, op: merged }),
    });
  } else {
    doc = Object.freeze({ live: Object.freeze({ ...rest, op: merged }) });
  }
  documents.set(value, doc);
  return doc;
}

export class ScopeFeed {
  readonly #feed: TreeFeedClient;

  constructor(feed: TreeFeedClient) {
    this.#feed = feed;
  }

  follow(follow: readonly Follow[]): void {
    this.#feed.follow(follow.map(({ scope, cwd }) => ({ path: pathOf(scope), ...(cwd === undefined ? {} : { cwd }) })));
  }

  get(scope: string): ScopeReplica | undefined {
    const replica = this.#feed.get(pathOf(scope));
    if (replica === undefined) return undefined;
    const doc = documentOf(scope, replica.value);
    if (doc === undefined) return undefined;
    const value = replica.value as Record<string, unknown>;
    const log = Array.isArray(value.ops) ? value.ops : [];
    return {
      scope,
      epoch: this.#feed.instance ?? "",
      rev: replica.version,
      doc,
      verifiedRev: replica.verified,
      ops: log.filter(isOpRecord),
      heldSince: typeof value.heldSince === "number" ? value.heldSince : null,
    };
  }

  /** Why `scope` ended, if it did since it was followed. The whole error of a failed load is the bridge's `loadFailures`. */
  endOf(scope: string): End | undefined {
    const end = this.#feed.endOf(pathOf(scope));
    // A path that is not there yet (a session still to be loaded) has not ended.
    if (end === undefined || end.reason === "absent") return undefined;
    return { reason: reasonOf(end), error: this.#errorOf(scope, end) };
  }

  #errorOf(scope: string, end: TreeEnd): ProtocolError | undefined {
    const summary = errorOf(end.error);
    if (!scope.startsWith("session/") || end.reason !== "load_failed") return summary;
    const global = this.#feed.get("/global")?.value;
    const failures = isRecord(global) ? global.loadFailures : undefined;
    const whole = isRecord(failures) ? failures[scope.slice("session/".length)] : undefined;
    return isRecord(whole) && typeof whole.code === "string" ? (whole as unknown as ProtocolError) : summary;
  }

  /** Calls `listener` with a scope each time its document changes or ends. */
  subscribe(listener: (scope: string) => void): () => void {
    return this.#feed.subscribe((path) => {
      const scope = scopeOf(path);
      if (scope === undefined) return;
      // Nothing to say of a path that is not there yet, or no longer there for no reason given.
      if (this.#feed.get(path) === undefined && this.#feed.endOf(path)?.reason === "absent") return;
      listener(scope);
    });
  }

  onStatus(listener: (status: FeedStatus) => void): () => void {
    return this.#feed.onStatus(listener);
  }

  close(): void {
    this.#feed.close();
  }
}

/** The reasons revision 3's ends had; the tree adds two for fixed views, which a page does not follow. */
function reasonOf(end: TreeEnd): End["reason"] {
  return end.reason === "deleted" || end.reason === "closed" ? end.reason : "load_failed";
}
