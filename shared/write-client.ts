// The write side of the client of revision 3 (docs/protocol.md §6, §7). Operations
// carry an id the client chose and the `op` its document shows as `expect`; every one
// is answered `202` when admitted and its result arrives in the document. Actions take
// no `expect`. Writes and actions never change a replica: the stream alone does (C1).
// A write without a definite answer is never resent by itself (C3): the client returns
// what it needs to repeat it or to resolve it from the document (§6.6).

import type { OpArgs, ProtocolError } from "./protocol";
import {
  isProtocolError,
  isRecord,
  refusal,
  ResourceClient,
  type ActionOutcome,
  type HttpFetch,
} from "./protocol-client";
import { newOperationId, resolveWrite, type Resolution, type ShownOperation, type Write } from "./write-resolution";

export type { ActionOutcome } from "./protocol-client";
export type { Write } from "./write-resolution";

/** The documents a writer reads `expect` from. */
export interface ReplicaSource {
  get(scope: string): { readonly doc: { readonly live?: unknown } } | undefined;
}

export type WriteOutcome =
  /** `202`: admitted, nothing more; the document shows what follows. */
  | { readonly kind: "accepted"; readonly op: string }
  /** A definite refusal: the write changed nothing (§6.4). */
  | { readonly kind: "refused"; readonly error: ProtocolError }
  /** No definite answer: the write may have taken effect (C3). Repeat or resolve it with `write`. */
  | { readonly kind: "uncertain"; readonly write?: Write }
  /** Not sent: the document does not allow it as it stands. */
  | { readonly kind: "blocked"; readonly reason: string };

/** A write that starts an operation; `cancel` and `answer` are not operations of their own. */
export type StartingWrite = Exclude<OpArgs, { kind: "cancel" | "answer" }>;

export class OpsClient {
  readonly #fetch: HttpFetch;
  readonly #replicas: ReplicaSource;
  readonly #newId: () => string;

  constructor(fetch: HttpFetch, replicas: ReplicaSource, newId: () => string = newOperationId) {
    this.#fetch = fetch;
    this.#replicas = replicas;
    this.#newId = newId;
  }

  /** Starts an operation with a new id, against the `op` the document of `scope` shows. */
  write(scope: string, args: StartingWrite): Promise<WriteOutcome> {
    const op = shownOperation(this.#replicas, scope);
    if (op === undefined) return Promise.resolve(blocked(`no aligned document of ${scope}`));
    const write: Write = { id: this.#newId(), expect: op.id, scope };
    return this.#send(write.scope, { id: write.id, expect: write.expect, kind: args.kind, args: args.args }, write);
  }

  /** Sends `write` again, with the same id and `expect`: harmless if it was admitted (§6.6). */
  repeat(write: Write, args: StartingWrite): Promise<WriteOutcome> {
    return this.#send(write.scope, { id: write.id, expect: write.expect, kind: args.kind, args: args.args }, write);
  }

  /** Whether `write` was admitted, from the document of the scope it was written to (§6.6). */
  resolve(write: Write): Resolution {
    return resolveWrite(write, shownOperation(this.#replicas, write.scope));
  }

  /** Asks the running operation to end; a cancel has no id of its own. */
  cancel(scope: string): Promise<WriteOutcome> {
    const op = shownOperation(this.#replicas, scope);
    if (op === undefined) return Promise.resolve(blocked(`no aligned document of ${scope}`));
    return this.#send(scope, { expect: op.id, kind: "cancel", args: {} }, undefined);
  }

  async #send(scope: string, body: unknown, write: Write | undefined): Promise<WriteOutcome> {
    let response: Awaited<ReturnType<HttpFetch>>;
    try {
      response = await this.#fetch(`/api/v2/scopes/${encodeURIComponent(scope)}/ops`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
    } catch {
      return { kind: "uncertain", write };
    }
    const answer = await response.json().catch(() => undefined);
    if (response.status === 202) {
      return isRecord(answer) && typeof answer.op === "string"
        ? { kind: "accepted", op: answer.op }
        : { kind: "uncertain", write };
    }
    const refused = refusal(response.status, answer);
    return refused.kind === "refused" ? refused : { kind: "uncertain", write };
  }
}

/** The `op` the aligned document of `scope` shows. */
function shownOperation(replicas: ReplicaSource, scope: string): ShownOperation | undefined {
  const live = replicas.get(scope)?.doc.live;
  if (!isRecord(live) || !isRecord(live.op) || typeof live.op.id !== "string") return undefined;
  const { id, scope: written, after, status } = live.op;
  return {
    id,
    scope: typeof written === "string" ? written : scope,
    after: typeof after === "string" ? after : null,
    status: typeof status === "string" ? status : "running",
  };
}

function blocked(reason: string): WriteOutcome {
  return { kind: "blocked", reason };
}

export type InteractionAnswer =
  | { readonly outcome: unknown }
  | { readonly response: unknown };

/**
 * Sends actions (§7). Controls and close are held until the Agent answers: an error it
 * answered with is a refusal here, and a lost answer is uncertain, never resent.
 */
export class ActionsClient {
  readonly #fetch: HttpFetch;
  readonly #resources: ResourceClient;

  constructor(fetch: HttpFetch) {
    this.#fetch = fetch;
    this.#resources = new ResourceClient(fetch, "/api/v2");
  }

  /** Answers one pending interaction, at most once. */
  answer(scope: string, interactionId: string, answer: InteractionAnswer): Promise<ActionOutcome> {
    return this.#post(`${base(scope)}/resources/${encodeURIComponent(interactionId)}/answer`, answer, false);
  }

  setMode(scope: string, modeId: string): Promise<ActionOutcome> {
    return this.#post(`${base(scope)}/actions/set_mode`, { modeId }, true);
  }

  setConfig(scope: string, configId: string, value: string | boolean): Promise<ActionOutcome> {
    return this.#post(`${base(scope)}/actions/set_config`, { configId, value }, true);
  }

  /** Starts the Agent connection when none runs (`global`, §7); done once it is started. */
  restart(): Promise<ActionOutcome> {
    return this.#post(`${base("global")}/actions/restart`, {}, false);
  }

  close(scope: string): Promise<ActionOutcome> {
    return this.#post(`${base(scope)}/actions/close`, {}, true);
  }

  input(scope: string, terminalId: string, data: string): Promise<ActionOutcome> {
    return this.#resources.input(scope, terminalId, data);
  }

  resize(scope: string, terminalId: string, cols: number, rows: number): Promise<ActionOutcome> {
    return this.#resources.resize(scope, terminalId, cols, rows);
  }

  dismiss(scope: string, resourceId: string): Promise<ActionOutcome> {
    return this.#resources.dismiss(scope, resourceId);
  }

  async #post(url: string, body: unknown, held: boolean): Promise<ActionOutcome> {
    let response: Awaited<ReturnType<HttpFetch>>;
    try {
      response = await this.#fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
    } catch {
      return { kind: "uncertain" };
    }
    const answer = await response.json().catch(() => undefined);
    if (response.status === 204 || (held && response.status === 200 && isRecord(answer) && !("error" in answer))) {
      return { kind: "done" };
    }
    if (held && response.status === 200) {
      return isRecord(answer) && isProtocolError(answer.error)
        ? { kind: "refused", error: answer.error }
        : { kind: "uncertain" };
    }
    return refusal(response.status, answer);
  }
}

function base(scope: string): string {
  return `/api/v2/scopes/${encodeURIComponent(scope)}`;
}
