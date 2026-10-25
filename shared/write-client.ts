// The write side of the client of revision 4 (docs/protocol-r4-design.md §8, §9). Operations are
// written to a target (`global`, `catalog`, `sessions/<id>`) and carry an id the client chose and
// the `op` its document shows as `expect`; every one is answered `202` when admitted and its
// result arrives in the document. Actions take no `expect`. Writes and actions never change a
// replica: the stream alone does (C1). A write without a definite answer is never resent by
// itself (C3): the client returns what it needs to repeat it or to find out what became of it
// from the target's log (§9.3).

import type { OpArgs, OpRequest, ProtocolError } from "./protocol";
import {
  isProtocolError,
  isRecord,
  refusal,
  ResourceClient,
  targetOf,
  type ActionOutcome,
  type HttpFetch,
} from "./protocol-client";
import { isOpRecord, outcome, type Logged, type OpRecord, type Outcome, type TargetState } from "./outcome";
import { newOperationId, nextStep, resolveWrite, type Resolution, type Write } from "./write-resolution";
import type { Next } from "./outcome";

export type { ActionOutcome } from "./protocol-client";
export type { Write } from "./write-resolution";

/** The same outcome table, with the retained record when its full contents are available. */
export type OperationOutcome = Outcome<OpRecord | undefined>;

/** The documents a writer reads `expect` from, and what it held when it wrote. */
export interface ReplicaSource {
  get(scope: string):
    | {
        /** The instance of the bridge. */
        readonly epoch?: string;
        /** The path version held. */
        readonly rev?: number;
        readonly heldSince?: number | null;
        readonly ops?: readonly Logged[];
        readonly doc: { readonly live?: unknown };
      }
    | undefined;
}

/** The `op` a target shows: only what a write reads. */
interface ShownOperation {
  readonly id: string;
  readonly status: string;
}

export type WriteOutcome =
  /** `202`: admitted, nothing more; the document shows what follows. */
  | { readonly kind: "accepted"; readonly op: string; readonly write?: Write }
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
    const held = this.#replicas.get(scope);
    const write: Write = {
      id: this.#newId(),
      expect: op.id,
      scope,
      ...(held?.epoch === undefined ? {} : { instance: held.epoch }),
      ...(held?.rev === undefined ? {} : { at: held.rev }),
    };
    return this.#send(write.scope, { ...args, id: write.id, expect: write.expect }, write);
  }

  /** Sends `write` again, with the same id and `expect`: harmless if it was admitted (§6.6). */
  repeat(write: Write, args: StartingWrite): Promise<WriteOutcome> {
    return this.#send(write.scope, { ...args, id: write.id, expect: write.expect }, write);
  }

  /** What became of `write`, from the log of the target it was written to as the client holds it (§9.3). */
  resolve(write: Write): Resolution {
    return resolveWrite(write, this.#targetOf(write));
  }

  /** Admission and the retained result, read only from the target this write was issued to. */
  outcome(write: Write): OperationOutcome | undefined {
    const target = this.#targetOf(write);
    if (target === undefined) return undefined;
    const result = outcome(target, {
      ...write, instance: write.instance ?? target.instance, at: write.at ?? 0,
    });
    if (result.outcome !== "admitted") return result;
    return { outcome: "admitted", record: target.ops.find((record): record is OpRecord =>
      record.id === write.id && isOpRecord(record)) };
  }

  /** What the client may do next with `write` (§9.3), from the same log. */
  nextStep(write: Write): Next {
    return nextStep(write, this.#targetOf(write));
  }

  #targetOf(write: Write): TargetState | undefined {
    const held = this.#replicas.get(write.scope);
    const op = shownOperation(this.#replicas, write.scope);
    if (held === undefined || op === undefined) return undefined;
    return {
      instance: held.epoch ?? write.instance ?? "",
      op: op.id,
      heldSince: held.heldSince ?? null,
      ops: held.ops ?? [],
    };
  }

  /** Where the bridge says what became of `write` (§9.3): `GET` this and give the answer to `resolveFrom`. */
  resolutionUrl(write: Write): string {
    const query = new URLSearchParams({ expect: write.expect });
    if (write.instance !== undefined) query.set("instance", write.instance);
    if (write.at !== undefined) query.set("at", String(write.at));
    return `/api/v2/targets/${targetOf(write.scope)}/ops/${encodeURIComponent(write.id)}?${query}`;
  }

  /** What became of `write`, from the bridge's answer to `resolutionUrl` (§9.3). */
  resolveFrom(_write: Write, answer: unknown): Resolution {
    if (!isRecord(answer)) return "unknown";
    if (answer.outcome === "admitted") return "admitted";
    return answer.outcome === "superseded" ? "not_admitted" : "unknown";
  }

  /** The HTTP outcome, retaining its terminal status and error summary as well as admission. */
  outcomeFrom(write: Write, answer: unknown): OperationOutcome | undefined {
    if (!isRecord(answer)) return undefined;
    if (answer.outcome === "admitted") {
      return isOpRecord(answer.record) && answer.record.id === write.id
        ? { outcome: "admitted", record: answer.record }
        : undefined;
    }
    if (answer.outcome === "open" || answer.outcome === "superseded") return { outcome: answer.outcome };
    if (answer.outcome === "unknown" &&
      (answer.reason === "expired" || answer.reason === "released" || answer.reason === "restarted")) {
      return { outcome: "unknown", reason: answer.reason };
    }
    return undefined;
  }

  /** Asks the running operation to end; a cancel has no id of its own. */
  cancel(scope: string): Promise<WriteOutcome> {
    const op = shownOperation(this.#replicas, scope);
    if (op === undefined) return Promise.resolve(blocked(`no aligned document of ${scope}`));
    return this.#send(scope, { expect: op.id, kind: "cancel", args: {} }, undefined);
  }

  async #send(scope: string, body: OpRequest, write: Write | undefined): Promise<WriteOutcome> {
    let response: Awaited<ReturnType<HttpFetch>>;
    try {
      response = await this.#fetch(`/api/v2/targets/${targetOf(scope)}/ops`, {
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
        ? { kind: "accepted", op: answer.op, ...(write === undefined ? {} : { write }) }
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
  return { id: live.op.id, status: typeof live.op.status === "string" ? live.op.status : "running" };
}

function blocked(reason: string): WriteOutcome {
  return { kind: "blocked", reason };
}

export type InteractionAnswer =
  | { readonly outcome: unknown }
  | { readonly response: unknown };

/**
 * Sends actions (§8). Controls and close are held until the Agent answers: an error it
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
  return `/api/v2/targets/${targetOf(scope)}`;
}
