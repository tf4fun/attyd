// A fake bridge for the page's tests: it holds the documents of the scopes the way revision 3 wrote
// them (a session's `{ history, prompts, live, entries }`, the `live` of `global` and `catalog`) and
// publishes each change as the revision 4 event a stream would carry: the whole path again as a
// snapshot, at a new path version, with the hash the bridge would state. The page reads the tree;
// the cases keep writing documents.
import { wire } from "../../shared/digest";
import { applyOps } from "../../shared/patch";
import type { Entry, PromptState } from "../../shared/entries";
import type { Op } from "../../shared/protocol";
import { turnHeads } from "../../web/src/dev/turn-heads";
import { hashAt, tokensOf, type TreeEnd, type TreeEvent } from "../../shared/tree-replica";

type Doc = Record<string, unknown>;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The head of an operation and its detail (design §4.4). */
function split(op: Record<string, unknown>): { head: Doc; detail: Doc } {
  const { input, result, error, ...head } = op;
  const summary = isRecord(error)
    ? { code: error.code, ...(isRecord(error.data) && typeof error.data.code === "number" ? { agentCode: error.data.code } : {}) }
    : undefined;
  return {
    head: { ...head, ...(summary === undefined ? {} : { error: summary }) },
    detail: { input: input ?? null, ...(result === undefined ? {} : { result }), ...(error === undefined ? {} : { error }) },
  };
}

/** The tree value of a scope's document. */
export function treeValue(scope: string, doc: Doc): Doc {
  const live = (isRecord(doc.live) ? doc.live : {}) as Doc;
  const { op, ...rest } = live;
  const { head, detail } = split((op ?? {}) as Doc);
  // A case supplies its operation log explicitly; this fixture does not maintain one.
  const common = { op: head, opDetail: detail, ops: doc.ops ?? [] };
  if (scope.startsWith("session/")) {
    const historyRev = typeof doc.historyRev === "number" ? doc.historyRev : 1;
    return {
      ...rest,
      ...common,
      historyRev,
      heldSince: doc.heldSince ?? 1,
      ...(doc.history === undefined ? {} : { history: doc.history }),
      ...(doc.entries === undefined ? {} : { entries: doc.entries }),
      prompts: doc.prompts ?? [],
      // The partition the bridge publishes with the entries.
      ...(doc.entries === undefined
        ? {}
        : { turns: turnHeads(doc.entries as Entry[], (doc.prompts ?? []) as PromptState[], historyRev) }),
    };
  }
  return { ...rest, ...common, ...(scope === "global" ? { loadFailures: doc.loadFailures ?? {} } : {}) };
}

export function pathOf(scope: string): string {
  return scope.startsWith("session/") ? `/sessions/${scope.slice("session/".length)}` : `/${scope}`;
}

export class TreeBridge {
  readonly instance: string;
  #rev = 1;
  readonly #docs = new Map<string, Doc>();
  /** The failed loads global keeps, by session ID. */
  readonly failures: Record<string, unknown> = {};

  constructor(instance = "E1") {
    this.instance = instance;
  }

  /** The document of `scope` as the cases last wrote it. */
  doc(scope: string): Doc | undefined {
    return this.#docs.get(scope);
  }

  #publish(scope: string, doc: Doc): TreeEvent {
    this.#docs.set(scope, doc);
    this.#rev += 1;
    const value = treeValue(scope, scope === "global" ? { ...doc, loadFailures: this.failures } : doc);
    const path = pathOf(scope);
    return {
      instance: this.instance,
      rev: this.#rev,
      changes: [{ path, snapshot: { base: null, at: this.#rev, hash: wire(hashAt(tokensOf(path), value)), node: { v: value } } }],
    };
  }

  /** The scope's document is `body` (`entries` apart), from nothing. */
  snapshot(scope: string, body: Doc, entries?: unknown[]): TreeEvent {
    return this.#publish(scope, { ...body, ...(entries === undefined ? {} : { entries }) });
  }

  /** The ops of revision 3's documents (`/live/…`, `/entries/…`, `/prompts/…`) applied to the scope's document. */
  ops(scope: string, ops: readonly Op[]): TreeEvent {
    const doc = this.#docs.get(scope);
    if (doc === undefined) throw new Error(`no document of ${scope}`);
    return this.#publish(scope, applyOps(doc, ops) as Doc);
  }

  /** A failed load of `sessionId`: global keeps its whole error, and the session's path ends with its summary. */
  loadFailed(sessionId: string, error: Record<string, unknown>): TreeEvent[] {
    this.failures[sessionId] = error;
    const summary: TreeEnd["error"] = {
      code: String(error.code),
      ...(isRecord(error.data) && typeof error.data.code === "number" ? { agentCode: error.data.code } : {}),
    };
    const events: TreeEvent[] = [];
    const global = this.#docs.get("global");
    if (global !== undefined) events.push(this.#publish("global", global));
    events.push({
      rev: this.#rev,
      changes: [{ path: pathOf(`session/${sessionId}`), end: { reason: "load_failed", error: summary } }],
    });
    return events;
  }

  end(scope: string, end: TreeEnd): TreeEvent {
    this.#docs.delete(scope);
    this.#rev += 1;
    return { rev: this.#rev, changes: [{ path: pathOf(scope), end }] };
  }
}
